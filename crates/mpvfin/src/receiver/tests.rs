use jellyfin::{
    PlayMethod, PlaybackProgress, QueueRequest, RemoteCommand, ResolvedMedia, TrackMap,
};

use super::*;

pub(super) fn gen_(n: u64) -> Generation {
    Generation::new(n).unwrap()
}

pub(super) fn token(entry_id: i64, hook_id: u64) -> PlayerToken {
    PlayerToken { entry_id, hook_id }
}

fn queue(ids: &[&str], start_index: usize, secs: u64, selection: StreamSelection) -> QueueRequest {
    QueueRequest {
        item_ids: ids.iter().map(|s| (*s).to_owned()).collect(),
        mode: QueueMode::Replace,
        start_index,
        start_position: Duration::from_secs(secs),
        selection,
    }
}

fn media(item_id: &str) -> ResolvedMedia {
    ResolvedMedia {
        item_id: item_id.to_owned(),
        uri: format!("https://media.invalid/{item_id}"),
        title: format!("title-{item_id}"),
        play_session_id: format!("session-{item_id}"),
        media_source_id: format!("source-{item_id}"),
        play_method: PlayMethod::DirectPlay,
        external_subtitles: Vec::new(),
        tracks: TrackMap::default(),
        defaults: StreamSelection::default(),
    }
}

pub(super) fn open(r: &mut Receiver, item: &str, hook: u64) -> Vec<ReceiverAction> {
    r.handle(ReceiverEvent::PlayerOpening {
        token: token(1, hook),
        item_id: item.to_owned(),
        playlist_item_id: None,
    })
}

pub(super) fn resolve(r: &mut Receiver, g: Generation, item: &str) -> Vec<ReceiverAction> {
    r.handle(ReceiverEvent::MediaResolved {
        generation: g,
        media: media(item),
    })
}

pub(super) fn begin_playing(r: &mut Receiver, item: &str, hook: u64) -> Generation {
    let g = match open(r, item, hook).as_slice() {
        [ReceiverAction::ResolveMedia { generation, .. }] => *generation,
        other => panic!("unexpected actions: {other:?}"),
    };
    assert!(matches!(
        resolve(r, g, item).as_slice(),
        [ReceiverAction::ContinueLoad { .. }]
    ));
    assert!(matches!(
        r.handle(ReceiverEvent::PlayerFileLoaded { generation: g })
            .as_slice(),
        [ReceiverAction::ReportStarted(_)]
    ));
    g
}

pub(super) fn ended(g: Generation, reason: EndReason) -> ReceiverEvent {
    ReceiverEvent::PlayerEnded {
        generation: g,
        reason,
    }
}

#[test]
fn remote_play_queues_items_without_changing_state() {
    let mut r = Receiver::new();
    let req = queue(&["a", "b"], 1, 30, StreamSelection::default());
    assert_eq!(
        r.handle(ReceiverEvent::RemotePlay(req.clone())),
        vec![ReceiverAction::QueueItems(req)]
    );
    assert_eq!(r.playback(), &PlaybackState::Idle);
}

#[test]
fn opening_entry_starts_resolution_with_fresh_generation() {
    let mut r = Receiver::new();
    assert_eq!(
        open(&mut r, "a", 70),
        vec![ReceiverAction::ResolveMedia {
            generation: gen_(1),
            token: token(1, 70),
            item_id: "a".into()
        }]
    );
    r.handle(ended(gen_(1), EndReason::Stopped));
    assert!(matches!(
        open(&mut r, "b", 71).as_slice(),
        [ReceiverAction::ResolveMedia { generation, .. }] if *generation == gen_(2)
    ));
}

#[test]
fn resolution_continues_the_pending_hook_with_requested_selection() {
    let mut r = Receiver::new();
    let sel = StreamSelection {
        audio: Some(2),
        subtitle: Some(-1),
    };
    r.handle(ReceiverEvent::RemotePlay(queue(&["a", "b"], 0, 0, sel)));
    open(&mut r, "a", 70);
    let actions = resolve(&mut r, gen_(1), "a");
    assert!(matches!(
        actions.last(),
        Some(ReceiverAction::ContinueLoad { hook_id: 70, selection, .. }) if *selection == sel
    ));
    let g2 = match open(&mut r, "b", 80).last() {
        Some(ReceiverAction::ResolveMedia { generation, .. }) => *generation,
        other => panic!("unexpected actions: {other:?}"),
    };
    assert!(matches!(
        resolve(&mut r, g2, "b").last(),
        Some(ReceiverAction::ContinueLoad { hook_id: 80, selection, .. })
            if *selection == StreamSelection::default()
    ));
}

#[test]
fn play_is_reported_only_after_correlated_file_loaded() {
    let mut r = Receiver::new();
    open(&mut r, "a", 70);
    let actions = resolve(&mut r, gen_(1), "a");
    assert!(actions.iter().all(|a| !matches!(
        a,
        ReceiverAction::ReportStarted(_) | ReceiverAction::ReportProgress(_)
    )));
    assert!(matches!(r.playback(), PlaybackState::Loading { .. }));
    assert!(
        r.handle(ReceiverEvent::PlayerFileLoaded {
            generation: gen_(101)
        })
        .is_empty()
    );
    assert!(matches!(r.playback(), PlaybackState::Loading { .. }));
    let actions = r.handle(ReceiverEvent::PlayerFileLoaded {
        generation: gen_(1),
    });
    let [ReceiverAction::ReportStarted(s)] = actions.as_slice() else {
        panic!("unexpected actions: {actions:?}");
    };
    assert_eq!(s.position, Duration::ZERO);
    assert!(matches!(r.playback(), PlaybackState::Playing(_)));
}

#[test]
fn stale_resolution_is_ignored_after_next_entry_opens() {
    let mut r = Receiver::new();
    open(&mut r, "a", 70);
    let actions = open(&mut r, "b", 80);
    assert!(matches!(
        actions.as_slice(),
        [
            ReceiverAction::AbortLoad { hook_id: 70 },
            ReceiverAction::ResolveMedia { generation, .. }
        ] if *generation == gen_(2)
    ));
    assert!(resolve(&mut r, gen_(1), "a").is_empty());
}

#[test]
fn every_hook_is_continued_exactly_once() {
    fn count(actions: &[ReceiverAction], hook: u64) -> usize {
        actions
            .iter()
            .filter(|a| {
                matches!(a, ReceiverAction::ContinueLoad { hook_id, .. }
                    | ReceiverAction::AbortLoad { hook_id } if *hook_id == hook)
            })
            .count()
    }
    // resolved
    let mut r = Receiver::new();
    open(&mut r, "a", 70);
    let all = resolve(&mut r, gen_(1), "a");
    assert_eq!(count(&all, 70), 1);
    // resolution failed
    let mut r = Receiver::new();
    open(&mut r, "a", 70);
    let all = r.handle(ReceiverEvent::MediaResolutionFailed {
        generation: gen_(1),
        message: "boom".into(),
    });
    assert_eq!(count(&all, 70), 1);
    assert!(matches!(
        all.as_slice(),
        [
            ReceiverAction::AbortLoad { .. },
            ReceiverAction::PlaybackFailed { .. }
        ]
    ));
    assert_eq!(r.playback(), &PlaybackState::Idle);
    // superseded
    let mut r = Receiver::new();
    let mut all = open(&mut r, "a", 70);
    all.extend(open(&mut r, "b", 80));
    all.extend(resolve(&mut r, gen_(1), "a"));
    all.extend(resolve(&mut r, gen_(2), "b"));
    assert_eq!(count(&all, 70), 1);
    assert_eq!(count(&all, 80), 1);
    // shutdown while resolving
    let mut r = Receiver::new();
    let mut all = open(&mut r, "a", 70);
    all.extend(r.handle(ReceiverEvent::Shutdown));
    assert_eq!(count(&all, 70), 1);
}

#[test]
fn end_of_file_reports_stopped_once_without_stop_command() {
    let mut r = Receiver::new();
    let g = begin_playing(&mut r, "a", 70);
    let actions = r.handle(ended(g, EndReason::Eof));
    assert!(matches!(
        actions.as_slice(),
        [ReceiverAction::ReportStopped {
            reason: StopReason::Ended,
            ..
        }]
    ));
    assert!(r.handle(ended(g, EndReason::Eof)).is_empty());
}

#[test]
fn remote_stop_stops_player_then_reports_on_end() {
    let mut r = Receiver::new();
    let g = begin_playing(&mut r, "a", 70);
    assert_eq!(
        r.handle(ReceiverEvent::RemoteStop),
        vec![ReceiverAction::StopPlayer { generation: g }]
    );
    assert!(matches!(
        r.handle(ended(g, EndReason::Stopped)).as_slice(),
        [ReceiverAction::ReportStopped {
            reason: StopReason::Stopped,
            ..
        }]
    ));
}

fn begin_loading(r: &mut Receiver, item: &str, hook: u64) -> Generation {
    let g = match open(r, item, hook).as_slice() {
        [ReceiverAction::ResolveMedia { generation, .. }] => *generation,
        other => panic!("unexpected actions: {other:?}"),
    };
    resolve(r, g, item);
    g
}

#[test]
fn next_entry_while_loading_reports_nothing() {
    let mut r = Receiver::new();
    begin_loading(&mut r, "a", 70);
    assert!(matches!(
        open(&mut r, "b", 80).as_slice(),
        [ReceiverAction::ResolveMedia { token, .. }] if token.hook_id == 80
    ));
}

#[test]
fn shutdown_while_loading_reports_nothing() {
    let mut r = Receiver::new();
    begin_loading(&mut r, "a", 70);
    assert!(r.handle(ReceiverEvent::Shutdown).is_empty());
    assert_eq!(r.playback(), &PlaybackState::Idle);
}

#[test]
fn remote_stop_while_loading_stops_player_and_reports_nothing() {
    let mut r = Receiver::new();
    let g = begin_loading(&mut r, "a", 70);
    assert_eq!(
        r.handle(ReceiverEvent::RemoteStop),
        vec![ReceiverAction::StopPlayer { generation: g }]
    );
    assert!(r.handle(ended(g, EndReason::Stopped)).is_empty());
    assert_eq!(r.playback(), &PlaybackState::Idle);
}

#[test]
fn failed_end_while_loading_only_surfaces_the_error() {
    let mut r = Receiver::new();
    let g = begin_loading(&mut r, "a", 70);
    assert_eq!(
        r.handle(ended(g, EndReason::Failed("x".into()))),
        vec![ReceiverAction::PlaybackFailed {
            generation: g,
            item_id: "a".into(),
            message: "x".into()
        }]
    );
}

#[test]
fn remote_stop_while_resolving_aborts_hook_and_stops_player() {
    let mut r = Receiver::new();
    open(&mut r, "a", 70);
    assert_eq!(
        r.handle(ReceiverEvent::RemoteStop),
        vec![
            ReceiverAction::AbortLoad { hook_id: 70 },
            ReceiverAction::StopPlayer {
                generation: gen_(1)
            }
        ]
    );
    assert_eq!(r.playback(), &PlaybackState::Idle);
}

#[test]
fn failed_end_while_resolving_only_aborts_hook() {
    let mut r = Receiver::new();
    open(&mut r, "a", 70);
    assert_eq!(
        r.handle(ended(gen_(1), EndReason::Failed("x".into()))),
        vec![ReceiverAction::AbortLoad { hook_id: 70 }]
    );
    assert_eq!(r.playback(), &PlaybackState::Idle);
}

#[test]
fn opening_while_playing_reports_stopped_without_stop_command() {
    let mut r = Receiver::new();
    let g = begin_playing(&mut r, "a", 70);
    let actions = open(&mut r, "b", 80);
    assert!(matches!(
        actions.as_slice(),
        [
            ReceiverAction::ReportStopped { snapshot, reason: StopReason::Stopped },
            ReceiverAction::ResolveMedia { .. }
        ] if snapshot.generation == g
    ));
}

#[test]
fn player_failure_reports_failed_stop_and_surfaces_error() {
    let mut r = Receiver::new();
    let g = begin_playing(&mut r, "a", 70);
    let actions = r.handle(ended(g, EndReason::Failed("x".into())));
    assert!(matches!(
        actions.as_slice(),
        [
            ReceiverAction::ReportStopped { reason: StopReason::Failed, .. },
            ReceiverAction::PlaybackFailed { message, .. }
        ] if message == "x"
    ));
}

#[test]
fn toggles_invert_confirmed_state() {
    let mut r = Receiver::new();
    let g = begin_playing(&mut r, "a", 70);
    r.handle(ReceiverEvent::PlayerMuteChanged {
        generation: g,
        muted: true,
    });
    assert_eq!(
        r.handle(ReceiverEvent::RemoteTogglePause),
        vec![ReceiverAction::SetPaused {
            generation: g,
            paused: true
        }]
    );
    assert_eq!(
        r.handle(ReceiverEvent::RemoteToggleMute),
        vec![ReceiverAction::SetMuted {
            generation: g,
            muted: false
        }]
    );
}

#[test]
fn pause_control_matching_the_confirmed_state_is_a_no_op() {
    let mut r = Receiver::new();
    let g = begin_playing(&mut r, "a", 70);
    assert!(r.handle(ReceiverEvent::RemoteResume).is_empty());
    r.handle(ReceiverEvent::PlayerPauseChanged {
        generation: g,
        paused: true,
    });
    assert!(r.handle(ReceiverEvent::RemotePause).is_empty());
    assert_eq!(
        r.handle(ReceiverEvent::RemoteResume),
        vec![ReceiverAction::SetPaused {
            generation: g,
            paused: false
        }]
    );
}

#[test]
fn track_changes_are_reported_only_after_player_confirmation() {
    let mut r = Receiver::new();
    let g = begin_playing(&mut r, "a", 70);
    assert_eq!(
        r.handle(ReceiverEvent::RemoteSetAudioStream(2)),
        vec![ReceiverAction::SetAudioStream {
            generation: g,
            index: 2
        }]
    );
    let ev = ReceiverEvent::PlayerTracksChanged {
        generation: g,
        audio_stream_index: Some(2),
        subtitle_stream_index: Some(5),
    };
    let actions = r.handle(ev.clone());
    assert!(matches!(
        actions.as_slice(),
        [ReceiverAction::ReportProgress(s)]
            if s.audio_stream_index == Some(2) && s.subtitle_stream_index == Some(5)
    ));
    assert!(r.handle(ev).is_empty());
}

#[test]
fn stream_commands_and_playlist_commands() {
    let mut r = Receiver::new();
    assert!(
        r.handle(ReceiverEvent::RemoteSetSubtitleStream(1))
            .is_empty()
    );
    // Idle: a phone must not skip through a local playlist.
    assert!(r.handle(ReceiverEvent::RemoteNext).is_empty());
    assert!(r.handle(ReceiverEvent::RemotePrevious).is_empty());
    begin_playing(&mut r, "a", 70);
    assert_eq!(
        r.handle(ReceiverEvent::RemoteNext),
        vec![ReceiverAction::PlaylistNext]
    );
    assert_eq!(
        r.handle(ReceiverEvent::RemotePrevious),
        vec![ReceiverAction::PlaylistPrevious]
    );
}

#[test]
fn controller_changes_are_reported_only_after_player_confirmation() {
    let mut r = Receiver::new();
    let g = begin_playing(&mut r, "a", 70);
    assert!(matches!(
        r.handle(ReceiverEvent::RemotePause).as_slice(),
        [ReceiverAction::SetPaused { paused: true, .. }]
    ));
    assert!(matches!(
        r.handle(ReceiverEvent::PlayerPauseChanged {
            generation: g,
            paused: true
        })
        .as_slice(),
        [ReceiverAction::ReportProgress(s)] if s.paused
    ));
    assert!(matches!(
        r.handle(ReceiverEvent::RemoteSeek(Duration::from_secs(90)))
            .as_slice(),
        [ReceiverAction::Seek { .. }]
    ));
    assert!(
        r.handle(ReceiverEvent::PlayerPositionChanged {
            generation: g,
            position: Duration::from_secs(90),
            seek_completed: false,
        })
        .is_empty()
    );
    assert!(matches!(
        r.handle(ReceiverEvent::PlayerPositionChanged {
            generation: g,
            position: Duration::from_secs(90),
            seek_completed: true,
        })
        .as_slice(),
        [ReceiverAction::ReportProgress(s)] if s.position == Duration::from_secs(90)
    ));
}

#[test]
fn shutdown_reports_active_playback_without_stop_command() {
    let mut r = Receiver::new();
    begin_playing(&mut r, "a", 70);
    assert!(matches!(
        r.handle(ReceiverEvent::Shutdown).as_slice(),
        [ReceiverAction::ReportStopped {
            reason: StopReason::Shutdown,
            ..
        }]
    ));
}

fn seg(kind: SegmentKind, start: u64, end: u64, to_end_of_file: bool) -> Segment {
    Segment {
        kind,
        start: Duration::from_secs(start),
        end: Duration::from_secs(end),
        to_end_of_file,
    }
}

fn segments(r: &mut Receiver, g: Generation, list: Vec<Segment>) -> Vec<ReceiverAction> {
    r.handle(ReceiverEvent::SegmentsResolved {
        generation: g,
        segments: list,
    })
}

fn at(r: &mut Receiver, g: Generation, secs: u64) -> Vec<ReceiverAction> {
    r.handle(ReceiverEvent::PlayerPositionChanged {
        generation: g,
        position: Duration::from_secs(secs),
        seek_completed: false,
    })
}

/// mpv's first confirmed position for the item, which opens the button.
pub(super) fn settle(r: &mut Receiver, g: Generation, secs: u64) -> Vec<ReceiverAction> {
    r.handle(ReceiverEvent::PlayerPositionChanged {
        generation: g,
        position: Duration::from_secs(secs),
        seek_completed: true,
    })
}

fn show(g: Generation, kind: SegmentKind) -> ReceiverAction {
    ReceiverAction::ShowSkipButton {
        generation: g,
        kind,
    }
}

fn hide(g: Generation) -> ReceiverAction {
    ReceiverAction::HideSkipButton { generation: g }
}

fn playing_with(list: Vec<Segment>) -> (Receiver, Generation) {
    let mut r = Receiver::new();
    let g = begin_playing(&mut r, "a", 70);
    assert!(segments(&mut r, g, list).is_empty());
    settle(&mut r, g, 0);
    (r, g)
}

#[test]
fn no_button_before_the_first_settled_position() {
    let mut r = Receiver::new();
    let g = begin_playing(&mut r, "a", 70);
    assert!(at(&mut r, g, 0).is_empty());
    let list = vec![
        seg(SegmentKind::Recap, 0, 4, false),
        seg(SegmentKind::Intro, 5, 20, false),
    ];
    assert!(segments(&mut r, g, list).is_empty(), "inside the recap");
    assert!(at(&mut r, g, 8).is_empty(), "inside the intro");
    let actions = settle(&mut r, g, 8);
    assert!(
        matches!(actions.as_slice(), [ReceiverAction::ReportProgress(_), s] if *s == show(g, SegmentKind::Intro)),
        "{actions:?}"
    );
}

#[test]
fn the_next_item_must_settle_again() {
    let (mut r, g) = playing_with(vec![]);
    r.handle(ended(g, EndReason::Eof));
    let g2 = begin_playing(&mut r, "b", 80);
    assert!(segments(&mut r, g2, vec![seg(SegmentKind::Intro, 0, 20, false)]).is_empty());
    assert!(at(&mut r, g2, 8).is_empty());
}

#[test]
fn segment_button_shows_on_entry_and_hides_on_exit() {
    let (mut r, g) = playing_with(vec![seg(SegmentKind::Intro, 5, 20, false)]);
    assert!(at(&mut r, g, 4).is_empty());
    assert_eq!(at(&mut r, g, 5), vec![show(g, SegmentKind::Intro)]);
    assert!(at(&mut r, g, 10).is_empty());
    assert_eq!(at(&mut r, g, 20), vec![hide(g)]);
}

#[test]
fn segments_arriving_mid_segment_show_the_button_at_once() {
    let mut r = Receiver::new();
    let g = begin_playing(&mut r, "a", 70);
    settle(&mut r, g, 8);
    assert_eq!(
        segments(&mut r, g, vec![seg(SegmentKind::Intro, 5, 20, false)]),
        vec![show(g, SegmentKind::Intro)]
    );
}

#[test]
fn stale_segments_are_ignored() {
    let mut r = Receiver::new();
    let g = begin_playing(&mut r, "a", 70);
    settle(&mut r, g, 0);
    assert!(
        segments(
            &mut r,
            gen_(99),
            vec![seg(SegmentKind::Intro, 0, 20, false)]
        )
        .is_empty()
    );
    assert!(at(&mut r, g, 8).is_empty());
}

#[test]
fn skip_seeks_to_segment_end_and_hides() {
    let (mut r, g) = playing_with(vec![seg(SegmentKind::Intro, 5, 20, false)]);
    at(&mut r, g, 10);
    assert_eq!(
        r.handle(ReceiverEvent::SkipRequested),
        vec![
            hide(g),
            ReceiverAction::Seek {
                generation: g,
                position: Duration::from_secs(20)
            }
        ]
    );
}

#[test]
fn skipping_credits_marks_played_first() {
    let (mut r, g) = playing_with(vec![seg(SegmentKind::Outro, 90, 110, false)]);
    at(&mut r, g, 95);
    assert_eq!(
        r.handle(ReceiverEvent::SkipRequested),
        vec![
            ReceiverAction::MarkPlayed {
                generation: g,
                item_id: "a".into()
            },
            hide(g),
            ReceiverAction::Seek {
                generation: g,
                position: Duration::from_secs(110)
            }
        ]
    );
}

#[test]
fn skipping_a_segment_at_end_of_file_seeks_to_end() {
    let (mut r, g) = playing_with(vec![seg(SegmentKind::Preview, 90, 100, true)]);
    at(&mut r, g, 95);
    assert_eq!(
        r.handle(ReceiverEvent::SkipRequested),
        vec![hide(g), ReceiverAction::SeekToEnd { generation: g }]
    );
}

#[test]
fn dismissed_segment_stays_hidden_while_the_seek_lands() {
    let (mut r, g) = playing_with(vec![seg(SegmentKind::Intro, 5, 20, false)]);
    at(&mut r, g, 10);
    r.handle(ReceiverEvent::SkipRequested);
    assert!(at(&mut r, g, 12).is_empty());
}

#[test]
fn seeking_back_into_a_dismissed_segment_shows_it_again() {
    let (mut r, g) = playing_with(vec![seg(SegmentKind::Intro, 5, 20, false)]);
    at(&mut r, g, 10);
    r.handle(ReceiverEvent::SkipRequested);
    assert!(at(&mut r, g, 25).is_empty());
    assert_eq!(at(&mut r, g, 10), vec![show(g, SegmentKind::Intro)]);
}

#[test]
fn skipping_credits_lands_on_an_adjacent_segment_and_shows_its_button() {
    let (mut r, g) = playing_with(vec![
        seg(SegmentKind::Outro, 90, 110, false),
        seg(SegmentKind::Preview, 110, 120, false),
    ]);
    assert_eq!(
        at(&mut r, g, 95),
        vec![show(g, SegmentKind::Outro), show_next(g)]
    );
    assert_eq!(
        r.handle(ReceiverEvent::SkipRequested),
        vec![
            ReceiverAction::MarkPlayed {
                generation: g,
                item_id: "a".into()
            },
            hide(g),
            ReceiverAction::Seek {
                generation: g,
                position: Duration::from_secs(110)
            }
        ]
    );
    assert_eq!(at(&mut r, g, 110), vec![show(g, SegmentKind::Preview)]);
}

#[test]
fn adjacent_segments_swap_the_button() {
    let (mut r, g) = playing_with(vec![
        seg(SegmentKind::Outro, 90, 110, false),
        seg(SegmentKind::Preview, 110, 120, false),
    ]);
    assert_eq!(
        at(&mut r, g, 109),
        vec![show(g, SegmentKind::Outro), show_next(g)]
    );
    assert_eq!(
        at(&mut r, g, 111),
        vec![hide(g), show(g, SegmentKind::Preview)]
    );
}

#[test]
fn skip_outside_a_segment_does_nothing() {
    let (mut r, g) = playing_with(vec![seg(SegmentKind::Intro, 5, 20, false)]);
    at(&mut r, g, 30);
    assert!(r.handle(ReceiverEvent::SkipRequested).is_empty());
    let mut idle = Receiver::new();
    assert!(idle.handle(ReceiverEvent::SkipRequested).is_empty());
}

#[test]
fn item_end_hides_the_button() {
    let (mut r, g) = playing_with(vec![seg(SegmentKind::Intro, 90, 110, false)]);
    at(&mut r, g, 95);
    let actions = r.handle(ended(g, EndReason::Eof));
    assert!(matches!(
        actions.as_slice(),
        [
            ReceiverAction::HideSkipButton { .. },
            ReceiverAction::ReportStopped { .. }
        ]
    ));
    assert!(at(&mut r, g, 96).is_empty());
}

#[test]
fn next_item_starts_without_old_segments() {
    let (mut r, g) = playing_with(vec![seg(SegmentKind::Intro, 5, 20, false)]);
    at(&mut r, g, 10);
    r.handle(ended(g, EndReason::Eof));
    let g2 = begin_playing(&mut r, "b", 80);
    settle(&mut r, g2, 0);
    assert!(at(&mut r, g2, 10).is_empty());
}

fn show_next(g: Generation) -> ReceiverAction {
    ReceiverAction::ShowNextUp { generation: g }
}

fn hide_next(g: Generation) -> ReceiverAction {
    ReceiverAction::HideNextUp { generation: g }
}

fn length(r: &mut Receiver, g: Generation, secs: u64) -> Vec<ReceiverAction> {
    r.handle(ReceiverEvent::PlayerDurationChanged {
        generation: g,
        duration: Some(Duration::from_secs(secs)),
    })
}

fn played(g: Generation) -> ReceiverAction {
    ReceiverAction::MarkPlayed {
        generation: g,
        item_id: "a".into(),
    }
}

#[test]
fn next_up_comes_with_the_credits_and_outlasts_them() {
    let (mut r, g) = playing_with(vec![seg(SegmentKind::Outro, 90, 110, false)]);
    length(&mut r, g, 200);
    assert!(at(&mut r, g, 89).is_empty());
    assert_eq!(
        at(&mut r, g, 90),
        vec![show(g, SegmentKind::Outro), show_next(g)]
    );
    assert_eq!(
        at(&mut r, g, 110),
        vec![hide(g)],
        "only the skip button goes"
    );
    assert!(at(&mut r, g, 150).is_empty());
}

#[test]
fn next_up_without_credits_is_the_last_thirty_seconds() {
    let (mut r, g) = playing_with(vec![]);
    assert!(length(&mut r, g, 120).is_empty());
    assert!(at(&mut r, g, 89).is_empty());
    assert_eq!(at(&mut r, g, 90), vec![show_next(g)]);
}

#[test]
fn late_credits_are_not_preempted_by_the_last_thirty_seconds() {
    let (mut r, g) = playing_with(vec![seg(SegmentKind::Outro, 110, 120, true)]);
    length(&mut r, g, 120);
    assert!(at(&mut r, g, 100).is_empty());
}

#[test]
fn next_up_needs_credits_or_a_known_length() {
    let (mut r, g) = playing_with(vec![]);
    assert!(at(&mut r, g, 100_000).is_empty());
}

#[test]
fn learning_the_length_inside_the_window_shows_next_up() {
    let (mut r, g) = playing_with(vec![]);
    at(&mut r, g, 100);
    assert_eq!(length(&mut r, g, 120), vec![show_next(g)]);
}

#[test]
fn a_stale_length_is_ignored() {
    let (mut r, g) = playing_with(vec![]);
    at(&mut r, g, 100);
    assert!(length(&mut r, gen_(99), 120).is_empty());
}

#[test]
fn no_next_up_before_the_first_settled_position() {
    let mut r = Receiver::new();
    let g = begin_playing(&mut r, "a", 70);
    length(&mut r, g, 120);
    assert!(at(&mut r, g, 100).is_empty());
    assert_eq!(settle(&mut r, g, 100).last(), Some(&show_next(g)));
}

#[test]
fn seeking_out_of_the_window_hides_next_up() {
    let (mut r, g) = playing_with(vec![]);
    length(&mut r, g, 120);
    at(&mut r, g, 100);
    assert_eq!(at(&mut r, g, 50), vec![hide_next(g)]);
}

#[test]
fn play_now_marks_played_and_moves_to_the_next_entry() {
    let (mut r, g) = playing_with(vec![]);
    length(&mut r, g, 120);
    at(&mut r, g, 100);
    assert_eq!(
        r.handle(ReceiverEvent::NextUpRequested),
        vec![played(g), hide_next(g), ReceiverAction::PlaylistNext]
    );
}

#[test]
fn play_now_while_paused_starts_the_next_entry_playing() {
    let (mut r, g) = playing_with(vec![]);
    length(&mut r, g, 120);
    at(&mut r, g, 100);
    r.handle(ReceiverEvent::PlayerPauseChanged {
        generation: g,
        paused: true,
    });
    assert_eq!(
        r.handle(ReceiverEvent::NextUpRequested),
        vec![
            played(g),
            hide_next(g),
            ReceiverAction::SetPaused {
                generation: g,
                paused: false
            },
            ReceiverAction::PlaylistNext
        ]
    );
}

#[test]
fn play_now_without_the_card_does_nothing() {
    let (mut r, g) = playing_with(vec![]);
    length(&mut r, g, 120);
    at(&mut r, g, 10);
    assert!(r.handle(ReceiverEvent::NextUpRequested).is_empty());
    assert!(
        Receiver::new()
            .handle(ReceiverEvent::NextUpRequested)
            .is_empty()
    );
}

#[test]
fn a_pressed_card_stays_gone_until_playback_leaves_the_window() {
    let (mut r, g) = playing_with(vec![]);
    length(&mut r, g, 120);
    at(&mut r, g, 100);
    r.handle(ReceiverEvent::NextUpRequested);
    assert!(at(&mut r, g, 101).is_empty());
    assert!(r.handle(ReceiverEvent::NextUpRequested).is_empty());
    assert!(at(&mut r, g, 50).is_empty());
    assert_eq!(at(&mut r, g, 100), vec![show_next(g)]);
}

#[test]
fn item_end_hides_next_up_and_the_next_item_starts_clean() {
    let (mut r, g) = playing_with(vec![]);
    length(&mut r, g, 120);
    at(&mut r, g, 100);
    let actions = r.handle(ended(g, EndReason::Eof));
    assert!(
        matches!(
            actions.as_slice(),
            [
                ReceiverAction::HideNextUp { .. },
                ReceiverAction::ReportStopped { .. }
            ]
        ),
        "{actions:?}"
    );
    let g2 = begin_playing(&mut r, "b", 80);
    settle(&mut r, g2, 0);
    assert!(
        at(&mut r, g2, 100).is_empty(),
        "the old length is forgotten"
    );
}

#[test]
fn every_remote_command_reaches_its_event() {
    let request = QueueRequest {
        item_ids: vec!["a".into()],
        mode: QueueMode::Next,
        start_index: 0,
        start_position: Duration::ZERO,
        selection: StreamSelection::default(),
    };
    let at = Duration::from_secs(9);
    let cases = [
        (
            RemoteCommand::Play(request.clone()),
            ReceiverEvent::RemotePlay(request),
        ),
        (RemoteCommand::Pause, ReceiverEvent::RemotePause),
        (RemoteCommand::Resume, ReceiverEvent::RemoteResume),
        (RemoteCommand::TogglePause, ReceiverEvent::RemoteTogglePause),
        (RemoteCommand::Stop, ReceiverEvent::RemoteStop),
        (RemoteCommand::Next, ReceiverEvent::RemoteNext),
        (RemoteCommand::Previous, ReceiverEvent::RemotePrevious),
        (RemoteCommand::Seek(at), ReceiverEvent::RemoteSeek(at)),
        (
            RemoteCommand::SetVolume(7),
            ReceiverEvent::RemoteSetVolume(7),
        ),
        (
            RemoteCommand::SetMuted(true),
            ReceiverEvent::RemoteSetMuted(true),
        ),
        (RemoteCommand::ToggleMute, ReceiverEvent::RemoteToggleMute),
        (
            RemoteCommand::SetAudioStream(3),
            ReceiverEvent::RemoteSetAudioStream(3),
        ),
        (
            RemoteCommand::SetSubtitleStream(4),
            ReceiverEvent::RemoteSetSubtitleStream(4),
        ),
    ];
    for (command, event) in cases {
        assert_eq!(ReceiverEvent::from(command), event);
    }
}

#[test]
fn a_report_carries_every_field_of_the_snapshot() {
    let snapshot = PlaybackSnapshot {
        generation: gen_(1),
        item_id: "item".into(),
        play_session_id: "session".into(),
        media_source_id: "source".into(),
        play_method: PlayMethod::Transcode,
        position: Duration::from_secs(30),
        paused: true,
        volume_percent: 40,
        muted: true,
        audio_stream_index: Some(2),
        subtitle_stream_index: Some(5),
    };
    assert_eq!(
        PlaybackProgress::from(&snapshot),
        PlaybackProgress {
            item_id: "item".into(),
            play_session_id: "session".into(),
            media_source_id: "source".into(),
            play_method: PlayMethod::Transcode,
            position: Duration::from_secs(30),
            paused: true,
            volume_percent: 40,
            muted: true,
            audio_stream_index: Some(2),
            subtitle_stream_index: Some(5),
        }
    );
}
