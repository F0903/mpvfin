//! Deterministic playback/session state for mpvfin.
//!
//! The core deliberately performs no I/O. Adapters submit [`ReceiverEvent`]s
//! and execute the returned [`ReceiverAction`]s.
//!
//! Casts become `mpvfin://<itemId>` entries in mpv's own playlist. An mpv
//! load hook reports each entry as it starts ([`ReceiverEvent::PlayerOpening`]);
//! the reducer asks for Jellyfin negotiation and must continue every hook
//! exactly once ([`ReceiverAction::ContinueLoad`] or
//! [`ReceiverAction::AbortLoad`]).

mod action;
mod event;
mod group;
mod playback;
mod segments;

use std::mem;
use std::time::Duration;

use jellyfin::{GroupRequest, QueueMode, Segment, SegmentKind, StreamSelection};

use crate::receiver::segments::NextUpState;

pub use action::{GroupLoadOptions, ReceiverAction};
pub use event::ReceiverEvent;
pub use playback::{
    EndReason, Generation, PlaybackSnapshot, PlaybackState, PlayerToken, StopReason,
};

/// Deterministic owner of the receiver playback state.
#[derive(Debug)]
pub struct Receiver {
    next_generation: u64,
    playback: PlaybackState,
    /// Selection for the start item of the last `Replace`, consumed by the
    /// first matching `PlayerOpening`.
    pending_selection: Option<(String, StreamSelection)>,
    /// Segments of the current item, sorted by start.
    segments: Vec<Segment>,
    /// Segment whose skip button is showing.
    active: Option<Segment>,
    /// Segment the viewer skipped, hidden until the position leaves it.
    dismissed: Option<Segment>,
    /// Length of the current item, once mpv knows it.
    duration: Option<Duration>,
    next_up: NextUpState,
    /// Whether mpv confirmed a position for this item. Before that the position
    /// is the snapshot's zero, from before a requested start.
    position_settled: bool,
    /// The joined `SyncPlay` group.
    group: Option<group::Group>,
    /// A `GroupCreate` awaits its `GroupJoined`.
    creating_group: bool,
    /// Queue entry of the last `PlayerOpening`; meaningless while idle.
    opened_pid: Option<String>,
}

impl Default for Receiver {
    fn default() -> Self {
        Self::new()
    }
}

impl Receiver {
    /// Creates an idle receiver.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            next_generation: 1,
            playback: PlaybackState::Idle,
            pending_selection: None,
            segments: Vec::new(),
            active: None,
            dismissed: None,
            duration: None,
            next_up: NextUpState::Hidden,
            position_settled: false,
            group: None,
            creating_group: false,
            opened_pid: None,
        }
    }

    /// Returns the current playback state.
    #[must_use]
    #[cfg(test)]
    pub const fn playback(&self) -> &PlaybackState {
        &self.playback
    }

    /// Reduces one event and returns ordered side effects for adapters.
    // Keeping the full event table together makes transition coverage and
    // stale-generation checks auditable as one deterministic reducer.
    #[allow(clippy::too_many_lines)]
    pub fn handle(&mut self, event: ReceiverEvent) -> Vec<ReceiverAction> {
        let mut actions = Vec::new();

        match event {
            ReceiverEvent::RemoteNext
            | ReceiverEvent::RemotePrevious
            | ReceiverEvent::RemotePause
            | ReceiverEvent::RemoteResume
            | ReceiverEvent::RemoteTogglePause
            | ReceiverEvent::RemoteSeek(_)
            | ReceiverEvent::RemoteStop
                if self.group.is_some() =>
            {
                self.group_control(&event, &mut actions);
            }
            ReceiverEvent::RemotePlay(request) => {
                self.pending_selection = match request.mode {
                    QueueMode::Replace if request.selection != StreamSelection::default() => {
                        let last = request.item_ids.len().checked_sub(1);
                        last.map(|last| {
                            (
                                request.item_ids[request.start_index.min(last)].clone(),
                                request.selection,
                            )
                        })
                    }
                    _ => None,
                };
                if self.group.is_some() {
                    Self::group_remote_play(request, &mut actions);
                } else {
                    actions.push(ReceiverAction::QueueItems(request));
                }
            }
            ReceiverEvent::RemoteNext if self.current_generation().is_some() => {
                actions.push(ReceiverAction::PlaylistNext);
            }
            ReceiverEvent::RemotePrevious if self.current_generation().is_some() => {
                actions.push(ReceiverAction::PlaylistPrevious);
            }
            ReceiverEvent::RemoteNext | ReceiverEvent::RemotePrevious => {}
            ReceiverEvent::PlayerOpening {
                token,
                item_id,
                playlist_item_id,
            } => {
                self.terminate_current(StopReason::Stopped, &mut actions);
                self.opened_pid = playlist_item_id;
                let generation = self.allocate_generation();
                let selection = match self.pending_selection.take() {
                    Some((id, selection)) if id == item_id => selection,
                    other => {
                        self.pending_selection = other;
                        StreamSelection::default()
                    }
                };
                self.playback = PlaybackState::Resolving {
                    generation,
                    token,
                    item_id: item_id.clone(),
                    selection,
                };
                self.group_opening(&mut actions);
                actions.push(ReceiverAction::ResolveMedia {
                    generation,
                    token,
                    item_id,
                });
            }
            ReceiverEvent::MediaResolved { generation, media } => {
                match mem::take(&mut self.playback) {
                    PlaybackState::Resolving {
                        generation: expected,
                        token,
                        selection,
                        ..
                    } if expected == generation => {
                        self.playback = PlaybackState::Loading {
                            generation,
                            media: media.clone(),
                        };
                        actions.push(ReceiverAction::ContinueLoad {
                            generation,
                            hook_id: token.hook_id,
                            media,
                            selection,
                            group: self.group_load_options(),
                        });
                    }
                    other => self.playback = other,
                }
            }
            ReceiverEvent::MediaResolutionFailed {
                generation,
                message,
            } => match mem::take(&mut self.playback) {
                PlaybackState::Resolving {
                    generation: expected,
                    token,
                    item_id,
                    ..
                } if expected == generation => {
                    actions.push(ReceiverAction::AbortLoad {
                        hook_id: token.hook_id,
                    });
                    actions.push(ReceiverAction::PlaybackFailed {
                        generation,
                        item_id,
                        message,
                    });
                    self.group_item_failed(&mut actions);
                }
                other => self.playback = other,
            },
            ReceiverEvent::PlayerFileLoaded { generation } => match mem::take(&mut self.playback) {
                PlaybackState::Loading {
                    generation: expected,
                    media,
                } if expected == generation => {
                    let snapshot = Self::initial_snapshot(generation, &media);
                    self.playback = PlaybackState::Playing(snapshot.clone());
                    self.position_settled = false;
                    actions.push(ReceiverAction::ReportStarted(snapshot));
                }
                other => self.playback = other,
            },
            ReceiverEvent::PlayerPositionChanged {
                generation,
                position,
                seek_completed,
            } => {
                if let Some(snapshot) = self.playing_mut(generation) {
                    snapshot.position = position;
                    if seek_completed {
                        actions.push(ReceiverAction::ReportProgress(snapshot.clone()));
                        self.group_position_settled(generation, position, &mut actions);
                        self.position_settled = true;
                    }
                    self.evaluate_segments(&mut actions);
                }
            }
            ReceiverEvent::PlayerPauseChanged { generation, paused } => {
                if let Some(snapshot) = self.playing_mut(generation)
                    && snapshot.paused != paused
                {
                    snapshot.paused = paused;
                    actions.push(ReceiverAction::ReportProgress(snapshot.clone()));
                    self.group_pause_changed(paused, &mut actions);
                }
            }
            ReceiverEvent::PlayerVolumeChanged {
                generation,
                volume_percent,
            } => {
                if let Some(snapshot) = self.playing_mut(generation) {
                    let volume_percent = volume_percent.min(100);
                    if snapshot.volume_percent != volume_percent {
                        snapshot.volume_percent = volume_percent;
                        actions.push(ReceiverAction::ReportProgress(snapshot.clone()));
                    }
                }
            }
            ReceiverEvent::PlayerMuteChanged { generation, muted } => {
                if let Some(snapshot) = self.playing_mut(generation)
                    && snapshot.muted != muted
                {
                    snapshot.muted = muted;
                    actions.push(ReceiverAction::ReportProgress(snapshot.clone()));
                }
            }
            ReceiverEvent::PlayerTracksChanged {
                generation,
                audio_stream_index,
                subtitle_stream_index,
            } => {
                if let Some(snapshot) = self.playing_mut(generation)
                    && (snapshot.audio_stream_index != audio_stream_index
                        || snapshot.subtitle_stream_index != subtitle_stream_index)
                {
                    snapshot.audio_stream_index = audio_stream_index;
                    snapshot.subtitle_stream_index = subtitle_stream_index;
                    actions.push(ReceiverAction::ReportProgress(snapshot.clone()));
                }
            }
            ReceiverEvent::PlayerEnded { generation, reason } => {
                if self.current_generation() == Some(generation) {
                    let stop = match &reason {
                        EndReason::Eof => StopReason::Ended,
                        EndReason::Stopped => StopReason::Stopped,
                        EndReason::Failed(_) => StopReason::Failed,
                    };
                    let was_resolving = matches!(self.playback, PlaybackState::Resolving { .. });
                    let item_id = self.current_item_id();
                    self.terminate_current(stop, &mut actions);
                    if let (EndReason::Failed(message), Some(item_id), false) =
                        (reason, item_id, was_resolving)
                    {
                        actions.push(ReceiverAction::PlaybackFailed {
                            generation,
                            item_id,
                            message,
                        });
                    }
                    match stop {
                        StopReason::Stopped => self.group_load_stopped(),
                        StopReason::Failed => self.group_item_failed(&mut actions),
                        StopReason::Ended | StopReason::Shutdown => {
                            self.group_load_ended(&mut actions);
                        }
                    }
                }
            }
            ReceiverEvent::RemotePause => self.control_pause(true, &mut actions),
            ReceiverEvent::RemoteResume => self.control_pause(false, &mut actions),
            ReceiverEvent::RemoteTogglePause => {
                if let PlaybackState::Playing(snapshot) = &self.playback {
                    let paused = !snapshot.paused;
                    self.control_pause(paused, &mut actions);
                }
            }
            ReceiverEvent::RemoteSeek(position) => {
                if let Some(generation) = self.playing_generation() {
                    actions.push(ReceiverAction::Seek {
                        generation,
                        position,
                    });
                }
            }
            ReceiverEvent::RemoteStop => self.stop_player(&mut actions),
            ReceiverEvent::RemoteSetVolume(volume_percent) => {
                if let Some(generation) = self.playing_generation() {
                    actions.push(ReceiverAction::SetVolume {
                        generation,
                        volume_percent: volume_percent.min(100),
                    });
                }
            }
            ReceiverEvent::RemoteSetMuted(muted) => {
                if let Some(generation) = self.playing_generation() {
                    actions.push(ReceiverAction::SetMuted { generation, muted });
                }
            }
            ReceiverEvent::RemoteToggleMute => {
                if let PlaybackState::Playing(snapshot) = &self.playback {
                    actions.push(ReceiverAction::SetMuted {
                        generation: snapshot.generation,
                        muted: !snapshot.muted,
                    });
                }
            }
            ReceiverEvent::RemoteSetAudioStream(index) => {
                if let Some(generation) = self.playing_generation() {
                    actions.push(ReceiverAction::SetAudioStream { generation, index });
                }
            }
            ReceiverEvent::RemoteSetSubtitleStream(index) => {
                if let Some(generation) = self.playing_generation() {
                    actions.push(ReceiverAction::SetSubtitleStream { generation, index });
                }
            }
            ReceiverEvent::SegmentsResolved {
                generation,
                segments,
            } => {
                if matches!(
                    self.playback,
                    PlaybackState::Loading { .. } | PlaybackState::Playing(_)
                ) && self.current_generation() == Some(generation)
                {
                    self.segments = segments;
                    self.evaluate_segments(&mut actions);
                }
            }
            ReceiverEvent::SkipRequested => {
                if let PlaybackState::Playing(snapshot) = &self.playback
                    && let Some(segment) = self.active.take()
                {
                    let generation = snapshot.generation;
                    if segment.kind == SegmentKind::Outro {
                        actions.push(ReceiverAction::MarkPlayed {
                            generation,
                            item_id: snapshot.item_id.clone(),
                        });
                    }
                    actions.push(ReceiverAction::HideSkipButton { generation });
                    match (self.group_on_item().is_some(), segment.to_end_of_file) {
                        (true, true) => self.group_reached_end(generation, &mut actions),
                        (true, false) => actions.push(ReceiverAction::Group(GroupRequest::Seek {
                            position: segment.end,
                        })),
                        (false, true) => actions.push(ReceiverAction::SeekToEnd { generation }),
                        (false, false) => actions.push(ReceiverAction::Seek {
                            generation,
                            position: segment.end,
                        }),
                    }
                    self.dismissed = Some(segment);
                }
            }
            ReceiverEvent::PlayerDurationChanged {
                generation,
                duration,
            } => {
                if self.playing_mut(generation).is_some() {
                    self.duration = duration;
                    self.evaluate_segments(&mut actions);
                }
            }
            ReceiverEvent::NextUpRequested => {
                if self.next_up == NextUpState::Shown
                    && let PlaybackState::Playing(snapshot) = &self.playback
                {
                    let generation = snapshot.generation;
                    actions.push(ReceiverAction::MarkPlayed {
                        generation,
                        item_id: snapshot.item_id.clone(),
                    });
                    actions.push(ReceiverAction::HideNextUp { generation });
                    self.next_up = NextUpState::Taken;
                    // mpv would open the next entry paused; a group's pause is the group's.
                    if self.group.is_none() {
                        self.control_pause(false, &mut actions);
                    }
                    actions.extend(self.handle(ReceiverEvent::RemoteNext));
                }
            }
            ReceiverEvent::PlayerReachedEnd { generation } => {
                self.group_reached_end(generation, &mut actions);
            }
            ReceiverEvent::PlayerStoppedEarly { generation } => {
                self.group_stopped_early(generation, &mut actions);
            }
            ReceiverEvent::PlayerBuffering {
                generation,
                buffering,
            } => self.group_buffering(generation, buffering, &mut actions),
            ReceiverEvent::PlayerOpenedForeign => self.group_opened_foreign(&mut actions),
            ReceiverEvent::PlayerIdle => self.group_idle(&mut actions),
            ReceiverEvent::DriftSeek {
                generation,
                position,
            } => self.group_drift_seek(generation, position, &mut actions),
            ReceiverEvent::DriftLost => self.group_drift_lost(&mut actions),
            ReceiverEvent::GroupCreate { name } => self.group_create(name, &mut actions),
            ReceiverEvent::GroupJoin { group_id } => self.group_join(group_id, &mut actions),
            ReceiverEvent::GroupLeave => self.group_leave(&mut actions),
            ReceiverEvent::GroupJoined { group_id } => self.group_joined(group_id, &mut actions),
            ReceiverEvent::GroupLeft => self.drop_group(&mut actions),
            ReceiverEvent::GroupGone { message } => self.group_gone(message, &mut actions),
            ReceiverEvent::GroupQueueChanged(queue) => {
                self.group_queue_changed(queue, &mut actions);
            }
            ReceiverEvent::GroupCommandReceived(command) => {
                self.group_command(&command, &mut actions);
            }
            ReceiverEvent::GroupResumeDue { generation } => {
                self.group_resume_due(generation, &mut actions);
            }
            ReceiverEvent::ProgressTick => {
                if let PlaybackState::Playing(snapshot) = &self.playback {
                    actions.push(ReceiverAction::ReportProgress(snapshot.clone()));
                }
            }
            ReceiverEvent::Shutdown => {
                self.terminate_current(StopReason::Shutdown, &mut actions);
            }
        }

        actions
    }
}

#[cfg(test)]
mod tests;
