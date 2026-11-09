//! The group's queue: mirroring it into mpv's playlist, and which of its
//! entries mpv has open.

use std::time::Duration;

use jellyfin::{GroupQueue, GroupRequest, QueueMode, QueueReason, QueueRequest};

use crate::receiver::group::{Group, POSITION_TOLERANCE};
use crate::receiver::{GroupLoadOptions, Receiver, ReceiverAction};

impl Receiver {
    pub(in crate::receiver) fn group_queue_changed(
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

    pub(in crate::receiver) fn group_remote_play(
        request: QueueRequest,
        actions: &mut Vec<ReceiverAction>,
    ) {
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
    pub(in crate::receiver) fn group_opening(&mut self, actions: &mut Vec<ReceiverAction>) {
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

    pub(in crate::receiver) fn group_load_options(&self) -> Option<GroupLoadOptions> {
        let g = self.group.as_ref()?;
        (self.opened_pid.is_some() && g.current_pid == self.opened_pid)
            .then_some(GroupLoadOptions { start: g.start })
    }

    /// The group, while mpv has its current entry open.
    pub(in crate::receiver) fn group_on_item(&mut self) -> Option<&mut Group> {
        let opened = self.opened_pid.as_ref()?;
        self.group
            .as_mut()
            .filter(|g| g.current_pid.as_ref() == Some(opened))
    }

    /// The queue entry mpv has open, if any.
    pub(super) fn loaded_pid(&self) -> Option<&str> {
        self.current_generation()?;
        self.opened_pid.as_deref()
    }
}
