//! What this viewer's mpv does while grouped, as requests and reports to the
//! group: settling, pausing, transport controls, stalls and ends.

use std::mem;
use std::time::Duration;

use jellyfin::{GroupEntry, GroupRequest, ZERO_ID};

use crate::receiver::{Generation, PlaybackState, Receiver, ReceiverAction, ReceiverEvent};

/// A settle this close to a seek of ours is that seek: the server's own
/// tolerance for a Ready, and mpv can read a frame or two past its target.
pub(super) const SEEK_LANDING_TOLERANCE: Duration = Duration::from_millis(500);

impl Receiver {
    /// mpv finished a load or seek on the current generation.
    pub(in crate::receiver) fn group_position_settled(
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
    pub(in crate::receiver) fn group_pause_changed(
        &mut self,
        paused: bool,
        actions: &mut Vec<ReceiverAction>,
    ) {
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
    pub(in crate::receiver) fn group_control(
        &self,
        event: &ReceiverEvent,
        actions: &mut Vec<ReceiverAction>,
    ) {
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

    /// Once per item: the server has no end-of-file notion of its own.
    pub(in crate::receiver) fn group_reached_end(
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
    pub(in crate::receiver) fn group_stopped_early(
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

    pub(in crate::receiver) fn group_buffering(
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

    pub(in crate::receiver) fn group_opened_foreign(&mut self, actions: &mut Vec<ReceiverAction>) {
        if self.group.is_some() {
            self.group_leave(actions);
        }
    }

    /// The server has no timeout: an item it waits on that ended would hang
    /// the group. An end of the item being replaced says nothing of the wait.
    pub(in crate::receiver) fn group_load_ended(&mut self, actions: &mut Vec<ReceiverAction>) {
        if self.server_waits_on_open_item() {
            self.abandon_group(actions);
        }
    }

    /// A failed entry abandons even with no wait on it: mpv would auto-advance
    /// to the next entry, and that reads as the viewer picking it for everyone.
    pub(in crate::receiver) fn group_item_failed(&mut self, actions: &mut Vec<ReceiverAction>) {
        if self.group_on_item().is_some() {
            self.abandon_group(actions);
        }
    }

    /// mpv ends an entry with a stop before it opens the next one, so the
    /// decision waits for an opening, or for mpv going idle.
    pub(in crate::receiver) fn group_load_stopped(&mut self) {
        let waited_on = self.server_waits_on_open_item();
        if let Some(g) = self.group.as_mut() {
            g.stopped_owing = waited_on;
        }
    }

    pub(in crate::receiver) fn group_idle(&mut self, actions: &mut Vec<ReceiverAction>) {
        if self.group.as_ref().is_some_and(|g| g.stopped_owing) {
            self.abandon_group(actions);
        }
    }

    pub(super) fn server_waits_on_open_item(&self) -> bool {
        self.group
            .as_ref()
            .is_some_and(|g| (g.awaiting_ready || g.buffering) && g.current_pid == self.opened_pid)
    }

    /// mpv's `stop` also clears the mirrored queue, which it would play on through alone.
    pub(super) fn abandon_group(&mut self, actions: &mut Vec<ReceiverAction>) {
        self.group_leave(actions);
        if let Some(generation) = Generation::new(self.next_generation - 1) {
            actions.push(ReceiverAction::StopPlayer { generation });
        }
        actions.push(ReceiverAction::ShowMessage {
            text: "mpvfin: couldn't load that item, so you've left the group".to_owned(),
        });
    }

    pub(super) fn playing_position(&self) -> Option<Duration> {
        match &self.playback {
            PlaybackState::Playing(snapshot) => Some(snapshot.position),
            _ => None,
        }
    }

    /// Reports the loaded item ready; the zero id is never a real entry.
    pub(super) fn send_ready(&mut self, position: Duration, actions: &mut Vec<ReceiverAction>) {
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
