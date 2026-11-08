//! Following a `SyncPlay` group: the rules of `docs/jellyfin-syncplay.md`,
//! sections 6 to 8, as reducer transitions.

use std::mem;
use std::time::Duration;

use crate::{
    Generation, GroupCommand, GroupCommandKind, GroupEntry, GroupLoadOptions, GroupQueue,
    GroupRequest, PlaybackState, QueueReason, Receiver, ReceiverAction, ZERO_ID,
};

/// Closer than this to a commanded position needs no seek.
const POSITION_TOLERANCE: Duration = Duration::from_millis(100);

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
    /// A seek issued for the group is on its way.
    #[allow(dead_code)] // read in Task 5
    own_seek: bool,
    /// Where the group starts: a load's start, or a scheduled resume's position.
    start: Duration,
    /// The load replaces a playing item with no Unpause to come.
    resume_on_settle: bool,
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
            own_seek: false,
            start: Duration::ZERO,
            resume_on_settle: false,
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
        self.group = Some(Group::new(group_id));
        if mem::take(&mut self.creating_group)
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
        g.awaiting_ready = !plays_on;
        g.resume_on_settle = plays_on;
        g.expected_paused = !queue.is_playing;
        g.start = queue.start_position;
        // A drift anchor from the old item would drag the new one to its clock.
        self.stop_following(actions);
        if self.loaded_pid() != Some(pid.as_str()) {
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
        let Some(g) = self.group.as_mut() else {
            return;
        };
        let zero = command.playlist_item_id == ZERO_ID;
        if !zero && g.current_pid.as_deref() != Some(command.playlist_item_id.as_str()) {
            return;
        }
        let position = command.position;
        match command.kind {
            GroupCommandKind::Stop => {
                if !zero && self.current_generation().is_some() {
                    self.stop_player(actions);
                    self.stop_following(actions);
                }
            }
            GroupCommandKind::Pause => {
                g.expected_paused = true;
                self.hold(actions);
                self.seek_unless_near(position, actions);
            }
            GroupCommandKind::Seek => {
                g.expected_paused = true;
                g.awaiting_ready = true;
                self.hold(actions);
                self.group_seek(position, actions);
            }
            GroupCommandKind::Unpause => {
                g.expected_paused = false;
                let delay = Duration::from_millis(command.delay_ms.unsigned_abs());
                if command.delay_ms > 0 {
                    // The anchor for later; the seek may not have landed by then.
                    g.start = position;
                    self.seek_unless_near(position, actions);
                    if let Some(generation) = self.playing_generation() {
                        actions.push(ReceiverAction::ScheduleResume {
                            generation,
                            after: delay,
                        });
                    }
                } else {
                    let target = position + delay;
                    self.seek_unless_near(target, actions);
                    if let Some(generation) = self.playing_generation() {
                        Self::start_playing(generation, target, actions);
                    }
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
        position: Duration,
        actions: &mut Vec<ReceiverAction>,
    ) {
        let Some(g) = self.group.as_mut() else {
            return;
        };
        // mpv is still on an item being replaced: this says nothing of the new one.
        if g.current_pid != self.opened_pid {
            return;
        }
        if g.awaiting_ready {
            self.send_ready(position, actions);
        } else if mem::take(&mut g.resume_on_settle)
            && !g.expected_paused
            && let Some(generation) = self.playing_generation()
        {
            // The hook loads every group entry paused.
            Self::start_playing(generation, position, actions);
        }
    }

    pub(super) fn group_load_options(&self) -> Option<GroupLoadOptions> {
        let g = self.group.as_ref()?;
        (self.opened_pid.is_some() && g.current_pid == self.opened_pid)
            .then_some(GroupLoadOptions { start: g.start })
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

    fn hold(&self, actions: &mut Vec<ReceiverAction>) {
        self.stop_following(actions);
        self.control_pause(true, actions);
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
            g.own_seek = true;
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
mod tests {
    use super::*;
    use crate::tests::{begin_playing, ended, gen_, open, resolve, settle, token};
    use crate::{EndReason, ReceiverEvent};

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

    fn new_queue(
        last_update_ms: i64,
        pids: &[&str],
        index: usize,
        start: Duration,
    ) -> ReceiverEvent {
        queue(
            QueueReason::NewPlaylist,
            last_update_ms,
            pids,
            Some(index),
            start,
            false,
        )
    }

    fn command(
        kind: GroupCommandKind,
        pid: &str,
        position: Duration,
        delay_ms: i64,
    ) -> ReceiverEvent {
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
        let mut r = grouped();
        r.handle(new_queue(100, &[pid], 0, secs(at)));
        let g = load_pid(&mut r, pid, 70);
        pause_state(&mut r, g, true);
        let actions = settle(&mut r, g, at);
        assert_eq!(actions.last(), Some(&ready(secs(at), pid)));
        (r, g)
    }

    fn own_seek(r: &Receiver) -> bool {
        r.group.as_ref().unwrap().own_seek
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
        assert!(
            r.handle(ReceiverEvent::GroupJoined {
                group_id: "g".into()
            })
            .is_empty(),
            "the create was consumed"
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
            matches!(
                settle(&mut r, g, 30).as_slice(),
                [ReceiverAction::ReportProgress(_)]
            ),
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
            matches!(
                settle(&mut r, g, 30).as_slice(),
                [ReceiverAction::ReportProgress(_)]
            ),
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
        assert!(
            r.handle(queue(
                QueueReason::NewPlaylist,
                100,
                &[],
                None,
                secs(0),
                false
            ))
            .is_empty()
        );
        assert!(
            r.handle(queue(
                QueueReason::NewPlaylist,
                100,
                &["p1"],
                Some(5),
                secs(0),
                false
            ))
            .is_empty(),
            "an index past the end names no entry"
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
        let g2 = load_pid(&mut r, "p2", 80);
        pause_state(&mut r, g2, true);
        let actions = settle(&mut r, g2, 0);
        assert!(
            matches!(
                actions.as_slice(),
                [ReceiverAction::ReportProgress(_), a, b]
                    if *a == set_paused(g2, false) && *b == anchor(g2, secs(0))
            ),
            "the hook loads every group entry paused, and no Unpause will follow: {actions:?}"
        );
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
        let (mut r, g) = following("p1", 10);
        pause_state(&mut r, g, false);
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
            vec![seek(g, secs(32)), set_paused(g, false), anchor(g, secs(32))]
        );

        let (mut r, g) = following("p1", 32);
        assert_eq!(
            r.handle(command(GroupCommandKind::Unpause, "p1", secs(30), -2000)),
            vec![set_paused(g, false), anchor(g, secs(32))]
        );

        let (mut r, g) = following("p1", 30);
        assert_eq!(
            r.handle(command(GroupCommandKind::Unpause, "p1", secs(30), 0)),
            vec![set_paused(g, false), anchor(g, secs(30))],
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
        open_pid(&mut r, "other", 70);
        assert!(matches!(
            resolve(&mut r, gen_(1), "item-other").as_slice(),
            [ReceiverAction::ContinueLoad { group: None, .. }]
        ));
    }
}
