//! Watch together on the session threads: group requests and their retries,
//! the server's clock, and the group's updates.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use jellyfin::{GroupRequest, GroupUpdate, HttpError, local_now_ms};

use crate::receiver::ReceiverEvent;
use crate::server::session::{Creds, GroupState, SessionEvent, Sink, locked, log, osd, sleep};

/// `/GetUtcTime` samples per clock batch.
pub(super) const CLOCK_SAMPLES: usize = 5;

/// Time between clock batches while in a group.
pub(super) const CLOCK_PERIOD: Duration = Duration::from_mins(1);

/// Attempts in all for a request the group's wait depends on, when the server
/// answers each with an error.
pub(super) const GROUP_WAIT_ATTEMPTS: u32 = 3;

/// Between those attempts.
pub(super) const GROUP_RETRY_PAUSE: Duration = Duration::from_millis(500);

/// Checks of the group, 250 ms apart, between attempts the network lost: about 2 s.
pub(super) const GROUP_NETWORK_POLLS: u32 = 8;

/// How long a report the network keeps losing is retried: past the 60 s of
/// silence after which the socket is reconnected, and that hands over to the rejoin.
pub(super) const GROUP_WAIT_LIMIT: Duration = Duration::from_secs(70);

/// `When` (server time) minus the server's clock now.
pub(super) fn command_delay_ms(when_ms: i64, local_now_ms: i64, offset_ms: i64) -> i64 {
    when_ms - (local_now_ms + offset_ms)
}

/// Measures the clock when woken (at connect and `GroupJoined`), and every
/// `CLOCK_PERIOD` while in a group. Ends when the supervisor drops the wake sender.
pub(super) fn keep_clock(
    woken: &Receiver<()>,
    sink: &Sink,
    stop: &AtomicBool,
    creds: &Mutex<Option<Creds>>,
    group: &Mutex<GroupState>,
) {
    loop {
        match woken.recv_timeout(CLOCK_PERIOD) {
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) if !locked(group).in_group => continue,
            _ => {}
        }
        let Some(c) = locked(creds).clone() else {
            continue;
        };
        let samples: Vec<_> = (0..CLOCK_SAMPLES)
            .take_while(|_| !stop.load(Ordering::Relaxed))
            .map(|_| c.client.clock_probe())
            .collect();
        let Some(&(offset_ms, rtt)) = samples.iter().flatten().min_by_key(|(_, rtt)| *rtt) else {
            if let Some(Err(e)) = samples.last() {
                log(sink, format!("clock sync failed: {e}"));
            }
            continue;
        };
        let in_group = {
            let mut state = locked(group);
            state.offset_ms = offset_ms;
            state.in_group
        };
        // The server doubles this for its start delay, so it is one-way.
        if in_group && let Err(e) = c.client.ping((rtt / 2).max(0)) {
            log(sink, format!("group ping failed: {e}"));
        }
    }
}

/// Tracks membership and passes one `SyncPlay` message to the reducer.
pub(super) fn on_group_update(
    update: GroupUpdate,
    sink: &Sink,
    group: &Mutex<GroupState>,
    clock_wake: &Sender<()>,
) {
    if matches!(update, GroupUpdate::Left | GroupUpdate::Gone { .. }) {
        let mut state = locked(group);
        state.joined = None;
        state.in_group = false;
    }
    let event = match update {
        GroupUpdate::Joined { group_id } => {
            {
                let mut state = locked(group);
                state.joined = Some(group_id.clone());
                state.in_group = true;
            }
            let _ = clock_wake.send(());
            ReceiverEvent::GroupJoined { group_id }
        }
        GroupUpdate::Left => ReceiverEvent::GroupLeft,
        GroupUpdate::Gone { message } => ReceiverEvent::GroupGone {
            message: message.to_owned(),
        },
        GroupUpdate::Queue(queue) => ReceiverEvent::GroupQueueChanged(queue),
        GroupUpdate::Command {
            mut command,
            when_ms,
        } => {
            let offset_ms = locked(group).offset_ms;
            command.delay_ms = command_delay_ms(when_ms, local_now_ms(), offset_ms);
            ReceiverEvent::GroupCommandReceived(command)
        }
        GroupUpdate::Ignored => return,
    };
    sink(SessionEvent::Receiver(event));
}

pub(super) fn post_group_request(
    request: &GroupRequest,
    current: Option<&Creds>,
    group: &Mutex<GroupState>,
) -> Result<(), HttpError> {
    let c = current.ok_or_else(|| HttpError::transport("not connected"))?;
    // Stamped here, not at submit: this queue can be behind.
    let server_now_ms = local_now_ms() + locked(group).offset_ms;
    c.client.syncplay(request, server_now_ms)
}

pub(super) fn send_group_request(
    request: &GroupRequest,
    current: Option<&Creds>,
    sink: &Sink,
    stop: &AtomicBool,
    group: &Mutex<GroupState>,
) {
    if *request == GroupRequest::Leave {
        locked(group).joined = None; // even if the POST fails: never rejoin
    }
    if matches!(
        request,
        GroupRequest::Ready { .. } | GroupRequest::Buffering { .. }
    ) {
        send_group_wait_report(request, current, sink, stop, group, GROUP_WAIT_LIMIT);
        return;
    }
    let Err(e) = post_group_request(request, current, group) else {
        return;
    };
    log(sink, format!("group request failed: {e}"));
    if matches!(
        request,
        GroupRequest::New { .. } | GroupRequest::Join { .. }
    ) {
        osd(
            sink,
            "mpvfin: couldn't reach Jellyfin to join that group",
            5,
        );
    }
}

/// The server has no timeout: if one of these is lost, the group waits forever.
/// A server that refuses it won't change its mind, but a network that lost it
/// may come back, and a Leave would be lost the same way, so that is waited out.
pub(super) fn send_group_wait_report(
    request: &GroupRequest,
    current: Option<&Creds>,
    sink: &Sink,
    stop: &AtomicBool,
    group: &Mutex<GroupState>,
    limit: Duration,
) {
    let started = Instant::now();
    let mut refused = 0;
    let (e, give_up) = loop {
        let Err(e) = post_group_request(request, current, group) else {
            return;
        };
        if matches!(e, HttpError::Transport(_)) {
            refused = 0;
            if started.elapsed() >= limit {
                break (e, true);
            }
            // Out of the group the rejoin owns the resync: a Ready before its Join
            // draws NotInGroup, which reads as the group gone. Polled, because
            // a reconnect can drop and rejoin inside one long pause.
            let in_group = (0..GROUP_NETWORK_POLLS)
                .all(|_| sleep(stop, Duration::from_millis(250)) && locked(group).in_group);
            if !in_group {
                break (e, false);
            }
        } else {
            refused += 1;
            if refused == GROUP_WAIT_ATTEMPTS {
                break (e, true);
            }
            if !sleep(stop, GROUP_RETRY_PAUSE) {
                break (e, false);
            }
        }
    };
    log(sink, format!("group request failed: {e}"));
    if give_up && !stop.load(Ordering::Relaxed) {
        give_up_on_group(current, sink, group);
    }
}

/// Out of the group, so it can go on without us; the viewer hears first, as
/// the Leave can stall as long as the requests that just failed.
pub(super) fn give_up_on_group(current: Option<&Creds>, sink: &Sink, group: &Mutex<GroupState>) {
    {
        let mut state = locked(group);
        state.joined = None;
        state.in_group = false;
    }
    sink(SessionEvent::Receiver(ReceiverEvent::GroupGone {
        message: "mpvfin: lost contact with the group, so you've left it".to_owned(),
    }));
    if let Err(e) = post_group_request(&GroupRequest::Leave, current, group) {
        log(sink, format!("leaving the group failed: {e}"));
    }
}

/// Always answered, so the runtime's in-flight guard can't stick.
pub(super) fn list_groups(current: Option<Creds>, sink: &Sink) {
    let list = current
        .ok_or_else(|| "not connected".to_owned())
        .and_then(|c| c.client.list_groups().map_err(|e| e.to_string()));
    match list {
        Ok(groups) => sink(SessionEvent::GroupList(groups)),
        Err(e) => {
            log(sink, format!("listing groups failed: {e}"));
            osd(sink, "mpvfin: couldn't reach Jellyfin to list groups", 5);
            sink(SessionEvent::GroupListFailed);
        }
    }
}
