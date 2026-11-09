//! Following a `SyncPlay` group: the rules of `docs/jellyfin-syncplay.md`,
//! sections 6 to 8, as reducer transitions.

use std::mem;
use std::time::Duration;

use jellyfin::{
    GroupCommand, GroupCommandKind, GroupEntry, GroupQueue, GroupRequest, QueueMode, QueueReason,
    QueueRequest, ZERO_ID,
};

use crate::receiver::{
    Generation, GroupLoadOptions, PlaybackState, Receiver, ReceiverAction, ReceiverEvent,
};

/// Closer than this to a commanded position needs no seek.
const POSITION_TOLERANCE: Duration = Duration::from_millis(100);

/// A settle this close to a seek of ours is that seek: the server's own
/// tolerance for a Ready, and mpv can read a frame or two past its target.
const SEEK_LANDING_TOLERANCE: Duration = Duration::from_millis(500);

/// What this session knows of its group.
// Each flag tracks a different server exchange; they are independent.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub(crate) struct Group {
    id: String,
    /// The queue in server order.
    entries: Vec<GroupEntry>,
    /// The queue's current entry, from the latest update.
    current_pid: Option<String>,
    /// Server time of the last applied queue update.
    last_update_ms: i64,
    /// The server waits for a Ready once the item settles.
    awaiting_ready: bool,
    /// The pause state the group wants.
    expected_paused: bool,
    /// Where a seek issued for the group is going, until it lands.
    own_seek: Option<Duration>,
    /// Where the group starts: a load's start, or a scheduled resume's position.
    start: Duration,
    /// The load replaces a playing item with no Unpause to come.
    resume_on_settle: bool,
    /// The server has heard that the current item ended.
    reached_end: bool,
    /// The server has heard that playback stalled.
    buffering: bool,
    /// The viewer's Pause and Unpause requests whose broadcasts are on their way.
    own_requests: u32,
    /// mpv hasn't yet confirmed the pause that holds a viewer's resume.
    hold_unconfirmed: bool,
    /// A `GroupLoad` has not reached mpv's hook yet.
    load_coming: bool,
    /// An entry the server waits on stopped; only mpv going idle makes that final.
    stopped_owing: bool,
}

impl Group {
    const fn new(id: String) -> Self {
        Self {
            id,
            entries: Vec::new(),
            current_pid: None,
            last_update_ms: 0,
            awaiting_ready: false,
            expected_paused: true,
            own_seek: None,
            start: Duration::ZERO,
            resume_on_settle: false,
            reached_end: false,
            buffering: false,
            own_requests: 0,
            hold_unconfirmed: false,
            load_coming: false,
            stopped_owing: false,
        }
    }
}

impl Receiver {
    /// The joined group's id.
    #[must_use]
    pub fn group_id(&self) -> Option<&str> {
        self.group.as_ref().map(|g| g.id.as_str())
    }

    pub(super) fn group_create(&mut self, name: String, actions: &mut Vec<ReceiverAction>) {
        self.creating_group = true;
        actions.push(ReceiverAction::Group(GroupRequest::New { name }));
    }

    pub(super) fn group_join(&mut self, group_id: String, actions: &mut Vec<ReceiverAction>) {
        self.creating_group = false;
        actions.push(ReceiverAction::Group(GroupRequest::Join { group_id }));
    }

    /// Out at once: a Leave that fails during an outage must not leave a ghost
    /// group, and commands racing the `GroupLeft` must not be applied.
    pub(super) fn group_leave(&mut self, actions: &mut Vec<ReceiverAction>) {
        actions.push(ReceiverAction::Group(GroupRequest::Leave));
        self.drop_group(actions);
    }

    pub(super) fn group_joined(&mut self, group_id: String, actions: &mut Vec<ReceiverAction>) {
        let rejoined = self.group.replace(Group::new(group_id)).is_some();
        if rejoined {
            self.stop_following(actions);
        }
        // A rejoin is not the group a failed create asked for; its queue stays.
        if mem::take(&mut self.creating_group)
            && !rejoined
            && let Some(position) = self.playing_position()
        {
            actions.push(ReceiverAction::GroupPublishQueue { position });
        }
    }

    pub(super) fn drop_group(&mut self, actions: &mut Vec<ReceiverAction>) {
        if self.group.take().is_some() {
            self.stop_following(actions);
        }
    }

    /// The message is shown even with no group: a refused join ends up here.
    pub(super) fn group_gone(&mut self, message: String, actions: &mut Vec<ReceiverAction>) {
        self.creating_group = false;
        self.drop_group(actions);
        actions.push(ReceiverAction::ShowMessage { text: message });
    }

    pub(super) fn group_queue_changed(
        &mut self,
        queue: GroupQueue,
        actions: &mut Vec<ReceiverAction>,
    ) {
        let loaded_pid = self.loaded_pid().map(str::to_owned);
        let Some(g) = self.group.as_mut() else {
            return;
        };
        // Equal is applied: the server's fix-up reuses the last timestamp.
        if queue.last_update_ms < g.last_update_ms {
            return;
        }
        g.last_update_ms = queue.last_update_ms;
        g.entries = queue.entries;
        let previous_pid = g.current_pid.take();
        let Some(index) = queue.playing_index.filter(|&i| i < g.entries.len()) else {
            g.awaiting_ready = false;
            g.resume_on_settle = false;
            self.stop_following(actions);
            return;
        };
        let pid = g.entries[index].playlist_item_id.clone();
        g.current_pid = Some(pid.clone());
        let entries = g.entries.clone();
        let item_changing = match queue.reason {
            QueueReason::NewPlaylist
            | QueueReason::SetCurrentItem
            | QueueReason::NextItem
            | QueueReason::PreviousItem => true,
            QueueReason::RemoveItems => previous_pid.as_ref() != Some(&pid),
            QueueReason::Other => false,
        };
        if !item_changing {
            actions.push(ReceiverAction::GroupSyncPlaylist { entries, index });
            return;
        }
        // Only removing the playing item changes it without a wait for Ready.
        let plays_on = queue.reason == QueueReason::RemoveItems && queue.is_playing;
        // mpv is about to leave whatever it has open for a load already asked for.
        let will_load = g.load_coming || loaded_pid.as_deref() != Some(pid.as_str());
        g.load_coming = will_load;
        g.awaiting_ready = !plays_on;
        g.resume_on_settle = plays_on && will_load;
        // Held for its Ready, or by the hook, whatever the group is doing.
        g.expected_paused = !plays_on || will_load;
        g.start = queue.start_position;
        g.reached_end = false;
        g.own_requests = 0;
        g.stopped_owing = false;
        // A drift anchor from the old item would drag the new one to its clock.
        self.stop_following(actions);
        if will_load {
            actions.push(ReceiverAction::GroupLoad { entries, index });
            return;
        }
        if plays_on {
            return;
        }
        // Still loading, or positioned by a pending start: the settle sends the Ready.
        let Some(position) = self.playing_position().filter(|_| self.position_settled) else {
            return;
        };
        self.control_pause(true, actions);
        if position.abs_diff(queue.start_position) > POSITION_TOLERANCE {
            self.group_seek(queue.start_position, actions);
        } else {
            self.send_ready(position, actions);
        }
    }

    pub(super) fn group_command(
        &mut self,
        command: &GroupCommand,
        actions: &mut Vec<ReceiverAction>,
    ) {
        let playing = self.playing_generation();
        let idle = self.current_generation().is_none();
        let player_position = self.playing_position();
        let Some(g) = self.group.as_mut() else {
            return;
        };
        let zero = command.playlist_item_id == ZERO_ID;
        if !zero && g.current_pid.as_deref() != Some(command.playlist_item_id.as_str()) {
            return;
        }
        // Until a coming load reaches mpv, the open file is not the group's.
        let generation = playing.filter(|_| g.current_pid == self.opened_pid);
        let position = command.position;
        // Our own request coming back: its position would undo a seek of the
        // viewer's, as uosc pauses before it seeks and resumes on release.
        let own_request = g.own_requests > 0;
        g.own_requests = match command.kind {
            GroupCommandKind::Pause | GroupCommandKind::Unpause => g.own_requests.saturating_sub(1),
            GroupCommandKind::Seek | GroupCommandKind::Stop => 0,
        };
        match command.kind {
            GroupCommandKind::Stop => {
                if !zero && !idle {
                    g.awaiting_ready = false;
                    g.buffering = false;
                    self.stop_player(actions);
                    self.stop_following(actions);
                }
            }
            GroupCommandKind::Pause => {
                g.expected_paused = true;
                g.resume_on_settle = false;
                self.stop_following(actions);
                if generation.is_some() {
                    self.control_pause(true, actions);
                    if !own_request {
                        self.seek_unless_near(position, actions);
                    }
                }
            }
            GroupCommandKind::Seek => {
                // Nothing is open and no load is coming, so no Ready could follow.
                if idle && !g.awaiting_ready && !g.resume_on_settle {
                    self.abandon_group(actions);
                    return;
                }
                g.expected_paused = true;
                g.resume_on_settle = false;
                g.awaiting_ready = true;
                g.start = position;
                self.stop_following(actions);
                if generation.is_some() {
                    self.control_pause(true, actions);
                    self.group_seek(position, actions);
                }
            }
            GroupCommandKind::Unpause => {
                let delay = Duration::from_millis(command.delay_ms.unsigned_abs());
                if command.delay_ms <= 0 {
                    actions.push(ReceiverAction::CancelResume);
                }
                // Not on the player yet: the Ready that follows gets this resent.
                let Some(generation) = generation else {
                    return;
                };
                g.expected_paused = false;
                let scheduled = command.delay_ms > 0;
                let target = if scheduled {
                    position
                } else {
                    position + delay
                };
                // An anchor at the broadcast's position would seek the viewer back.
                let start = if own_request {
                    player_position.unwrap_or(target)
                } else {
                    target
                };
                if scheduled {
                    // The anchor for later; the seek may not have landed by then.
                    g.start = start;
                }
                if !own_request {
                    self.seek_unless_near(target, actions);
                }
                if scheduled {
                    actions.push(ReceiverAction::ScheduleResume {
                        generation,
                        after: delay,
                    });
                } else {
                    Self::start_playing(generation, start, actions);
                }
            }
        }
    }

    pub(super) fn group_resume_due(
        &self,
        generation: Generation,
        actions: &mut Vec<ReceiverAction>,
    ) {
        if let Some(g) = &self.group
            && !g.expected_paused
            && self.playing_generation() == Some(generation)
        {
            Self::start_playing(generation, g.start, actions);
        }
    }

    /// mpv finished a load or seek on the current generation.
    pub(super) fn group_position_settled(
        &mut self,
        generation: Generation,
        position: Duration,
        actions: &mut Vec<ReceiverAction>,
    ) {
        let load_settling = !self.position_settled;
        // Not on the group's item: this says nothing of it.
        let Some(g) = self.group_on_item() else {
            return;
        };
        let landed = g
            .own_seek
            .take_if(|target| target.abs_diff(position) <= SEEK_LANDING_TOLERANCE)
            .is_some();
        // Away from a seek still on its way, the viewer moved mpv: that seek's
        // landing owes the Ready. A load's own landing is never the viewer's.
        let viewer_sought =
            !landed && !load_settling && (g.own_seek.is_some() || !g.awaiting_ready);
        if viewer_sought {
            actions.push(ReceiverAction::Group(GroupRequest::Seek { position }));
            // Until the broadcast, the group's clock would pull this seek back.
            self.stop_following(actions);
        } else if g.awaiting_ready {
            self.send_ready(position, actions);
        } else if mem::take(&mut g.resume_on_settle) {
            // The hook loads every group entry paused.
            g.expected_paused = false;
            Self::start_playing(generation, position, actions);
        }
    }

    /// The viewer pausing or resuming mpv is a request; mpv waits for the broadcast.
    pub(super) fn group_pause_changed(&mut self, paused: bool, actions: &mut Vec<ReceiverAction>) {
        let Some(g) = self.group_on_item() else {
            return;
        };
        // Can land after the broadcast has already said the group is playing.
        // shortcut: if mpv merges this pause with an immediate start, no event clears the flag
        // and the viewer's next pause is lost; only a start with no lead time can do that.
        if paused && mem::take(&mut g.hold_unconfirmed) {
            return;
        }
        // Once the end is reported the server is moving on: a Pause would stop it resuming.
        if paused == g.expected_paused || g.reached_end {
            return;
        }
        g.own_requests += 1;
        if paused {
            g.expected_paused = true;
            actions.push(ReceiverAction::Group(GroupRequest::Pause));
            self.stop_following(actions);
        } else {
            g.hold_unconfirmed = true;
            actions.push(ReceiverAction::Group(GroupRequest::Unpause));
            self.control_pause(true, actions);
        }
    }

    /// A remote transport control, as a request to the group.
    pub(super) fn group_control(&self, event: &ReceiverEvent, actions: &mut Vec<ReceiverAction>) {
        let Some(g) = &self.group else {
            return;
        };
        let current = g.current_pid.clone();
        let is_not = |end: Option<&GroupEntry>, pid: &String| {
            end.is_some_and(|e| e.playlist_item_id != *pid)
        };
        let request = match *event {
            ReceiverEvent::RemoteResume => Some(GroupRequest::Unpause),
            ReceiverEvent::RemoteTogglePause if g.expected_paused => Some(GroupRequest::Unpause),
            ReceiverEvent::RemotePause | ReceiverEvent::RemoteTogglePause => {
                Some(GroupRequest::Pause)
            }
            ReceiverEvent::RemoteSeek(position) => Some(GroupRequest::Seek { position }),
            ReceiverEvent::RemoteStop => Some(GroupRequest::Stop),
            // No entry to move to, in Waiting, would send the group to Idle.
            ReceiverEvent::RemoteNext => current
                .filter(|pid| is_not(g.entries.last(), pid))
                .map(|pid| GroupRequest::NextItem { pid }),
            ReceiverEvent::RemotePrevious => current
                .filter(|pid| is_not(g.entries.first(), pid))
                .map(|pid| GroupRequest::PreviousItem { pid }),
            _ => None,
        };
        actions.extend(request.map(ReceiverAction::Group));
    }

    pub(super) fn group_remote_play(request: QueueRequest, actions: &mut Vec<ReceiverAction>) {
        let QueueRequest {
            item_ids,
            mode,
            start_index,
            start_position: start,
            ..
        } = request;
        actions.push(ReceiverAction::Group(if mode == QueueMode::Replace {
            GroupRequest::SetNewQueue {
                index: start_index.min(item_ids.len().saturating_sub(1)),
                item_ids,
                start,
            }
        } else {
            GroupRequest::Queue {
                item_ids,
                next: mode == QueueMode::Next,
            }
        }));
    }

    /// mpv opened an entry, so anything pending was for the last one. An entry
    /// other than the group's current one means the viewer moved in the playlist.
    pub(super) fn group_opening(&mut self, actions: &mut Vec<ReceiverAction>) {
        let Some(g) = self.group.as_mut() else {
            return;
        };
        g.own_seek = None;
        g.reached_end = false;
        g.buffering = false;
        g.own_requests = 0;
        g.hold_unconfirmed = false;
        g.stopped_owing = false;
        let Some(pid) = self.opened_pid.clone() else {
            return;
        };
        // The hook holds a load paused.
        if g.current_pid.as_ref() == Some(&pid) {
            g.load_coming = false;
            g.expected_paused = true;
            return;
        }
        // A load overtaken by a newer one opens late; an unknown pid would cost
        // the group its current item.
        if g.load_coming || !g.entries.iter().any(|e| e.playlist_item_id == pid) {
            return;
        }
        g.current_pid = Some(pid.clone());
        g.awaiting_ready = true;
        g.expected_paused = true;
        g.resume_on_settle = false;
        // The server names the start in its update; until then, the old item's isn't it.
        g.start = Duration::ZERO;
        actions.push(ReceiverAction::Group(GroupRequest::SetPlaylistItem { pid }));
        self.stop_following(actions);
    }

    /// Once per item: the server has no end-of-file notion of its own.
    pub(super) fn group_reached_end(
        &mut self,
        generation: Generation,
        actions: &mut Vec<ReceiverAction>,
    ) {
        if self.playing_mut(generation).is_none() {
            return;
        }
        let Some(g) = self.group_on_item() else {
            return;
        };
        let Some(pid) = g.current_pid.clone() else {
            return;
        };
        if mem::replace(&mut g.reached_end, true) {
            return;
        }
        let later_entry = g.entries.last().is_some_and(|e| e.playlist_item_id != pid);
        actions.push(ReceiverAction::Group(if later_entry {
            GroupRequest::NextItem { pid }
        } else {
            GroupRequest::Stop
        }));
    }

    /// A dead link is this viewer's problem: `NextItem` would move everyone on.
    pub(super) fn group_stopped_early(
        &mut self,
        generation: Generation,
        actions: &mut Vec<ReceiverAction>,
    ) {
        if self.playing_mut(generation).is_some() && self.group_on_item().is_some() {
            self.group_leave(actions);
            // Or, with `keep-open` restored, mpv plays on through the group's queue alone.
            actions.push(ReceiverAction::StopPlayer { generation });
            actions.push(ReceiverAction::ShowMessage {
                text: "mpvfin: the stream stopped early, so you've left the group".to_owned(),
            });
        }
    }

    pub(super) fn group_buffering(
        &mut self,
        generation: Generation,
        buffering: bool,
        actions: &mut Vec<ReceiverAction>,
    ) {
        let Some((position, paused)) = self.playing_mut(generation).map(|s| (s.position, s.paused))
        else {
            return;
        };
        let Some(g) = self.group_on_item() else {
            return;
        };
        let Some(pid) = g.current_pid.clone() else {
            return;
        };
        if buffering {
            // Server-initiated waits already count everyone as buffering.
            if !g.buffering && !g.expected_paused && !g.awaiting_ready {
                g.buffering = true;
                actions.push(ReceiverAction::Group(GroupRequest::Buffering {
                    position,
                    pid,
                }));
                // The group freezes here, and the clock would run on ahead of it.
                self.stop_following(actions);
            }
        } else if mem::take(&mut g.buffering) && !g.awaiting_ready {
            // A settle owes the Ready instead when a seek overtook the stall.
            actions.push(ReceiverAction::Group(GroupRequest::Ready {
                position,
                is_playing: !paused,
                pid,
            }));
            // A Ready that says playing gets no Unpause to anchor on.
            if !paused && !g.expected_paused {
                actions.push(ReceiverAction::DriftAnchor {
                    generation,
                    position: Some(position),
                });
            }
        }
    }

    pub(super) fn group_drift_seek(
        &mut self,
        generation: Generation,
        position: Duration,
        actions: &mut Vec<ReceiverAction>,
    ) {
        if self.playing_mut(generation).is_some() {
            self.group_seek(position, actions);
        }
    }

    /// Unpause in Playing is resent to the caller with a fresh position, which
    /// re-anchors; not counted as ours, or that position would be ignored.
    pub(super) fn group_drift_lost(&self, actions: &mut Vec<ReceiverAction>) {
        if self.group.as_ref().is_some_and(|g| !g.expected_paused) {
            actions.push(ReceiverAction::Group(GroupRequest::Unpause));
        }
    }

    pub(super) fn group_opened_foreign(&mut self, actions: &mut Vec<ReceiverAction>) {
        if self.group.is_some() {
            self.group_leave(actions);
        }
    }

    /// The server has no timeout: an item it waits on that ended would hang
    /// the group. An end of the item being replaced says nothing of the wait.
    pub(super) fn group_load_ended(&mut self, actions: &mut Vec<ReceiverAction>) {
        if self.server_waits_on_open_item() {
            self.abandon_group(actions);
        }
    }

    /// A failed entry abandons even with no wait on it: mpv would auto-advance
    /// to the next entry, and that reads as the viewer picking it for everyone.
    pub(super) fn group_item_failed(&mut self, actions: &mut Vec<ReceiverAction>) {
        if self.group_on_item().is_some() {
            self.abandon_group(actions);
        }
    }

    /// mpv ends an entry with a stop before it opens the next one, so the
    /// decision waits for an opening, or for mpv going idle.
    pub(super) fn group_load_stopped(&mut self) {
        let waited_on = self.server_waits_on_open_item();
        if let Some(g) = self.group.as_mut() {
            g.stopped_owing = waited_on;
        }
    }

    pub(super) fn group_idle(&mut self, actions: &mut Vec<ReceiverAction>) {
        if self.group.as_ref().is_some_and(|g| g.stopped_owing) {
            self.abandon_group(actions);
        }
    }

    fn server_waits_on_open_item(&self) -> bool {
        self.group
            .as_ref()
            .is_some_and(|g| (g.awaiting_ready || g.buffering) && g.current_pid == self.opened_pid)
    }

    /// mpv's `stop` also clears the mirrored queue, which it would play on through alone.
    fn abandon_group(&mut self, actions: &mut Vec<ReceiverAction>) {
        self.group_leave(actions);
        if let Some(generation) = Generation::new(self.next_generation - 1) {
            actions.push(ReceiverAction::StopPlayer { generation });
        }
        actions.push(ReceiverAction::ShowMessage {
            text: "mpvfin: couldn't load that item, so you've left the group".to_owned(),
        });
    }

    pub(super) fn group_load_options(&self) -> Option<GroupLoadOptions> {
        let g = self.group.as_ref()?;
        (self.opened_pid.is_some() && g.current_pid == self.opened_pid)
            .then_some(GroupLoadOptions { start: g.start })
    }

    /// The group, while mpv has its current entry open.
    pub(super) fn group_on_item(&mut self) -> Option<&mut Group> {
        let opened = self.opened_pid.as_ref()?;
        self.group
            .as_mut()
            .filter(|g| g.current_pid.as_ref() == Some(opened))
    }

    /// The queue entry mpv has open, if any.
    fn loaded_pid(&self) -> Option<&str> {
        self.current_generation()?;
        self.opened_pid.as_deref()
    }

    fn playing_position(&self) -> Option<Duration> {
        match &self.playback {
            PlaybackState::Playing(snapshot) => Some(snapshot.position),
            _ => None,
        }
    }

    /// Stops correcting playback and forgets a scheduled start.
    fn stop_following(&self, actions: &mut Vec<ReceiverAction>) {
        actions.push(ReceiverAction::CancelResume);
        // The newest generation, even idle: an anchor outlives the item it was set on.
        if let Some(generation) = Generation::new(self.next_generation - 1) {
            actions.push(ReceiverAction::DriftAnchor {
                generation,
                position: None,
            });
        }
    }

    fn start_playing(
        generation: Generation,
        position: Duration,
        actions: &mut Vec<ReceiverAction>,
    ) {
        actions.push(ReceiverAction::SetPaused {
            generation,
            paused: false,
        });
        actions.push(ReceiverAction::DriftAnchor {
            generation,
            position: Some(position),
        });
    }

    fn seek_unless_near(&mut self, target: Duration, actions: &mut Vec<ReceiverAction>) {
        if self
            .playing_position()
            .is_some_and(|position| position.abs_diff(target) > POSITION_TOLERANCE)
        {
            self.group_seek(target, actions);
        }
    }

    fn group_seek(&mut self, position: Duration, actions: &mut Vec<ReceiverAction>) {
        if let (Some(generation), Some(g)) = (self.playing_generation(), self.group.as_mut()) {
            g.own_seek = Some(position);
            actions.push(ReceiverAction::Seek {
                generation,
                position,
            });
        }
    }

    /// Reports the loaded item ready; the zero id is never a real entry.
    fn send_ready(&mut self, position: Duration, actions: &mut Vec<ReceiverAction>) {
        let Some(g) = self.group.as_mut() else {
            return;
        };
        if let Some(pid) = g.current_pid.clone().filter(|pid| pid != ZERO_ID) {
            g.awaiting_ready = false;
            actions.push(ReceiverAction::Group(GroupRequest::Ready {
                position,
                is_playing: false,
                pid,
            }));
        }
    }
}

#[cfg(test)]
mod tests;
