//! Carrying out what the reducer asks for.

use std::time::{Duration, Instant};

use jellyfin::GroupRequest;

use super::load::{TrackKind, group_hook_properties, hook_properties, seek_command};
use super::playlist::{group_sync_commands, queue_commands};
use super::{
    Entry, GROUP_RESOLVE_DEADLINE, MESSAGE_DURATION, NextUp, PendingResolve, PendingSeek,
    RESOLVE_DEADLINE, Runtime,
};
use crate::receiver::{ReceiverAction, StopReason};
use crate::runtime::values::yes_no;
use crate::server::session::{Job, Report};

impl Runtime {
    // One arm per action keeps the adapter auditable against the reducer.
    #[allow(clippy::too_many_lines)]
    pub(super) fn execute(&mut self, action: ReceiverAction) {
        match action {
            ReceiverAction::QueueItems(request) => {
                for command in queue_commands(&request) {
                    self.run(&command);
                }
            }
            ReceiverAction::PlaylistNext => self.run(&["playlist-next"]),
            ReceiverAction::PlaylistPrevious => self.run(&["playlist-prev"]),
            ReceiverAction::ResolveMedia {
                generation,
                token,
                item_id,
            } => {
                let entry = Entry {
                    generation,
                    media: None,
                    keep_open_before: None,
                };
                self.entries.insert(token.entry_id, entry);
                let allowed = if self.receiver.group_id().is_some() {
                    GROUP_RESOLVE_DEADLINE
                } else {
                    RESOLVE_DEADLINE
                };
                self.pending_resolve = Some(PendingResolve {
                    generation,
                    deadline: Instant::now() + allowed,
                    expired: false,
                });
                self.session.submit(Job::Resolve {
                    generation,
                    item_id,
                });
            }
            ReceiverAction::ContinueLoad {
                generation,
                hook_id,
                media,
                selection,
                group,
            } => {
                let keep_open_before = group
                    .is_some()
                    .then(|| self.mpv.get_property("keep-open"))
                    .flatten();
                let group_properties = group
                    .map(|options| group_hook_properties(options.start))
                    .unwrap_or_default();
                for (name, value) in hook_properties(&media, &self.watch_later_dir)
                    .into_iter()
                    .chain(group_properties)
                {
                    self.set(name, &value);
                }
                if let Some(entry) = self
                    .entries
                    .values_mut()
                    .find(|e| e.generation == generation)
                {
                    entry.media = Some((media, selection));
                    entry.keep_open_before = keep_open_before;
                }
                self.mpv.hook_continue(hook_id);
            }
            ReceiverAction::AbortLoad { hook_id } => self.mpv.hook_continue(hook_id),
            ReceiverAction::SetPaused { paused: true, .. } => {
                self.set("file-local-options/pause", "yes");
            }
            // Plain: a file-local `no` would restore a local pause when the item ends.
            ReceiverAction::SetPaused { paused: false, .. } => self.set("pause", "no"),
            ReceiverAction::Seek { position, .. } => {
                self.run(&seek_command(position));
            }
            ReceiverAction::SetVolume { volume_percent, .. } => {
                self.set("volume", &volume_percent.to_string());
            }
            ReceiverAction::SetMuted { muted, .. } => self.set("mute", yes_no(muted)),
            ReceiverAction::SetAudioStream { generation, index } => {
                self.select_track(generation, TrackKind::Audio, index);
            }
            ReceiverAction::SetSubtitleStream { generation, index } => {
                self.select_track(generation, TrackKind::Subtitle, index);
            }
            ReceiverAction::StopPlayer { .. } => self.run(&["stop"]),
            ReceiverAction::ReportStarted(snapshot) => {
                self.session.submit(Job::Report(Report::Started(snapshot)));
            }
            ReceiverAction::ReportProgress(snapshot) => {
                self.session.submit(Job::Report(Report::Progress(snapshot)));
            }
            ReceiverAction::ReportStopped { snapshot, reason } => {
                let failed = reason == StopReason::Failed;
                self.session
                    .submit(Job::Report(Report::Stopped { snapshot, failed }));
            }
            ReceiverAction::ShowSkipButton { kind, .. } => self.show_skip_button(kind),
            ReceiverAction::HideSkipButton { .. } => self.hide_skip_button(),
            ReceiverAction::ShowNextUp { .. } => {
                self.next_up = Some(NextUp::default());
                self.refresh_next_up();
            }
            ReceiverAction::HideNextUp { .. } => {
                if self.next_up_on_screen() {
                    self.remove_next_up_card();
                }
                self.next_up = None;
            }
            ReceiverAction::SeekToEnd { .. } => {
                self.run(&["seek", "100", "absolute-percent+exact"]);
                // mpv holds a paused last frame; only unpausing ends the item.
                if self.observed.paused {
                    self.set("pause", "no");
                }
            }
            ReceiverAction::MarkPlayed { item_id, .. } => {
                self.session.submit(Job::MarkPlayed { item_id });
            }
            ReceiverAction::PlaybackFailed { message, .. } => {
                self.mpv
                    .show_text(&format!("mpvfin: {message}"), Duration::from_secs(5));
            }
            ReceiverAction::Group(GroupRequest::Seek { position }) => {
                self.pending_seek.hold(position, Instant::now());
            }
            ReceiverAction::Group(request) => self.submit_group(request),
            ReceiverAction::GroupLoad { entries, index } => {
                // A held seek was for the item being replaced.
                self.pending_seek = PendingSeek::default();
                self.group_load(&entries, index);
            }
            ReceiverAction::GroupSyncPlaylist { entries, index } => {
                for command in group_sync_commands(&entries, index) {
                    self.run(&command);
                }
            }
            ReceiverAction::GroupPublishQueue { position } => self.publish_queue(position),
            ReceiverAction::ScheduleResume { generation, after } => {
                self.resume_at = Some((Instant::now() + after, generation));
            }
            ReceiverAction::CancelResume => self.resume_at = None,
            ReceiverAction::DriftAnchor {
                generation,
                position,
            } => {
                self.drift_generation = position.map(|_| generation);
                let action = self.drift.anchor(position, Instant::now());
                self.apply_drift(generation, action);
            }
            ReceiverAction::ShowMessage { text } => self.mpv.show_text(&text, MESSAGE_DURATION),
        }
    }
}
