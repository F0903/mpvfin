//! The group's playback commands (pause, unpause, seek and stop) and
//! following them.

use std::time::Duration;

use jellyfin::{GroupCommand, GroupCommandKind, GroupRequest, ZERO_ID};

use crate::receiver::group::POSITION_TOLERANCE;
use crate::receiver::{Generation, Receiver, ReceiverAction};

impl Receiver {
    pub(in crate::receiver) fn group_command(
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

    pub(in crate::receiver) fn group_resume_due(
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

    pub(in crate::receiver) fn group_drift_seek(
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
    pub(in crate::receiver) fn group_drift_lost(&self, actions: &mut Vec<ReceiverAction>) {
        if self.group.as_ref().is_some_and(|g| !g.expected_paused) {
            actions.push(ReceiverAction::Group(GroupRequest::Unpause));
        }
    }

    /// Stops correcting playback and forgets a scheduled start.
    pub(super) fn stop_following(&self, actions: &mut Vec<ReceiverAction>) {
        actions.push(ReceiverAction::CancelResume);
        // The newest generation, even idle: an anchor outlives the item it was set on.
        if let Some(generation) = Generation::new(self.next_generation - 1) {
            actions.push(ReceiverAction::DriftAnchor {
                generation,
                position: None,
            });
        }
    }

    pub(super) fn start_playing(
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

    pub(super) fn seek_unless_near(&mut self, target: Duration, actions: &mut Vec<ReceiverAction>) {
        if self
            .playing_position()
            .is_some_and(|position| position.abs_diff(target) > POSITION_TOLERANCE)
        {
            self.group_seek(target, actions);
        }
    }

    pub(super) fn group_seek(&mut self, position: Duration, actions: &mut Vec<ReceiverAction>) {
        if let (Some(generation), Some(g)) = (self.playing_generation(), self.group.as_mut()) {
            g.own_seek = Some(position);
            actions.push(ReceiverAction::Seek {
                generation,
                position,
            });
        }
    }
}
