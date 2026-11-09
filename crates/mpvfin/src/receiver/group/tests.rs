use jellyfin::{
    GroupCommand, GroupCommandKind, GroupQueue, GroupRequest, QueueMode, QueueReason, QueueRequest,
    ZERO_ID,
};

use super::*;
use crate::receiver::tests::{begin_playing, ended, gen_, open, resolve, settle, token};
use crate::receiver::{
    EndReason, Generation, PlaybackState, Receiver, ReceiverAction, ReceiverEvent, Segment,
    SegmentKind, StreamSelection,
};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn grouped() -> Receiver {
    let mut r = Receiver::new();
    assert!(
        r.handle(ReceiverEvent::GroupJoined {
            group_id: "g".into()
        })
        .is_empty()
    );
    r
}

fn entries(pids: &[&str]) -> Vec<GroupEntry> {
    pids.iter()
        .map(|pid| GroupEntry {
            item_id: format!("item-{pid}"),
            playlist_item_id: (*pid).to_owned(),
        })
        .collect()
}

fn queue(
    reason: QueueReason,
    last_update_ms: i64,
    pids: &[&str],
    playing_index: Option<usize>,
    start_position: Duration,
    is_playing: bool,
) -> ReceiverEvent {
    ReceiverEvent::GroupQueueChanged(GroupQueue {
        reason,
        last_update_ms,
        entries: entries(pids),
        playing_index,
        start_position,
        is_playing,
    })
}

fn new_queue(last_update_ms: i64, pids: &[&str], index: usize, start: Duration) -> ReceiverEvent {
    queue(
        QueueReason::NewPlaylist,
        last_update_ms,
        pids,
        Some(index),
        start,
        false,
    )
}

fn command(kind: GroupCommandKind, pid: &str, position: Duration, delay_ms: i64) -> ReceiverEvent {
    ReceiverEvent::GroupCommandReceived(GroupCommand {
        kind,
        playlist_item_id: pid.to_owned(),
        position,
        delay_ms,
    })
}

fn ready(position: Duration, pid: &str) -> ReceiverAction {
    ReceiverAction::Group(GroupRequest::Ready {
        position,
        is_playing: false,
        pid: pid.to_owned(),
    })
}

fn stop_following(g: Generation) -> Vec<ReceiverAction> {
    vec![
        ReceiverAction::CancelResume,
        ReceiverAction::DriftAnchor {
            generation: g,
            position: None,
        },
    ]
}

fn set_paused(g: Generation, paused: bool) -> ReceiverAction {
    ReceiverAction::SetPaused {
        generation: g,
        paused,
    }
}

fn seek(g: Generation, position: Duration) -> ReceiverAction {
    ReceiverAction::Seek {
        generation: g,
        position,
    }
}

fn anchor(g: Generation, position: Duration) -> ReceiverAction {
    ReceiverAction::DriftAnchor {
        generation: g,
        position: Some(position),
    }
}

fn open_pid(r: &mut Receiver, pid: &str, hook: u64) -> Vec<ReceiverAction> {
    r.handle(ReceiverEvent::PlayerOpening {
        token: token(1, hook),
        item_id: format!("item-{pid}"),
        playlist_item_id: Some(pid.to_owned()),
    })
}

/// Takes the entry from its hook to loaded, returning its generation.
fn load_pid(r: &mut Receiver, pid: &str, hook: u64) -> Generation {
    let g = match open_pid(r, pid, hook).last() {
        Some(ReceiverAction::ResolveMedia { generation, .. }) => *generation,
        other => panic!("unexpected action: {other:?}"),
    };
    resolve(r, g, &format!("item-{pid}"));
    r.handle(ReceiverEvent::PlayerFileLoaded { generation: g });
    g
}

fn pause_state(r: &mut Receiver, g: Generation, paused: bool) {
    r.handle(ReceiverEvent::PlayerPauseChanged {
        generation: g,
        paused,
    });
}

fn moved(r: &mut Receiver, g: Generation, position: Duration) {
    r.handle(ReceiverEvent::PlayerPositionChanged {
        generation: g,
        position,
        seek_completed: false,
    });
}

/// Grouped with entry `pid` loaded, settled at `at` and paused.
fn following(pid: &str, at: u64) -> (Receiver, Generation) {
    following_in(&[pid], at)
}

/// As `following`, on the first entry of `pids`.
fn following_in(pids: &[&str], at: u64) -> (Receiver, Generation) {
    let mut r = grouped();
    r.handle(new_queue(100, pids, 0, secs(at)));
    let g = load_pid(&mut r, pids[0], 70);
    pause_state(&mut r, g, true);
    let actions = settle(&mut r, g, at);
    assert_eq!(actions.last(), Some(&ready(secs(at), pids[0])));
    (r, g)
}

/// As `following_in`, with the group playing.
fn playing(pids: &[&str], at: u64) -> (Receiver, Generation) {
    let (mut r, g) = following_in(pids, at);
    r.handle(command(GroupCommandKind::Unpause, pids[0], secs(at), 0));
    pause_state(&mut r, g, false);
    assert!(!expected_paused(&r));
    (r, g)
}

fn req(request: GroupRequest) -> ReceiverAction {
    ReceiverAction::Group(request)
}

fn requests(actions: &[ReceiverAction]) -> Vec<&GroupRequest> {
    actions
        .iter()
        .filter_map(|a| match a {
            ReceiverAction::Group(request) => Some(request),
            _ => None,
        })
        .collect()
}

fn pause(r: &mut Receiver, g: Generation, paused: bool) -> Vec<ReceiverAction> {
    r.handle(ReceiverEvent::PlayerPauseChanged {
        generation: g,
        paused,
    })
}

fn only_progress(actions: &[ReceiverAction]) -> bool {
    matches!(actions, [ReceiverAction::ReportProgress(_)])
}

fn reached_end(g: Generation) -> ReceiverEvent {
    ReceiverEvent::PlayerReachedEnd { generation: g }
}

fn buffering(g: Generation, buffering: bool) -> ReceiverEvent {
    ReceiverEvent::PlayerBuffering {
        generation: g,
        buffering,
    }
}

fn pid(pid: &str) -> String {
    pid.to_owned()
}

fn ids(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| (*id).to_owned()).collect()
}

fn remote_play(item_ids: &[&str], mode: QueueMode, start_index: usize) -> ReceiverEvent {
    ReceiverEvent::RemotePlay(QueueRequest {
        item_ids: ids(item_ids),
        mode,
        start_index,
        start_position: secs(30),
        selection: StreamSelection::default(),
    })
}

/// Plays the first of `pids` inside a skippable outro.
fn in_outro(pids: &[&str], to_end_of_file: bool) -> (Receiver, Generation) {
    let (mut r, g) = playing(pids, 12);
    r.handle(ReceiverEvent::SegmentsResolved {
        generation: g,
        segments: vec![Segment {
            kind: SegmentKind::Outro,
            start: secs(10),
            end: secs(20),
            to_end_of_file,
        }],
    });
    (r, g)
}

fn own_seek(r: &Receiver) -> bool {
    r.group.as_ref().unwrap().own_seek.is_some()
}

fn expected_paused(r: &Receiver) -> bool {
    r.group.as_ref().unwrap().expected_paused
}

#[test]
fn joined_after_create_publishes_the_playing_queue() {
    let mut r = Receiver::new();
    let g = begin_playing(&mut r, "a", 70);
    settle(&mut r, g, 12);
    assert_eq!(
        r.handle(ReceiverEvent::GroupCreate {
            name: "movie".into()
        }),
        vec![ReceiverAction::Group(GroupRequest::New {
            name: "movie".into()
        })]
    );
    assert_eq!(r.group_id(), None);
    assert_eq!(
        r.handle(ReceiverEvent::GroupJoined {
            group_id: "g".into()
        }),
        vec![ReceiverAction::GroupPublishQueue { position: secs(12) }]
    );
    assert_eq!(r.group_id(), Some("g"));
    assert_eq!(
        r.handle(ReceiverEvent::GroupJoined {
            group_id: "g".into()
        }),
        stop_following(g),
        "the create was consumed, and a rejoin stops what the old group started"
    );
}

#[test]
fn joined_without_create_publishes_nothing() {
    let mut r = Receiver::new();
    begin_playing(&mut r, "a", 70);
    assert_eq!(
        r.handle(ReceiverEvent::GroupJoin {
            group_id: "g".into()
        }),
        vec![ReceiverAction::Group(GroupRequest::Join {
            group_id: "g".into()
        })]
    );
    assert!(
        r.handle(ReceiverEvent::GroupJoined {
            group_id: "g".into()
        })
        .is_empty()
    );
}

#[test]
fn a_failed_create_does_not_publish_later() {
    let mut r = Receiver::new();
    begin_playing(&mut r, "a", 70);
    r.handle(ReceiverEvent::GroupCreate { name: "m".into() });
    r.handle(ReceiverEvent::GroupGone {
        message: "no".into(),
    });
    assert!(
        r.handle(ReceiverEvent::GroupJoined {
            group_id: "g".into()
        })
        .is_empty()
    );
}

#[test]
fn a_failed_create_while_grouped_does_not_publish_on_the_rejoin() {
    let (mut r, g) = following("p1", 10);
    r.handle(ReceiverEvent::GroupCreate { name: "m".into() });
    // The New failed, then the socket came back and rejoined the old group.
    assert_eq!(
        r.handle(ReceiverEvent::GroupJoined {
            group_id: "g".into()
        }),
        stop_following(g),
        "a SetNewQueue here would replace the queue for everyone"
    );
    r.handle(ReceiverEvent::GroupLeft);
    assert!(
        r.handle(ReceiverEvent::GroupJoined {
            group_id: "h".into()
        })
        .is_empty(),
        "the rejoin used the create up"
    );
}

#[test]
fn older_queue_updates_are_ignored() {
    let mut r = grouped();
    assert!(!r.handle(new_queue(100, &["p1"], 0, secs(0))).is_empty());
    assert!(r.handle(new_queue(99, &["p2"], 0, secs(0))).is_empty());
}

#[test]
fn equal_last_update_is_applied() {
    let mut r = grouped();
    r.handle(new_queue(100, &["p1"], 0, secs(0)));
    assert!(matches!(
        r.handle(new_queue(100, &["p2"], 0, secs(0))).as_slice(),
        [
            ReceiverAction::CancelResume,
            ReceiverAction::GroupLoad { .. }
        ]
    ));
}

#[test]
fn new_item_loads_paused_and_readies_on_settle() {
    let mut r = grouped();
    assert_eq!(
        r.handle(new_queue(100, &["p1", "p2"], 1, secs(30))),
        vec![
            ReceiverAction::CancelResume,
            ReceiverAction::GroupLoad {
                entries: entries(&["p1", "p2"]),
                index: 1
            }
        ]
    );
    let g = match open_pid(&mut r, "p2", 70).as_slice() {
        [ReceiverAction::ResolveMedia { generation, .. }] => *generation,
        other => panic!("unexpected actions: {other:?}"),
    };
    let continued = resolve(&mut r, g, "item-p2");
    assert!(matches!(
        continued.as_slice(),
        [ReceiverAction::ContinueLoad { hook_id: 70, group: Some(o), .. }] if o.start == secs(30)
    ));
    assert!(matches!(
        r.handle(ReceiverEvent::PlayerFileLoaded { generation: g })
            .as_slice(),
        [ReceiverAction::ReportStarted(_)]
    ));
    let actions = settle(&mut r, g, 30);
    assert!(
        matches!(actions.as_slice(), [ReceiverAction::ReportProgress(_), a] if *a == ready(secs(30), "p2")),
        "{actions:?}"
    );
    assert!(
        !settle(&mut r, g, 30).contains(&ready(secs(30), "p2")),
        "one Ready per load"
    );
}

#[test]
fn already_loaded_item_seeks_then_readies() {
    let (mut r, g) = following("p1", 10);
    let mut expected = stop_following(g);
    expected.push(seek(g, secs(30)));
    assert_eq!(
        r.handle(queue(
            QueueReason::SetCurrentItem,
            200,
            &["p1"],
            Some(0),
            secs(30),
            false
        )),
        expected
    );
    assert!(own_seek(&r));
    let actions = settle(&mut r, g, 30);
    assert_eq!(actions.last(), Some(&ready(secs(30), "p1")));
}

#[test]
fn already_loaded_playing_item_pauses_before_it_seeks() {
    let (mut r, g) = following("p1", 10);
    pause_state(&mut r, g, false);
    let mut expected = stop_following(g);
    expected.extend([set_paused(g, true), seek(g, secs(30))]);
    assert_eq!(r.handle(new_queue(200, &["p1"], 0, secs(30))), expected);
}

#[test]
fn already_positioned_item_readies_at_once() {
    let (mut r, g) = following("p1", 30);
    moved(&mut r, g, ms(30_050));
    let mut expected = stop_following(g);
    expected.push(ready(ms(30_050), "p1"));
    assert_eq!(r.handle(new_queue(200, &["p1"], 0, secs(30))), expected);
    assert!(!own_seek(&r));
    assert!(
        !settle(&mut r, g, 30).contains(&ready(secs(30), "p1")),
        "already readied"
    );
}

#[test]
fn already_positioned_playing_item_is_paused_for_the_wait() {
    let (mut r, g) = following("p1", 30);
    pause_state(&mut r, g, false);
    let mut expected = stop_following(g);
    expected.extend([set_paused(g, true), ready(secs(30), "p1")]);
    assert_eq!(r.handle(new_queue(200, &["p1"], 0, secs(30))), expected);
}

#[test]
fn the_item_being_replaced_never_readies() {
    let (mut r, g) = following("p1", 10);
    r.handle(new_queue(200, &["p2"], 0, secs(0)));
    assert!(
        matches!(
            settle(&mut r, g, 10).as_slice(),
            [ReceiverAction::ReportProgress(_)]
        ),
        "p1 is still on screen, and the Ready would name p2"
    );
}

#[test]
fn a_repeat_update_during_the_load_readies_on_settle() {
    let mut r = grouped();
    r.handle(new_queue(100, &["p1"], 0, secs(30)));
    open_pid(&mut r, "p1", 70);
    assert_eq!(
        r.handle(new_queue(200, &["p1"], 0, secs(40))),
        stop_following(gen_(1)),
        "nothing to seek yet, and no second load"
    );
    assert!(matches!(
        resolve(&mut r, gen_(1), "item-p1").as_slice(),
        [ReceiverAction::ContinueLoad { group: Some(o), .. }] if o.start == secs(40)
    ));
    r.handle(ReceiverEvent::PlayerFileLoaded {
        generation: gen_(1),
    });
    assert_eq!(
        settle(&mut r, gen_(1), 40).last(),
        Some(&ready(secs(40), "p1"))
    );
}

#[test]
fn leaving_the_old_group_keeps_a_create_pending() {
    let mut r = Receiver::new();
    begin_playing(&mut r, "a", 70);
    r.handle(ReceiverEvent::GroupCreate { name: "m".into() });
    r.handle(ReceiverEvent::GroupLeft);
    assert!(matches!(
        r.handle(ReceiverEvent::GroupJoined {
            group_id: "g".into()
        })
        .as_slice(),
        [ReceiverAction::GroupPublishQueue { .. }]
    ));
}

#[test]
fn a_rejoin_keeps_the_loaded_item() {
    let (mut r, _) = following("p1", 30);
    r.handle(ReceiverEvent::GroupJoined {
        group_id: "g".into(),
    });
    assert!(
        r.handle(new_queue(50, &["p1"], 0, secs(30)))
            .contains(&ready(secs(30), "p1"))
    );
}

#[test]
fn empty_queue_loads_nothing() {
    let mut r = grouped();
    assert_eq!(
        r.handle(queue(
            QueueReason::NewPlaylist,
            100,
            &[],
            None,
            secs(0),
            false
        )),
        vec![ReceiverAction::CancelResume]
    );
    assert_eq!(
        r.handle(queue(
            QueueReason::NewPlaylist,
            100,
            &["p1"],
            Some(5),
            secs(0),
            false
        )),
        vec![ReceiverAction::CancelResume],
        "an index past the end names no entry"
    );
}

#[test]
fn losing_the_current_item_stops_following() {
    let (mut r, g) = following("p1", 10);
    assert_eq!(
        r.handle(queue(
            QueueReason::RemoveItems,
            200,
            &[],
            None,
            secs(0),
            false
        )),
        stop_following(g)
    );
}

#[test]
fn other_reasons_only_resync_the_playlist() {
    let (mut r, _) = following("p1", 10);
    let sync = vec![ReceiverAction::GroupSyncPlaylist {
        entries: entries(&["p1", "p2"]),
        index: 0,
    }];
    assert_eq!(
        r.handle(queue(
            QueueReason::Other,
            200,
            &["p1", "p2"],
            Some(0),
            secs(50),
            true
        )),
        sync
    );
    assert_eq!(
        r.handle(queue(
            QueueReason::RemoveItems,
            300,
            &["p1", "p2"],
            Some(0),
            secs(50),
            true
        )),
        sync,
        "the playing entry was not the one removed"
    );
}

#[test]
fn removed_playing_item_loads_and_plays_on() {
    let (mut r, g) = following("p1", 10);
    let mut expected = stop_following(g);
    expected.push(ReceiverAction::GroupLoad {
        entries: entries(&["p2"]),
        index: 0,
    });
    assert_eq!(
        r.handle(queue(
            QueueReason::RemoveItems,
            200,
            &["p2"],
            Some(0),
            secs(0),
            true
        )),
        expected
    );
    assert!(expected_paused(&r), "the hook holds the load paused");
    let g2 = load_pid(&mut r, "p2", 80);
    pause_state(&mut r, g2, true);
    assert!(
        expected_paused(&r),
        "that pause must not read as the viewer's"
    );
    let actions = settle(&mut r, g2, 0);
    assert!(
        matches!(
            actions.as_slice(),
            [ReceiverAction::ReportProgress(_), a, b]
                if *a == set_paused(g2, false) && *b == anchor(g2, secs(0))
        ),
        "the hook loads every group entry paused, and no Unpause will follow: {actions:?}"
    );
    assert!(!expected_paused(&r));
}

#[test]
fn a_pause_during_a_plays_on_load_wins() {
    for kind in [GroupCommandKind::Pause, GroupCommandKind::Seek] {
        let (mut r, _) = following("p1", 10);
        r.handle(queue(
            QueueReason::RemoveItems,
            200,
            &["p2"],
            Some(0),
            secs(0),
            true,
        ));
        let g2 = load_pid(&mut r, "p2", 80);
        pause_state(&mut r, g2, true);
        r.handle(command(kind, "p2", secs(0), 0));
        assert!(
            !settle(&mut r, g2, 0).contains(&set_paused(g2, false)),
            "{kind:?}"
        );
        assert!(expected_paused(&r), "{kind:?}");
    }
}

#[test]
fn unpause_during_a_load_leaves_it_held() {
    let mut r = grouped();
    r.handle(new_queue(100, &["p1"], 0, secs(30)));
    open_pid(&mut r, "p1", 70);
    assert!(
        r.handle(command(GroupCommandKind::Unpause, "p1", secs(30), 500))
            .is_empty()
    );
    assert_eq!(
        r.handle(command(GroupCommandKind::Unpause, "p1", secs(30), 0)),
        vec![ReceiverAction::CancelResume]
    );
    assert!(
        expected_paused(&r),
        "the server resends the Unpause to a late Ready"
    );
}

#[test]
fn commands_leave_the_old_file_alone_while_a_load_is_coming() {
    let (mut r, g) = following("p1", 10);
    pause_state(&mut r, g, false);
    r.handle(new_queue(200, &["p2"], 0, secs(0)));
    assert_eq!(
        r.handle(command(GroupCommandKind::Pause, "p2", secs(50), 0)),
        stop_following(g)
    );
    assert_eq!(
        r.handle(command(GroupCommandKind::Seek, "p2", secs(50), 0)),
        stop_following(g)
    );
    assert_eq!(
        r.handle(command(GroupCommandKind::Unpause, "p2", secs(50), 0)),
        vec![ReceiverAction::CancelResume]
    );
    assert!(
        r.handle(command(GroupCommandKind::Unpause, "p2", secs(50), 500))
            .is_empty()
    );
    assert!(!own_seek(&r));
    assert!(expected_paused(&r));
}

#[test]
fn seek_during_resolving_moves_the_start() {
    let mut r = grouped();
    r.handle(new_queue(100, &["p1"], 0, secs(30)));
    open_pid(&mut r, "p1", 70);
    r.handle(command(GroupCommandKind::Seek, "p1", secs(40), 0));
    assert!(matches!(
        resolve(&mut r, gen_(1), "item-p1").as_slice(),
        [ReceiverAction::ContinueLoad { group: Some(o), .. }] if o.start == secs(40)
    ));
}

fn left_for_a_failed_load(g: Generation) -> Vec<ReceiverAction> {
    let mut left = vec![ReceiverAction::Group(GroupRequest::Leave)];
    left.extend(stop_following(g));
    left.push(ReceiverAction::StopPlayer { generation: g });
    left.push(ReceiverAction::ShowMessage {
        text: "mpvfin: couldn't load that item, so you've left the group".into(),
    });
    left
}

#[test]
fn a_failed_entry_abandons_the_group_with_no_ready_owed() {
    let (mut r, g) = playing(&["p1", "p2"], 10);
    let actions = r.handle(ended(g, EndReason::Failed("x".into())));
    assert!(
        matches!(
            actions.as_slice(),
            [ReceiverAction::ReportStopped { .. }, ReceiverAction::PlaybackFailed { .. }, rest @ ..]
                if rest == left_for_a_failed_load(g).as_slice()
        ),
        "{actions:?}"
    );
    assert_eq!(r.group_id(), None);
    assert!(
        requests(&open_pid(&mut r, "p2", 80)).is_empty(),
        "mpv moving on is no longer the group's business"
    );
}

#[test]
fn a_failed_resolve_abandons_the_group_with_no_ready_owed() {
    let (mut r, _) = following("p1", 10);
    r.handle(queue(
        QueueReason::RemoveItems,
        200,
        &["p2", "p3"],
        Some(0),
        secs(0),
        true,
    ));
    open_pid(&mut r, "p2", 80);
    let actions = r.handle(ReceiverEvent::MediaResolutionFailed {
        generation: gen_(2),
        message: "boom".into(),
    });
    assert!(
        matches!(
            actions.as_slice(),
            [ReceiverAction::AbortLoad { hook_id: 80 }, ReceiverAction::PlaybackFailed { .. }, rest @ ..]
                if rest == left_for_a_failed_load(gen_(2)).as_slice()
        ),
        "{actions:?}"
    );
}

#[test]
fn a_failed_entry_the_group_has_moved_on_from_keeps_the_group() {
    let (mut r, g) = following_in(&["p1", "p2"], 10);
    r.handle(new_queue(200, &["p3"], 0, secs(0)));
    let actions = r.handle(ended(g, EndReason::Failed("x".into())));
    assert!(!actions.contains(&req(GroupRequest::Leave)), "{actions:?}");
    assert_eq!(r.group_id(), Some("g"));
}

#[test]
fn failed_load_leaves_the_group() {
    let mut r = grouped();
    r.handle(new_queue(100, &["p1"], 0, secs(30)));
    open_pid(&mut r, "p1", 70);
    let actions = r.handle(ReceiverEvent::MediaResolutionFailed {
        generation: gen_(1),
        message: "boom".into(),
    });
    assert!(
        matches!(
            actions.as_slice(),
            [ReceiverAction::AbortLoad { hook_id: 70 }, ReceiverAction::PlaybackFailed { .. }, rest @ ..]
                if rest == left_for_a_failed_load(gen_(1)).as_slice()
        ),
        "{actions:?}"
    );
    assert_eq!(r.group_id(), None);

    let mut r = grouped();
    r.handle(new_queue(100, &["p1"], 0, secs(30)));
    let g = load_pid(&mut r, "p1", 70);
    let actions = r.handle(ended(g, EndReason::Failed("x".into())));
    assert!(
        matches!(
            actions.as_slice(),
            [ReceiverAction::ReportStopped { .. }, ReceiverAction::PlaybackFailed { .. }, rest @ ..]
                if rest == left_for_a_failed_load(g).as_slice()
        ),
        "{actions:?}"
    );
    assert_eq!(r.group_id(), None);
}

#[test]
fn stopped_and_idle_while_awaiting_ready_leaves_the_group() {
    let mut r = grouped();
    r.handle(new_queue(100, &["p1"], 0, secs(30)));
    let g = load_pid(&mut r, "p1", 70);
    assert!(matches!(
        r.handle(ended(g, EndReason::Stopped)).as_slice(),
        [ReceiverAction::ReportStopped { .. }]
    ));
    assert_eq!(r.group_id(), Some("g"), "the next entry may be opening");
    assert_eq!(
        r.handle(ReceiverEvent::PlayerIdle),
        left_for_a_failed_load(g)
    );
    assert_eq!(r.group_id(), None);

    let mut r = grouped();
    r.handle(new_queue(100, &["p1"], 0, secs(30)));
    open_pid(&mut r, "p1", 70);
    assert_eq!(
        r.handle(ended(gen_(1), EndReason::Stopped)),
        vec![ReceiverAction::AbortLoad { hook_id: 70 }]
    );
    assert_eq!(
        r.handle(ReceiverEvent::PlayerIdle),
        left_for_a_failed_load(gen_(1))
    );
}

#[test]
fn pressing_next_during_an_owed_ready_keeps_the_group() {
    let (mut r, g) = following_in(&["p1", "p2", "p3"], 10);
    r.handle(command(GroupCommandKind::Seek, "p1", secs(40), 0));
    assert!(matches!(
        r.handle(ended(g, EndReason::Stopped)).as_slice(),
        [ReceiverAction::ReportStopped { .. }]
    ));
    assert_eq!(
        requests(&open_pid(&mut r, "p2", 80)),
        [&GroupRequest::SetPlaylistItem { pid: pid("p2") }]
    );
    assert_eq!(
        r.handle(ended(gen_(2), EndReason::Stopped)),
        vec![ReceiverAction::AbortLoad { hook_id: 80 }],
        "pressed again while p2 was still resolving"
    );
    assert_eq!(
        requests(&open_pid(&mut r, "p3", 90)),
        [&GroupRequest::SetPlaylistItem { pid: pid("p3") }]
    );
    assert!(
        r.handle(ReceiverEvent::PlayerIdle).is_empty(),
        "something opened"
    );
    assert_eq!(r.group_id(), Some("g"));
}

#[test]
fn picking_an_entry_during_a_load_keeps_the_group() {
    let mut r = grouped();
    r.handle(new_queue(100, &["p1", "p2"], 0, secs(30)));
    open_pid(&mut r, "p1", 70);
    r.handle(ended(gen_(1), EndReason::Stopped));
    assert_eq!(
        requests(&open_pid(&mut r, "p2", 80)),
        [&GroupRequest::SetPlaylistItem { pid: pid("p2") }]
    );
    assert_eq!(r.group_id(), Some("g"));
}

#[test]
fn a_group_load_after_a_stop_is_not_abandoned() {
    let (mut r, g) = following_in(&["p1", "p2"], 10);
    r.handle(command(GroupCommandKind::Seek, "p1", secs(40), 0));
    r.handle(ended(g, EndReason::Stopped));
    r.handle(new_queue(200, &["p1", "p2"], 1, secs(0)));
    assert!(r.handle(ReceiverEvent::PlayerIdle).is_empty());
    assert_eq!(r.group_id(), Some("g"));
}

#[test]
fn a_stream_dying_while_buffering_leaves_the_group() {
    let (mut r, g) = playing(&["p1"], 10);
    r.handle(buffering(g, true));
    let actions = r.handle(ended(g, EndReason::Failed("x".into())));
    assert!(actions.contains(&req(GroupRequest::Leave)), "{actions:?}");
    assert_eq!(r.group_id(), None);

    let (mut r, g) = playing(&["p1"], 10);
    r.handle(buffering(g, true));
    r.handle(ended(g, EndReason::Stopped));
    assert_eq!(
        r.handle(ReceiverEvent::PlayerIdle),
        left_for_a_failed_load(g)
    );
}

#[test]
fn a_stream_stopped_early_leaves_without_moving_the_group() {
    let (mut r, g) = playing(&["p1", "p2"], 10);
    let mut left = vec![req(GroupRequest::Leave)];
    left.extend(stop_following(g));
    left.push(ReceiverAction::StopPlayer { generation: g });
    left.push(ReceiverAction::ShowMessage {
        text: "mpvfin: the stream stopped early, so you've left the group".into(),
    });
    assert_eq!(
        r.handle(ReceiverEvent::PlayerStoppedEarly { generation: g }),
        left,
        "no NextItem or Stop for the others"
    );
    assert_eq!(r.group_id(), None);
}

#[test]
fn an_early_stop_of_anything_but_the_groups_item_is_ignored() {
    let (mut r, g) = playing(&["p1"], 10);
    r.handle(new_queue(200, &["p2"], 0, secs(0)));
    assert!(
        r.handle(ReceiverEvent::PlayerStoppedEarly { generation: g })
            .is_empty(),
        "that end is p1's, and the group has moved on"
    );
    assert_eq!(r.group_id(), Some("g"));

    let mut r = Receiver::new();
    let g = begin_playing(&mut r, "a", 70);
    assert!(
        r.handle(ReceiverEvent::PlayerStoppedEarly { generation: g })
            .is_empty()
    );
}

#[test]
fn a_lost_drift_clock_asks_the_server_where_the_group_is() {
    let (mut r, g) = playing(&["p1"], 10);
    assert_eq!(
        r.handle(ReceiverEvent::DriftLost),
        vec![req(GroupRequest::Unpause)]
    );
    // Not counted as ours, so the fresh position in the resend is obeyed.
    let actions = r.handle(command(GroupCommandKind::Unpause, "p1", secs(50), 0));
    assert!(actions.contains(&seek(g, secs(50))), "{actions:?}");
    assert!(actions.contains(&anchor(g, secs(50))), "{actions:?}");
}

#[test]
fn a_lost_drift_clock_means_nothing_unless_the_group_is_playing() {
    let (mut r, _) = following("p1", 10);
    assert!(r.handle(ReceiverEvent::DriftLost).is_empty(), "paused");

    let mut r = Receiver::new();
    begin_playing(&mut r, "a", 70);
    assert!(r.handle(ReceiverEvent::DriftLost).is_empty(), "no group");
}

#[test]
fn seek_with_nothing_loaded_leaves_the_group() {
    let (mut r, g) = following("p1", 10);
    r.handle(command(GroupCommandKind::Stop, "p1", secs(0), 0));
    r.handle(ended(g, EndReason::Stopped));
    assert_eq!(
        r.handle(command(GroupCommandKind::Seek, "p1", secs(40), 0)),
        left_for_a_failed_load(g)
    );
    assert_eq!(r.group_id(), None);
}

#[test]
fn a_seek_racing_the_load_does_not_leave() {
    let mut r = grouped();
    r.handle(new_queue(100, &["p1"], 0, secs(30)));
    assert_eq!(
        r.handle(command(GroupCommandKind::Seek, "p1", secs(40), 0)),
        vec![ReceiverAction::CancelResume],
        "the load is on its way; mpv has not reached its hook yet"
    );
    assert_eq!(r.group_id(), Some("g"));
}

#[test]
fn replacing_the_item_keeps_the_group() {
    let (mut r, g) = following("p1", 10);
    r.handle(new_queue(200, &["p2"], 0, secs(5)));
    assert!(
        matches!(
            r.handle(ended(g, EndReason::Stopped)).as_slice(),
            [ReceiverAction::ReportStopped { .. }]
        ),
        "the wait belongs to p2, and this end is p1's"
    );
    assert_eq!(r.group_id(), Some("g"));
    let g2 = load_pid(&mut r, "p2", 80);
    assert_eq!(settle(&mut r, g2, 5).last(), Some(&ready(secs(5), "p2")));
}

#[test]
fn idle_stop_is_ignored() {
    let mut r = grouped();
    assert!(
        r.handle(command(GroupCommandKind::Stop, ZERO_ID, secs(0), 0))
            .is_empty()
    );
    r.handle(new_queue(100, &["p1"], 0, secs(0)));
    assert!(
        r.handle(command(GroupCommandKind::Stop, "p1", secs(0), 0))
            .is_empty(),
        "the load has not started"
    );
    let (mut r, _) = following("p1", 10);
    assert!(
        r.handle(command(GroupCommandKind::Stop, ZERO_ID, secs(0), 0))
            .is_empty(),
        "the zero id is the normal notice for an empty group"
    );
}

#[test]
fn stop_stops_the_loaded_item() {
    let (mut r, g) = following("p1", 10);
    let mut expected = vec![ReceiverAction::StopPlayer { generation: g }];
    expected.extend(stop_following(g));
    assert_eq!(
        r.handle(command(GroupCommandKind::Stop, "p1", secs(0), 0)),
        expected
    );
    assert_eq!(r.group_id(), Some("g"));
}

#[test]
fn a_commanded_stop_during_the_wait_keeps_the_group() {
    let (mut r, g) = following("p1", 10);
    r.handle(command(GroupCommandKind::Seek, "p1", secs(40), 0));
    r.handle(command(GroupCommandKind::Stop, "p1", secs(0), 0));
    assert!(matches!(
        r.handle(ended(g, EndReason::Stopped)).as_slice(),
        [ReceiverAction::ReportStopped { .. }]
    ));
    assert_eq!(r.group_id(), Some("g"));
}

#[test]
fn stop_while_resolving_continues_the_hook() {
    let mut r = grouped();
    r.handle(new_queue(100, &["p1"], 0, secs(0)));
    open_pid(&mut r, "p1", 70);
    let mut expected = vec![
        ReceiverAction::AbortLoad { hook_id: 70 },
        ReceiverAction::StopPlayer {
            generation: gen_(1),
        },
    ];
    expected.extend(stop_following(gen_(1)));
    assert_eq!(
        r.handle(command(GroupCommandKind::Stop, "p1", secs(0), 0)),
        expected
    );
}

#[test]
fn pause_seeks_to_the_given_position() {
    let (mut r, g) = playing(&["p1"], 10);
    let mut expected = stop_following(g);
    expected.extend([set_paused(g, true), seek(g, secs(12))]);
    assert_eq!(
        r.handle(command(GroupCommandKind::Pause, "p1", secs(12), 0)),
        expected
    );
    assert!(own_seek(&r));

    let (mut r, g) = following("p1", 10);
    assert_eq!(
        r.handle(command(GroupCommandKind::Pause, "p1", ms(10_100), 0)),
        stop_following(g),
        "already paused, and within tolerance"
    );
    assert!(!own_seek(&r));
}

#[test]
fn seek_readies_after_it_lands() {
    let (mut r, g) = following("p1", 10);
    pause_state(&mut r, g, false);
    let mut expected = stop_following(g);
    expected.extend([set_paused(g, true), seek(g, secs(40))]);
    assert_eq!(
        r.handle(command(GroupCommandKind::Seek, "p1", secs(40), 0)),
        expected
    );
    assert!(own_seek(&r));
    assert_eq!(settle(&mut r, g, 40).last(), Some(&ready(secs(40), "p1")));
}

#[test]
fn future_unpause_is_scheduled() {
    let (mut r, g) = following("p1", 10);
    assert_eq!(
        r.handle(command(GroupCommandKind::Unpause, "p1", secs(30), 500)),
        vec![
            seek(g, secs(30)),
            ReceiverAction::ScheduleResume {
                generation: g,
                after: ms(500)
            }
        ]
    );
    assert!(own_seek(&r));

    let (mut r, g) = following("p1", 30);
    assert_eq!(
        r.handle(command(GroupCommandKind::Unpause, "p1", secs(30), 500)),
        vec![ReceiverAction::ScheduleResume {
            generation: g,
            after: ms(500)
        }]
    );
}

#[test]
fn late_unpause_catches_up() {
    let (mut r, g) = following("p1", 10);
    assert_eq!(
        r.handle(command(GroupCommandKind::Unpause, "p1", secs(30), -2000)),
        vec![
            ReceiverAction::CancelResume,
            seek(g, secs(32)),
            set_paused(g, false),
            anchor(g, secs(32))
        ]
    );

    let (mut r, g) = following("p1", 32);
    assert_eq!(
        r.handle(command(GroupCommandKind::Unpause, "p1", secs(30), -2000)),
        vec![
            ReceiverAction::CancelResume,
            set_paused(g, false),
            anchor(g, secs(32))
        ]
    );

    let (mut r, g) = following("p1", 30);
    assert_eq!(
        r.handle(command(GroupCommandKind::Unpause, "p1", secs(30), 0)),
        vec![
            ReceiverAction::CancelResume,
            set_paused(g, false),
            anchor(g, secs(30))
        ],
        "zero delay is due now"
    );
}

#[test]
fn resume_due_starts_playback() {
    let (mut r, g) = following("p1", 10);
    r.handle(command(GroupCommandKind::Unpause, "p1", secs(30), 500));
    assert!(
        r.handle(ReceiverEvent::GroupResumeDue {
            generation: gen_(99)
        })
        .is_empty(),
        "stale generation"
    );
    assert_eq!(
        r.handle(ReceiverEvent::GroupResumeDue { generation: g }),
        vec![set_paused(g, false), anchor(g, secs(30))],
        "anchored where the group starts, not where the seek has reached"
    );
}

#[test]
fn resume_due_after_a_pause_does_nothing() {
    let (mut r, g) = following("p1", 30);
    r.handle(command(GroupCommandKind::Unpause, "p1", secs(30), 500));
    r.handle(command(GroupCommandKind::Pause, "p1", secs(30), 0));
    assert!(
        r.handle(ReceiverEvent::GroupResumeDue { generation: g })
            .is_empty()
    );
}

#[test]
fn commands_for_another_item_are_ignored() {
    let (mut r, _) = following("p1", 10);
    for kind in [
        GroupCommandKind::Unpause,
        GroupCommandKind::Pause,
        GroupCommandKind::Seek,
        GroupCommandKind::Stop,
    ] {
        assert!(r.handle(command(kind, "other", secs(30), 0)).is_empty());
    }
}

#[test]
fn commands_outside_a_group_are_ignored() {
    let mut r = Receiver::new();
    begin_playing(&mut r, "a", 70);
    assert!(
        r.handle(command(GroupCommandKind::Pause, ZERO_ID, secs(1), 0))
            .is_empty()
    );
}

#[test]
fn leaving_keeps_playback() {
    let (mut r, g) = following("p1", 10);
    let mut expected = vec![ReceiverAction::Group(GroupRequest::Leave)];
    expected.extend(stop_following(g));
    assert_eq!(r.handle(ReceiverEvent::GroupLeave), expected);
    assert_eq!(r.group_id(), None);
    assert!(matches!(r.playback(), PlaybackState::Playing(_)));
    assert!(r.handle(ReceiverEvent::GroupLeft).is_empty(), "already out");
    assert!(
        r.handle(command(GroupCommandKind::Unpause, "p1", secs(0), 0))
            .is_empty(),
        "an Unpause can arrive just before GroupLeft"
    );
}

#[test]
fn the_server_ending_membership_keeps_playback() {
    let (mut r, g) = following("p1", 10);
    assert_eq!(r.handle(ReceiverEvent::GroupLeft), stop_following(g));
    assert_eq!(r.group_id(), None);
    assert!(matches!(r.playback(), PlaybackState::Playing(_)));
}

#[test]
fn gone_shows_why() {
    let (mut r, g) = following("p1", 10);
    let mut expected = stop_following(g);
    expected.push(ReceiverAction::ShowMessage {
        text: "gone".into(),
    });
    assert_eq!(
        r.handle(ReceiverEvent::GroupGone {
            message: "gone".into()
        }),
        expected
    );
    assert_eq!(r.group_id(), None);
    assert!(matches!(r.playback(), PlaybackState::Playing(_)));
}

#[test]
fn a_refused_join_still_says_why() {
    let mut r = Receiver::new();
    assert_eq!(
        r.handle(ReceiverEvent::GroupGone {
            message: "no such group".into()
        }),
        vec![ReceiverAction::ShowMessage {
            text: "no such group".into()
        }]
    );
}

#[test]
fn ending_an_item_keeps_the_group() {
    let (mut r, g) = following("p1", 10);
    r.handle(ended(g, EndReason::Eof));
    assert_eq!(r.group_id(), Some("g"));
}

#[test]
fn outside_a_group_loads_carry_no_options() {
    let mut r = Receiver::new();
    open(&mut r, "a", 70);
    assert!(matches!(
        resolve(&mut r, gen_(1), "a").as_slice(),
        [ReceiverAction::ContinueLoad { group: None, .. }]
    ));
}

#[test]
fn opening_another_entry_loads_without_group_options() {
    let mut r = grouped();
    r.handle(new_queue(100, &["p1"], 0, secs(30)));
    assert!(
        requests(&open_pid(&mut r, "other", 70)).is_empty(),
        "the server would lose its item to a pid it doesn't know"
    );
    assert!(matches!(
        resolve(&mut r, gen_(1), "item-other").as_slice(),
        [ReceiverAction::ContinueLoad { group: None, .. }]
    ));
}

#[test]
fn local_pause_requests_a_group_pause() {
    let (mut r, g) = playing(&["p1"], 10);
    let actions = pause(&mut r, g, true);
    let mut expected = vec![req(GroupRequest::Pause)];
    expected.extend(stop_following(g));
    assert!(matches!(&actions[0], ReceiverAction::ReportProgress(s) if s.paused));
    assert_eq!(&actions[1..], expected.as_slice());
    assert!(expected_paused(&r));
}

#[test]
fn local_resume_requests_unpause_and_holds() {
    let (mut r, g) = following("p1", 10);
    let actions = pause(&mut r, g, false);
    assert!(matches!(&actions[0], ReceiverAction::ReportProgress(s) if !s.paused));
    assert_eq!(
        &actions[1..],
        [req(GroupRequest::Unpause), set_paused(g, true)].as_slice()
    );
    assert!(expected_paused(&r), "held until the broadcast");
    assert!(only_progress(&pause(&mut r, g, true)), "the hold landing");
}

#[test]
fn a_hold_confirmed_after_the_broadcast_is_not_the_viewer_pausing() {
    let (mut r, g) = following("p1", 10);
    pause(&mut r, g, false);
    // The server's broadcast overtakes mpv's confirmation of the hold.
    r.handle(command(GroupCommandKind::Unpause, "p1", secs(10), 500));
    assert!(only_progress(&pause(&mut r, g, true)), "the hold landing");
    assert!(
        r.handle(ReceiverEvent::GroupResumeDue { generation: g })
            .contains(&set_paused(g, false)),
        "the scheduled start must survive it"
    );

    pause(&mut r, g, false);
    assert!(
        pause(&mut r, g, true).contains(&req(GroupRequest::Pause)),
        "a real pause afterwards still reaches the group"
    );
}

#[test]
fn commanded_pause_is_not_echoed() {
    let (mut r, g) = playing(&["p1"], 10);
    r.handle(command(GroupCommandKind::Pause, "p1", secs(10), 0));
    assert!(only_progress(&pause(&mut r, g, true)));
    r.handle(command(GroupCommandKind::Unpause, "p1", secs(10), 0));
    assert!(only_progress(&pause(&mut r, g, false)));
}

#[test]
fn a_same_item_update_that_plays_is_held_for_its_ready() {
    let (mut r, g) = playing(&["p1"], 30);
    let actions = r.handle(queue(
        QueueReason::SetCurrentItem,
        200,
        &["p1"],
        Some(0),
        secs(30),
        true,
    ));
    assert!(actions.contains(&set_paused(g, true)));
    assert!(expected_paused(&r));
    assert!(only_progress(&pause(&mut r, g, true)));
}

#[test]
fn local_seek_requests_a_group_seek() {
    let (mut r, g) = playing(&["p1"], 10);
    let actions = settle(&mut r, g, 40);
    let mut expected = vec![req(GroupRequest::Seek { position: secs(40) })];
    expected.extend(stop_following(g));
    assert!(matches!(&actions[0], ReceiverAction::ReportProgress(_)));
    assert_eq!(&actions[1..], expected.as_slice());
}

#[test]
fn own_seek_is_not_echoed() {
    let (mut r, g) = playing(&["p1"], 10);
    let drift = |g, position| ReceiverEvent::DriftSeek {
        generation: g,
        position,
    };
    assert!(r.handle(drift(gen_(99), secs(12))).is_empty(), "stale");
    assert_eq!(r.handle(drift(g, secs(12))), vec![seek(g, secs(12))]);
    assert!(own_seek(&r));
    assert!(requests(&settle(&mut r, g, 12)).is_empty());
    assert!(!own_seek(&r));
    assert_eq!(
        requests(&settle(&mut r, g, 50)),
        [&GroupRequest::Seek { position: secs(50) }],
        "the next one is the viewer's"
    );
}

#[test]
fn a_loads_first_settle_is_never_a_user_seek() {
    let mut r = grouped();
    r.handle(new_queue(100, &["p1"], 0, secs(30)));
    let g = load_pid(&mut r, "p1", 70);
    r.handle(ReceiverEvent::GroupJoined {
        group_id: "g".into(),
    });
    r.handle(queue(
        QueueReason::Other,
        50,
        &["p1"],
        Some(0),
        secs(30),
        false,
    ));
    assert!(
        requests(&settle(&mut r, g, 30)).is_empty(),
        "no wait and no seek of ours, and still only the load landing"
    );
}

#[test]
fn a_seek_behind_the_loads_settle_is_still_ours() {
    let mut r = grouped();
    r.handle(new_queue(100, &["p1"], 0, secs(30)));
    let g = load_pid(&mut r, "p1", 70);
    r.handle(command(GroupCommandKind::Seek, "p1", secs(40), 0));
    assert_eq!(settle(&mut r, g, 30).last(), Some(&ready(secs(30), "p1")));
    assert!(own_seek(&r), "that was the load; the seek is still to land");
    assert!(requests(&settle(&mut r, g, 40)).is_empty());
    assert!(!own_seek(&r));
}

#[test]
fn a_seek_landing_with_the_load_is_used_up() {
    let mut r = grouped();
    r.handle(new_queue(100, &["p1"], 0, secs(30)));
    let g = load_pid(&mut r, "p1", 70);
    r.handle(command(GroupCommandKind::Seek, "p1", secs(40), 0));
    assert_eq!(settle(&mut r, g, 40).last(), Some(&ready(secs(40), "p1")));
    assert!(!own_seek(&r), "mpv folded the seek into the load");
    assert_eq!(
        requests(&settle(&mut r, g, 50)),
        [&GroupRequest::Seek { position: secs(50) }]
    );
}

#[test]
fn opening_forgets_a_seek_in_flight() {
    let (mut r, _) = following("p1", 10);
    r.handle(command(GroupCommandKind::Seek, "p1", secs(40), 0));
    assert!(own_seek(&r));
    open_pid(&mut r, "p1", 80);
    assert!(!own_seek(&r));
}

#[test]
fn remote_play_replaces_the_group_queue() {
    let (mut r, _) = following("p1", 10);
    assert_eq!(
        r.handle(remote_play(&["a", "b"], QueueMode::Replace, 5)),
        vec![req(GroupRequest::SetNewQueue {
            item_ids: ids(&["a", "b"]),
            index: 1,
            start: secs(30),
        })]
    );
}

#[test]
fn remote_queue_modes_map() {
    let (mut r, _) = following("p1", 10);
    for (mode, next) in [(QueueMode::Next, true), (QueueMode::Last, false)] {
        assert_eq!(
            r.handle(remote_play(&["a"], mode, 0)),
            vec![req(GroupRequest::Queue {
                item_ids: ids(&["a"]),
                next
            })]
        );
    }
}

#[test]
fn remote_controls_map_to_requests() {
    let (mut r, _) = following_in(&["p1", "p2"], 10);
    for (event, request) in [
        (ReceiverEvent::RemotePause, GroupRequest::Pause),
        (ReceiverEvent::RemoteResume, GroupRequest::Unpause),
        (ReceiverEvent::RemoteTogglePause, GroupRequest::Unpause),
        (
            ReceiverEvent::RemoteSeek(secs(40)),
            GroupRequest::Seek { position: secs(40) },
        ),
        (ReceiverEvent::RemoteStop, GroupRequest::Stop),
        (
            ReceiverEvent::RemoteNext,
            GroupRequest::NextItem { pid: pid("p1") },
        ),
    ] {
        assert_eq!(r.handle(event), vec![req(request)]);
    }
    assert!(
        r.handle(ReceiverEvent::RemotePrevious).is_empty(),
        "the first entry has no previous, and a no-op in Waiting idles the group"
    );

    let mut r = grouped();
    r.handle(new_queue(100, &["p1", "p2"], 1, secs(0)));
    assert_eq!(
        r.handle(ReceiverEvent::RemotePrevious),
        vec![req(GroupRequest::PreviousItem { pid: pid("p2") })]
    );
    assert!(
        r.handle(ReceiverEvent::RemoteNext).is_empty(),
        "the last entry has no next"
    );

    let (mut r, _) = playing(&["p1"], 10);
    assert_eq!(
        r.handle(ReceiverEvent::RemoteTogglePause),
        vec![req(GroupRequest::Pause)]
    );

    let mut r = grouped();
    assert!(
        r.handle(ReceiverEvent::RemoteNext).is_empty(),
        "only ever with the current pid"
    );
}

#[test]
fn volume_stays_local() {
    let (mut r, g) = following("p1", 10);
    assert_eq!(
        r.handle(ReceiverEvent::RemoteSetVolume(50)),
        vec![ReceiverAction::SetVolume {
            generation: g,
            volume_percent: 50
        }]
    );
    assert_eq!(
        r.handle(ReceiverEvent::RemoteSetMuted(true)),
        vec![ReceiverAction::SetMuted {
            generation: g,
            muted: true
        }]
    );
    assert_eq!(
        r.handle(ReceiverEvent::RemoteSetAudioStream(2)),
        vec![ReceiverAction::SetAudioStream {
            generation: g,
            index: 2
        }]
    );
}

#[test]
fn navigating_mpvs_playlist_sets_the_group_item() {
    let (mut r, _) = playing(&["p1", "p2"], 30);
    let g2 = gen_(2);
    let actions = open_pid(&mut r, "p2", 80);
    assert_eq!(
        requests(&actions),
        [&GroupRequest::SetPlaylistItem { pid: pid("p2") }]
    );
    assert!(actions.contains(&ReceiverAction::CancelResume));
    assert!(actions.contains(&ReceiverAction::DriftAnchor {
        generation: g2,
        position: None
    }));
    assert!(matches!(
        actions.last(),
        Some(ReceiverAction::ResolveMedia { .. })
    ));
    assert!(matches!(
        resolve(&mut r, g2, "item-p2").as_slice(),
        [ReceiverAction::ContinueLoad { group: Some(o), .. }] if o.start == secs(0)
    ));
    r.handle(ReceiverEvent::PlayerFileLoaded { generation: g2 });
    assert!(only_progress(&pause(&mut r, g2, true)), "held by the hook");
    assert_eq!(settle(&mut r, g2, 0).last(), Some(&ready(secs(0), "p2")));
}

#[test]
fn opening_the_current_entry_is_not_navigation() {
    let mut r = grouped();
    r.handle(new_queue(100, &["p1", "p2"], 1, secs(30)));
    assert!(requests(&open_pid(&mut r, "p2", 70)).is_empty());
}

#[test]
fn end_of_item_requests_next_once() {
    let (mut r, g) = playing(&["p1", "p2"], 10);
    assert!(r.handle(reached_end(gen_(99))).is_empty(), "stale");
    assert_eq!(
        r.handle(reached_end(g)),
        vec![req(GroupRequest::NextItem { pid: pid("p1") })]
    );
    assert!(r.handle(reached_end(g)).is_empty());
}

#[test]
fn a_pause_at_the_end_is_not_sent_to_the_group() {
    let (mut r, g) = playing(&["p1", "p2"], 10);
    r.handle(reached_end(g));
    assert!(
        only_progress(&pause(&mut r, g, true)),
        "a Pause after NextItem would stop the group resuming"
    );
}

#[test]
fn end_of_queue_stops_the_group() {
    let (mut r, g) = playing(&["p1"], 10);
    assert_eq!(r.handle(reached_end(g)), vec![req(GroupRequest::Stop)]);
}

#[test]
fn the_item_being_replaced_requests_nothing() {
    let (mut r, g) = playing(&["p1"], 10);
    r.handle(new_queue(200, &["p2"], 0, secs(0)));
    assert!(only_progress(&pause(&mut r, g, true)));
    assert!(only_progress(&settle(&mut r, g, 50)));
    assert!(r.handle(reached_end(g)).is_empty());
    assert!(r.handle(buffering(g, true)).is_empty());
}

#[test]
fn a_file_the_group_does_not_know_requests_nothing() {
    let mut r = grouped();
    let g = begin_playing(&mut r, "a", 70);
    assert!(only_progress(&pause(&mut r, g, true)));
    assert!(only_progress(&settle(&mut r, g, 50)));
}

#[test]
fn buffering_is_reported_once_and_cleared() {
    let (mut r, g) = playing(&["p1"], 10);
    let mut expected = vec![req(GroupRequest::Buffering {
        position: secs(10),
        pid: pid("p1"),
    })];
    expected.extend(stop_following(g));
    assert_eq!(r.handle(buffering(g, true)), expected, "the group freezes");
    assert!(r.handle(buffering(g, true)).is_empty(), "once");
    moved(&mut r, g, secs(11));
    assert_eq!(
        r.handle(buffering(g, false)),
        vec![
            req(GroupRequest::Ready {
                position: secs(11),
                is_playing: true,
                pid: pid("p1")
            }),
            anchor(g, secs(11)),
        ],
        "a Ready that says playing gets no Unpause to anchor on"
    );
    assert!(r.handle(buffering(g, false)).is_empty());
    assert!(r.handle(buffering(gen_(99), true)).is_empty(), "stale");
}

#[test]
fn buffering_while_loading_is_not_reported() {
    let mut r = grouped();
    r.handle(new_queue(100, &["p1"], 0, secs(30)));
    let g = load_pid(&mut r, "p1", 70);
    assert!(r.handle(buffering(g, true)).is_empty());
    assert!(r.handle(buffering(g, false)).is_empty());

    r.handle(command(GroupCommandKind::Unpause, "p1", secs(30), 0));
    assert!(!expected_paused(&r));
    assert!(
        r.handle(buffering(g, true)).is_empty(),
        "a forced start before the settle: the Ready is still owed"
    );

    let (mut r, g) = following("p1", 10);
    assert!(
        r.handle(buffering(g, true)).is_empty(),
        "the group is paused"
    );
}

#[test]
fn a_seek_overtaking_buffering_owes_the_ready_to_its_settle() {
    let (mut r, g) = playing(&["p1"], 10);
    r.handle(buffering(g, true));
    r.handle(command(GroupCommandKind::Seek, "p1", secs(40), 0));
    assert!(r.handle(buffering(g, false)).is_empty());
    assert_eq!(settle(&mut r, g, 40).last(), Some(&ready(secs(40), "p1")));
}

#[test]
fn pausing_while_buffering_still_ends_the_wait() {
    let (mut r, g) = playing(&["p1"], 10);
    r.handle(buffering(g, true));
    pause(&mut r, g, true);
    assert_eq!(
        r.handle(buffering(g, false)),
        vec![ready(secs(10), "p1")],
        "the server waits on this Ready whatever the group does next"
    );
}

#[test]
fn skip_in_group_is_a_group_seek() {
    let (mut r, g) = in_outro(&["p1", "p2"], false);
    assert_eq!(
        r.handle(ReceiverEvent::SkipRequested),
        vec![
            ReceiverAction::MarkPlayed {
                generation: g,
                item_id: "item-p1".into()
            },
            ReceiverAction::HideSkipButton { generation: g },
            req(GroupRequest::Seek { position: secs(20) }),
        ]
    );
}

#[test]
fn skip_to_end_in_group_requests_next() {
    let (mut r, g) = in_outro(&["p1", "p2"], true);
    assert_eq!(
        r.handle(ReceiverEvent::SkipRequested),
        vec![
            ReceiverAction::MarkPlayed {
                generation: g,
                item_id: "item-p1".into()
            },
            ReceiverAction::HideSkipButton { generation: g },
            req(GroupRequest::NextItem { pid: pid("p1") }),
        ]
    );
    assert!(r.handle(reached_end(g)).is_empty(), "already requested");
}

#[test]
fn play_now_in_group_moves_the_group_without_touching_pause() {
    let (mut r, g) = in_outro(&["p1", "p2"], false);
    assert_eq!(
        r.handle(ReceiverEvent::NextUpRequested),
        vec![
            ReceiverAction::MarkPlayed {
                generation: g,
                item_id: "item-p1".into()
            },
            ReceiverAction::HideNextUp { generation: g },
            req(GroupRequest::NextItem { pid: pid("p1") }),
        ]
    );
}

#[test]
fn a_pause_on_its_way_during_buffering_gets_no_anchor() {
    let (mut r, g) = playing(&["p1"], 10);
    r.handle(buffering(g, true));
    r.handle(command(GroupCommandKind::Pause, "p1", secs(10), 0));
    assert_eq!(
        r.handle(buffering(g, false)),
        vec![req(GroupRequest::Ready {
            position: secs(10),
            is_playing: true,
            pid: pid("p1")
        })],
        "mpv hasn't confirmed the pause, but the group is no longer running"
    );
}

#[test]
fn buffering_through_a_scheduled_resume_reports_the_player_paused() {
    let (mut r, g) = following("p1", 10);
    r.handle(command(GroupCommandKind::Unpause, "p1", secs(10), 500));
    assert!(!expected_paused(&r));
    r.handle(buffering(g, true));
    assert_eq!(
        r.handle(buffering(g, false)),
        vec![ready(secs(10), "p1")],
        "still paused, so no anchor either"
    );
}

#[test]
fn a_viewers_pause_broadcast_does_not_undo_their_seek() {
    let (mut r, g) = playing(&["p1"], 100);
    pause(&mut r, g, true);
    moved(&mut r, g, secs(400));
    let actions = r.handle(command(GroupCommandKind::Pause, "p1", secs(100), 0));
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, ReceiverAction::Seek { .. })),
        "uosc pauses, then seeks: {actions:?}"
    );
    assert_eq!(
        requests(&settle(&mut r, g, 400)),
        [&GroupRequest::Seek {
            position: secs(400)
        }]
    );
    assert_eq!(
        r.handle(command(GroupCommandKind::Pause, "p1", secs(200), 0))
            .last(),
        Some(&seek(g, secs(200))),
        "only the broadcast of the viewer's own Pause"
    );
}

#[test]
fn a_release_before_the_restart_still_reaches_the_viewers_seek() {
    for delay_ms in [0, 500] {
        let (mut r, g) = playing(&["p1"], 100);
        pause(&mut r, g, true);
        moved(&mut r, g, secs(400));
        pause(&mut r, g, false);
        let mut actions = r.handle(command(GroupCommandKind::Pause, "p1", secs(100), 0));
        actions.extend(r.handle(command(
            GroupCommandKind::Unpause,
            "p1",
            secs(100),
            delay_ms,
        )));
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, ReceiverAction::Seek { .. })),
            "{actions:?}"
        );
        if delay_ms > 0 {
            actions = r.handle(ReceiverEvent::GroupResumeDue { generation: g });
        }
        assert!(
            actions.contains(&anchor(g, secs(400))),
            "anchored at the broadcast's 100, the corrector would seek back: {actions:?}"
        );
        assert_eq!(
            requests(&settle(&mut r, g, 400)),
            [&GroupRequest::Seek {
                position: secs(400)
            }]
        );
    }
}

#[test]
fn a_seek_stop_update_or_open_forgets_our_own_requests() {
    let partners_pause = command(GroupCommandKind::Pause, "p1", secs(50), 0);

    let (mut r, g) = playing(&["p1"], 10);
    pause(&mut r, g, true);
    r.handle(command(GroupCommandKind::Seek, "p1", secs(10), 0));
    assert_eq!(
        r.handle(partners_pause.clone()).last(),
        Some(&seek(g, secs(50)))
    );

    let (mut r, g) = playing(&["p1"], 10);
    pause(&mut r, g, true);
    r.handle(command(GroupCommandKind::Stop, "p1", secs(0), 0));
    assert_eq!(
        r.handle(partners_pause.clone()).last(),
        Some(&seek(g, secs(50)))
    );

    let (mut r, g) = playing(&["p1"], 10);
    pause(&mut r, g, true);
    r.handle(new_queue(200, &["p1"], 0, secs(10)));
    assert_eq!(
        r.handle(partners_pause.clone()).last(),
        Some(&seek(g, secs(50)))
    );

    let (mut r, g) = playing(&["p1"], 10);
    pause(&mut r, g, true);
    let g2 = load_pid(&mut r, "p1", 80);
    assert_ne!(g, g2);
    assert_eq!(r.handle(partners_pause).last(), Some(&seek(g2, secs(50))));
}

#[test]
fn a_group_stop_while_buffering_keeps_the_group() {
    let (mut r, g) = playing(&["p1"], 10);
    r.handle(buffering(g, true));
    r.handle(command(GroupCommandKind::Stop, "p1", secs(0), 0));
    r.handle(ended(g, EndReason::Stopped));
    assert!(r.handle(ReceiverEvent::PlayerIdle).is_empty());
    assert_eq!(r.group_id(), Some("g"));
}

#[test]
fn a_settle_away_from_our_seek_is_the_viewers() {
    let (mut r, g) = following("p1", 10);
    r.handle(command(GroupCommandKind::Seek, "p1", secs(40), 0));
    assert_eq!(
        requests(&settle(&mut r, g, 400)),
        [&GroupRequest::Seek {
            position: secs(400)
        }],
        "not the Ready, which is owed from the broadcast's own landing"
    );
    assert!(own_seek(&r), "the broadcast's seek is still to land");
    assert_eq!(
        requests(&settle(&mut r, g, 40)),
        [&GroupRequest::Ready {
            position: secs(40),
            is_playing: false,
            pid: pid("p1")
        }]
    );
    assert!(!own_seek(&r));
}

#[test]
fn a_settle_just_past_our_target_is_still_ours() {
    let (mut r, g) = playing(&["p1"], 10);
    r.handle(ReceiverEvent::DriftSeek {
        generation: g,
        position: secs(40),
    });
    let actions = r.handle(ReceiverEvent::PlayerPositionChanged {
        generation: g,
        position: ms(40_400),
        seek_completed: true,
    });
    assert!(requests(&actions).is_empty());
    assert!(!own_seek(&r));
}

#[test]
fn a_stale_opening_is_not_navigation() {
    let mut r = grouped();
    r.handle(new_queue(100, &["p1", "p2", "p3"], 1, secs(0)));
    r.handle(new_queue(200, &["p1", "p2", "p3"], 2, secs(0)));
    assert!(
        requests(&open_pid(&mut r, "p2", 70)).is_empty(),
        "the first load, reaching mpv after the second was asked for"
    );
    assert!(requests(&open_pid(&mut r, "p3", 80)).is_empty());
    assert_eq!(
        requests(&open_pid(&mut r, "p1", 90)),
        [&GroupRequest::SetPlaylistItem { pid: pid("p1") }],
        "that load arrived, so this one is the viewer's"
    );
}

#[test]
fn a_coming_load_replaces_the_open_file() {
    let (mut r, _) = following_in(&["p1", "p2"], 10);
    r.handle(new_queue(200, &["p1", "p2"], 1, secs(0)));
    assert!(
        r.handle(new_queue(300, &["p1", "p2"], 0, secs(0)))
            .contains(&ReceiverAction::GroupLoad {
                entries: entries(&["p1", "p2"]),
                index: 0
            }),
        "mpv is about to leave p1, so p1 has to be asked for again"
    );
}

#[test]
fn reopening_the_current_entry_is_held() {
    let (mut r, _) = playing(&["p1"], 10);
    assert!(requests(&open_pid(&mut r, "p1", 80)).is_empty());
    let g2 = gen_(2);
    resolve(&mut r, g2, "item-p1");
    r.handle(ReceiverEvent::PlayerFileLoaded { generation: g2 });
    assert!(only_progress(&pause(&mut r, g2, true)), "the hook's pause");
}

#[test]
fn foreign_file_leaves_the_group() {
    let (mut r, g) = following("p1", 10);
    let mut expected = vec![req(GroupRequest::Leave)];
    expected.extend(stop_following(g));
    assert_eq!(r.handle(ReceiverEvent::PlayerOpenedForeign), expected);
    assert_eq!(r.group_id(), None);
}

#[test]
fn outside_a_group_controls_are_unchanged() {
    let mut r = Receiver::new();
    let g = begin_playing(&mut r, "a", 70);
    assert!(only_progress(&settle(&mut r, g, 5)));
    assert!(only_progress(&pause(&mut r, g, true)));
    assert_eq!(
        r.handle(ReceiverEvent::RemoteResume),
        vec![set_paused(g, false)]
    );
    assert_eq!(
        r.handle(ReceiverEvent::RemoteTogglePause),
        vec![set_paused(g, false)]
    );
    assert_eq!(
        r.handle(ReceiverEvent::RemoteSeek(secs(9))),
        vec![seek(g, secs(9))]
    );
    assert_eq!(
        r.handle(ReceiverEvent::RemoteNext),
        vec![ReceiverAction::PlaylistNext]
    );
    assert_eq!(
        r.handle(ReceiverEvent::RemotePrevious),
        vec![ReceiverAction::PlaylistPrevious]
    );
    assert!(matches!(
        r.handle(remote_play(&["b"], QueueMode::Replace, 0))
            .as_slice(),
        [ReceiverAction::QueueItems(_)]
    ));
    assert!(r.handle(reached_end(g)).is_empty());
    assert!(r.handle(buffering(g, true)).is_empty());
    assert!(r.handle(ReceiverEvent::PlayerOpenedForeign).is_empty());
    assert!(
        r.handle(ReceiverEvent::DriftSeek {
            generation: g,
            position: secs(1)
        })
        .is_empty()
    );
    r.handle(ReceiverEvent::SegmentsResolved {
        generation: g,
        segments: vec![Segment {
            kind: SegmentKind::Intro,
            start: secs(0),
            end: secs(20),
            to_end_of_file: false,
        }],
    });
    assert!(matches!(
        r.handle(ReceiverEvent::SkipRequested).as_slice(),
        [
            ReceiverAction::HideSkipButton { .. },
            ReceiverAction::Seek { .. }
        ]
    ));
    assert_eq!(
        r.handle(ReceiverEvent::RemoteStop),
        vec![ReceiverAction::StopPlayer { generation: g }]
    );
}
