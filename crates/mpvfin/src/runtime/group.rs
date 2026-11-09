//! Watch together in the event loop: the group's seeks and playlist, drift
//! correction, and the menu.

use std::time::{Duration, Instant};

use jellyfin::{GroupEntry, GroupRequest, GroupSummary};

use super::load::GROUP_KEEP_OPEN;
use super::playlist::{group_load_commands, published_queue};
use super::{MESSAGE_DURATION, PendingSeek, Runtime};
use crate::receiver::{Generation, ReceiverEvent};
use crate::runtime::values::parse_position;
use crate::server::session::Job;
use crate::watch_together::drift::DriftAction;
use crate::watch_together::menu;

/// How long a group seek waits for a newer one: a timeline drag is one seek
/// per step, and each broadcast would pull the playhead back.
pub(super) const GROUP_SEEK_DEBOUNCE: Duration = Duration::from_millis(250);

impl PendingSeek {
    /// Holds `position`, replacing any seek already held.
    pub(super) fn hold(&mut self, position: Duration, now: Instant) {
        self.0 = Some((now + GROUP_SEEK_DEBOUNCE, position));
    }

    pub(super) fn due_at(&self) -> Option<Instant> {
        self.0.map(|(at, _)| at)
    }

    /// The held seek, once.
    pub(super) fn flush(&mut self) -> Option<Duration> {
        self.0.take().map(|(_, position)| position)
    }
}

impl Runtime {
    pub(super) fn expire_resume(&mut self) {
        if let Some((at, generation)) = self.resume_at
            && Instant::now() >= at
        {
            self.resume_at = None;
            self.feed(ReceiverEvent::GroupResumeDue { generation });
        }
    }

    pub(super) fn expire_group_seek(&mut self) {
        if self
            .pending_seek
            .due_at()
            .is_some_and(|at| Instant::now() >= at)
        {
            self.flush_group_seek();
        }
    }

    pub(super) fn flush_group_seek(&mut self) {
        if let Some(position) = self.pending_seek.flush() {
            self.session
                .submit(Job::Group(GroupRequest::Seek { position }));
        }
    }

    /// Sends a request after any held seek, so it can't overtake that seek.
    pub(super) fn submit_group(&mut self, request: GroupRequest) {
        self.flush_group_seek();
        self.session.submit(Job::Group(request));
    }

    /// Not while a seek settles (`time-pos` is at the target before `playback-restart`,
    /// so a viewer's seek would read as drift), nor while paused or held at the end.
    pub(super) fn correct_drift(&mut self) {
        let Some(generation) = self
            .current_generation()
            .filter(|generation| Some(*generation) == self.drift_generation)
        else {
            return;
        };
        let o = &self.observed;
        if !self.seeking && !o.paused && !o.eof_reached {
            let action = self.drift.observe(o.position, o.speed, Instant::now());
            self.apply_drift(generation, action);
        }
    }

    pub(super) fn apply_drift(&mut self, generation: Generation, action: DriftAction) {
        match action {
            DriftAction::None => {}
            DriftAction::SetSpeed(speed) | DriftAction::RestoreSpeed(speed) => {
                self.set("file-local-options/speed", &speed.to_string());
            }
            DriftAction::Seek(position) => {
                // The group's clock can run past the end of the file.
                let duration = parse_position(self.mpv.get_property("duration").as_deref());
                self.feed(ReceiverEvent::DriftSeek {
                    generation,
                    position: position.min(duration.unwrap_or(position)),
                });
            }
            DriftAction::Resync => {
                // A running speed correction goes back now; the server's answer re-anchors.
                let restore = self.drift.anchor(None, Instant::now());
                self.apply_drift(generation, restore);
                self.feed(ReceiverEvent::DriftLost);
            }
        }
    }

    /// The group hook makes its entry wait at the end for the server. Out of
    /// the group nothing would move it on, so it gets the viewer's value back;
    /// rejoining with the same entry still open makes it wait again.
    pub(super) fn apply_group_keep_open(&self, grouped: bool) {
        let entry = self.current.and_then(|id| self.entries.get(&id));
        if let Some(before) = entry.and_then(|entry| entry.keep_open_before.as_deref()) {
            let value = if grouped { GROUP_KEEP_OPEN } else { before };
            self.set("file-local-options/keep-open", value);
        }
    }

    /// A press while a list is on its way does nothing, or two menus would open.
    pub(super) fn request_group_menu(&mut self) {
        if !self.group_list_pending {
            self.group_list_pending = true;
            self.session.submit(Job::ListGroups);
        }
    }

    pub(super) fn open_group_menu(&mut self, groups: &[GroupSummary]) {
        let Some(client) = self.mpv.client_name() else {
            self.mpv.log("mpv reported no client name: no menu");
            return;
        };
        let items = menu::menu_items(groups, self.receiver.group_id());
        let opened = if self.accepted(&menu::uosc_probe_command()) {
            self.accepted(&menu::uosc_open_command(&items, &client))
        } else {
            let legacy = self
                .mpv
                .get_property("mpv-version")
                .is_some_and(|version| menu::console_is_legacy(&version));
            self.accepted(&menu::console_open_command(
                &items,
                &client,
                menu::CONSOLE_HANDLER,
                legacy,
            ))
        };
        if opened {
            self.menu_items = Some(items);
        } else {
            self.mpv.show_text(
                "mpvfin: watch together needs uosc or mpv's console",
                MESSAGE_DURATION,
            );
        }
    }

    /// Taken, so a double activation can't act twice.
    pub(super) fn menu_chosen(&mut self, index: usize) {
        let chosen = self.menu_items.take();
        let Some(item) = chosen.and_then(|items| items.into_iter().nth(index)) else {
            return;
        };
        let grouped = self.receiver.group_id().is_some();
        let device_name = jellyfin::device_name(crate::server::APP.name);
        for event in menu::choice_events(item.choice, grouped, &device_name) {
            self.feed(event);
        }
    }

    /// If mpv refuses the entry no hook will ever fire, and the server would
    /// hold the whole group for a Ready that can't come.
    pub(super) fn group_load(&mut self, entries: &[GroupEntry], index: usize) {
        let commands = group_load_commands(entries, index);
        if !commands
            .first()
            .is_some_and(|replace| self.try_run(replace))
        {
            self.feed(ReceiverEvent::GroupLeave);
            self.mpv.show_text(
                "mpvfin: couldn't load that item, so you've left the group",
                MESSAGE_DURATION,
            );
            return;
        }
        for command in &commands[1..] {
            self.run(command);
        }
    }

    pub(super) fn publish_queue(&mut self, position: Duration) {
        let number = |name: &str| self.mpv.get_property(name)?.parse::<usize>().ok();
        // Unavailable entries stay as blanks, so positions stay meaningful.
        let filenames: Vec<String> = (0..number("playlist/count").unwrap_or(0))
            .map(|n| {
                self.mpv
                    .get_property(&format!("playlist/{n}/filename"))
                    .unwrap_or_default()
            })
            .collect();
        let current = number("playlist-playing-pos").unwrap_or(0);
        let (item_ids, index) = published_queue(&filenames, current);
        self.submit_group(GroupRequest::SetNewQueue {
            item_ids,
            index,
            start: position,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_newer_seek_replaces_the_held_one_and_restarts_its_wait() {
        let secs = Duration::from_secs;
        let t0 = Instant::now();
        let mut pending = PendingSeek::default();
        assert_eq!(pending.due_at(), None);

        pending.hold(secs(10), t0);
        assert_eq!(pending.due_at(), Some(t0 + GROUP_SEEK_DEBOUNCE));

        let later = t0 + Duration::from_millis(120);
        pending.hold(secs(20), later);
        assert_eq!(pending.due_at(), Some(later + GROUP_SEEK_DEBOUNCE));

        assert_eq!(pending.flush(), Some(secs(20)));
        assert_eq!(pending.flush(), None, "sent once");
        assert_eq!(pending.due_at(), None, "and no wake left behind");
    }
}
