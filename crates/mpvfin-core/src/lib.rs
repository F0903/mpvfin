//! Deterministic playback/session state for mpvfin.
//!
//! The core deliberately performs no I/O. Adapters submit [`ReceiverEvent`]s
//! and execute the returned [`ReceiverAction`]s.

use std::mem;
use std::time::Duration;

/// Identifies one playback attempt.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Generation(u64);

impl Generation {
    /// Creates a generation from a non-zero wire or adapter value.
    ///
    /// The receiver itself allocates generations monotonically. This
    /// constructor is intended for adapters restoring an already-issued ID.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// Returns the underlying monotonically increasing value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A remote request to start one Jellyfin item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlayRequest {
    /// Jellyfin item identifier.
    pub item_id: String,
    /// Requested starting position.
    pub start_position: Duration,
}

/// How Jellyfin decided to deliver the media.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlayMethod {
    /// The server file can be played without conversion.
    DirectPlay,
    /// The server repackages the source without re-encoding it.
    DirectStream,
    /// The server transcodes at least one stream.
    Transcode,
}

/// A successful result from Jellyfin playback negotiation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedMedia {
    /// Jellyfin item identifier.
    pub item_id: String,
    /// URL or local path passed to mpv.
    pub uri: String,
    /// Jellyfin playback-session identifier.
    pub play_session_id: String,
    /// Selected Jellyfin media-source identifier.
    pub media_source_id: String,
    /// Delivery method selected by Jellyfin.
    pub play_method: PlayMethod,
}

/// A stable view used for Jellyfin lifecycle reports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaybackSnapshot {
    /// Playback generation.
    pub generation: Generation,
    /// Jellyfin item identifier.
    pub item_id: String,
    /// Jellyfin playback-session identifier.
    pub play_session_id: String,
    /// Selected Jellyfin media-source identifier.
    pub media_source_id: String,
    /// Delivery method selected by Jellyfin.
    pub play_method: PlayMethod,
    /// Last player-confirmed position.
    pub position: Duration,
    /// Last player-confirmed pause state.
    pub paused: bool,
    /// Last player-confirmed volume, from 0 through 100.
    pub volume_percent: u8,
    /// Last player-confirmed mute state.
    pub muted: bool,
}

/// Why an active or resolved playback was terminated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
    /// A remote controller requested stop.
    Remote,
    /// A newer play request replaced this playback.
    Replaced,
    /// mpv reached the natural end of the item.
    Ended,
    /// mpv failed while loading or playing the item.
    Failed,
    /// The receiver is shutting down.
    Shutdown,
}

/// Current receiver playback state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum PlaybackState {
    /// No item is being resolved or played.
    #[default]
    Idle,
    /// Jellyfin playback negotiation is in progress.
    Resolving {
        /// Playback generation.
        generation: Generation,
        /// Original remote request.
        request: PlayRequest,
    },
    /// mpv was told to load a negotiated item and has not confirmed it yet.
    Loading {
        /// Playback generation.
        generation: Generation,
        /// Negotiated media.
        media: ResolvedMedia,
        /// Requested starting position.
        start_position: Duration,
    },
    /// mpv confirmed that the negotiated item loaded.
    Playing(PlaybackSnapshot),
}

/// An input to the receiver reducer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReceiverEvent {
    /// A controller requested a new item.
    RemotePlay(PlayRequest),
    /// Jellyfin playback negotiation succeeded.
    MediaResolved {
        /// Generation attached to the request.
        generation: Generation,
        /// Negotiated media.
        media: ResolvedMedia,
    },
    /// Jellyfin playback negotiation failed.
    MediaResolutionFailed {
        /// Generation attached to the request.
        generation: Generation,
        /// Human-readable error without credentials or playback URLs.
        message: String,
    },
    /// mpv emitted `file-loaded` for this generation.
    PlayerFileLoaded {
        /// Playback generation.
        generation: Generation,
    },
    /// mpv failed to load or continue this generation.
    PlayerFailed {
        /// Playback generation.
        generation: Generation,
        /// Human-readable error without credentials or playback URLs.
        message: String,
    },
    /// mpv confirmed a new playback position.
    PlayerPositionChanged {
        /// Playback generation.
        generation: Generation,
        /// Confirmed position.
        position: Duration,
        /// Whether the change completes a requested seek.
        seek_completed: bool,
    },
    /// mpv confirmed a pause-state change.
    PlayerPauseChanged {
        /// Playback generation.
        generation: Generation,
        /// Confirmed state.
        paused: bool,
    },
    /// mpv confirmed a volume change.
    PlayerVolumeChanged {
        /// Playback generation.
        generation: Generation,
        /// Confirmed volume, from 0 through 100.
        volume_percent: u8,
    },
    /// mpv confirmed a mute-state change.
    PlayerMuteChanged {
        /// Playback generation.
        generation: Generation,
        /// Confirmed state.
        muted: bool,
    },
    /// mpv reached the natural end of the item.
    PlayerEnded {
        /// Playback generation.
        generation: Generation,
    },
    /// A controller requested pause.
    RemotePause,
    /// A controller requested resume.
    RemoteResume,
    /// A controller requested an absolute seek.
    RemoteSeek(Duration),
    /// A controller requested stop.
    RemoteStop,
    /// A controller requested a volume.
    RemoteSetVolume(u8),
    /// A controller requested a mute state.
    RemoteSetMuted(bool),
    /// Periodic lifecycle-reporting tick.
    ProgressTick,
    /// Receiver shutdown.
    Shutdown,
}

/// A side effect for an adapter to execute.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReceiverAction {
    /// Ask Jellyfin to negotiate playback.
    ResolveMedia {
        /// New playback generation.
        generation: Generation,
        /// Remote request.
        request: PlayRequest,
    },
    /// Cancel obsolete Jellyfin negotiation work.
    CancelMediaResolution {
        /// Obsolete generation.
        generation: Generation,
    },
    /// Load negotiated media into mpv.
    LoadPlayer {
        /// Playback generation.
        generation: Generation,
        /// Negotiated media.
        media: ResolvedMedia,
        /// Requested starting position.
        start_position: Duration,
    },
    /// Set mpv pause state.
    SetPaused {
        /// Playback generation.
        generation: Generation,
        /// Requested state.
        paused: bool,
    },
    /// Seek mpv to an absolute position.
    Seek {
        /// Playback generation.
        generation: Generation,
        /// Requested position.
        position: Duration,
    },
    /// Set mpv volume.
    SetVolume {
        /// Playback generation.
        generation: Generation,
        /// Requested volume, from 0 through 100.
        volume_percent: u8,
    },
    /// Set mpv mute state.
    SetMuted {
        /// Playback generation.
        generation: Generation,
        /// Requested state.
        muted: bool,
    },
    /// Stop mpv for the current generation.
    StopPlayer {
        /// Playback generation.
        generation: Generation,
    },
    /// Report that playback started.
    ReportStarted(PlaybackSnapshot),
    /// Report player-confirmed progress.
    ReportProgress(PlaybackSnapshot),
    /// Report that a resolved playback stopped.
    ReportStopped {
        /// Final player-confirmed snapshot.
        snapshot: PlaybackSnapshot,
        /// Termination reason.
        reason: StopReason,
    },
    /// Surface a sanitized playback failure.
    PlaybackFailed {
        /// Failed generation.
        generation: Generation,
        /// Jellyfin item identifier.
        item_id: String,
        /// Sanitized error.
        message: String,
    },
}

/// Deterministic owner of the receiver playback state.
#[derive(Debug)]
pub struct Receiver {
    next_generation: u64,
    playback: PlaybackState,
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
        }
    }

    /// Returns the current playback state.
    #[must_use]
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
            ReceiverEvent::RemotePlay(request) => {
                self.terminate_current(StopReason::Replaced, &mut actions);
                let generation = self.allocate_generation();
                self.playback = PlaybackState::Resolving {
                    generation,
                    request: request.clone(),
                };
                actions.push(ReceiverAction::ResolveMedia {
                    generation,
                    request,
                });
            }
            ReceiverEvent::MediaResolved { generation, media } => {
                let previous = mem::take(&mut self.playback);
                match previous {
                    PlaybackState::Resolving {
                        generation: expected,
                        request,
                    } if expected == generation => {
                        let start_position = request.start_position;
                        self.playback = PlaybackState::Loading {
                            generation,
                            media: media.clone(),
                            start_position,
                        };
                        actions.push(ReceiverAction::LoadPlayer {
                            generation,
                            media,
                            start_position,
                        });
                    }
                    other => self.playback = other,
                }
            }
            ReceiverEvent::MediaResolutionFailed {
                generation,
                message,
            } => {
                let previous = mem::take(&mut self.playback);
                match previous {
                    PlaybackState::Resolving {
                        generation: expected,
                        request,
                    } if expected == generation => {
                        actions.push(ReceiverAction::PlaybackFailed {
                            generation,
                            item_id: request.item_id,
                            message,
                        });
                    }
                    other => self.playback = other,
                }
            }
            ReceiverEvent::PlayerFileLoaded { generation } => {
                let previous = mem::take(&mut self.playback);
                match previous {
                    PlaybackState::Loading {
                        generation: expected,
                        media,
                        start_position,
                    } if expected == generation => {
                        let snapshot = Self::initial_snapshot(generation, &media, start_position);
                        self.playback = PlaybackState::Playing(snapshot.clone());
                        actions.push(ReceiverAction::ReportStarted(snapshot));
                    }
                    other => self.playback = other,
                }
            }
            ReceiverEvent::PlayerFailed {
                generation,
                message,
            } => {
                let previous = mem::take(&mut self.playback);
                match previous {
                    PlaybackState::Loading {
                        generation: expected,
                        media,
                        start_position,
                    } if expected == generation => {
                        let snapshot = Self::initial_snapshot(generation, &media, start_position);
                        actions.push(ReceiverAction::StopPlayer { generation });
                        actions.push(ReceiverAction::ReportStopped {
                            snapshot,
                            reason: StopReason::Failed,
                        });
                        actions.push(ReceiverAction::PlaybackFailed {
                            generation,
                            item_id: media.item_id,
                            message,
                        });
                    }
                    PlaybackState::Playing(snapshot) if snapshot.generation == generation => {
                        let item_id = snapshot.item_id.clone();
                        actions.push(ReceiverAction::StopPlayer { generation });
                        actions.push(ReceiverAction::ReportStopped {
                            snapshot,
                            reason: StopReason::Failed,
                        });
                        actions.push(ReceiverAction::PlaybackFailed {
                            generation,
                            item_id,
                            message,
                        });
                    }
                    other => self.playback = other,
                }
            }
            ReceiverEvent::PlayerPositionChanged {
                generation,
                position,
                seek_completed,
            } => {
                if let Some(snapshot) = self.playing_mut(generation) {
                    snapshot.position = position;
                    if seek_completed {
                        actions.push(ReceiverAction::ReportProgress(snapshot.clone()));
                    }
                }
            }
            ReceiverEvent::PlayerPauseChanged { generation, paused } => {
                if let Some(snapshot) = self.playing_mut(generation)
                    && snapshot.paused != paused
                {
                    snapshot.paused = paused;
                    actions.push(ReceiverAction::ReportProgress(snapshot.clone()));
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
            ReceiverEvent::PlayerEnded { generation } => {
                if self.current_generation() == Some(generation) {
                    self.terminate_current(StopReason::Ended, &mut actions);
                }
            }
            ReceiverEvent::RemotePause => self.control_pause(true, &mut actions),
            ReceiverEvent::RemoteResume => self.control_pause(false, &mut actions),
            ReceiverEvent::RemoteSeek(position) => {
                if let Some(generation) = self.playing_generation() {
                    actions.push(ReceiverAction::Seek {
                        generation,
                        position,
                    });
                }
            }
            ReceiverEvent::RemoteStop => {
                self.terminate_current(StopReason::Remote, &mut actions);
            }
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

    fn allocate_generation(&mut self) -> Generation {
        let generation = Generation(self.next_generation);
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("playback generation counter exhausted");
        generation
    }

    fn initial_snapshot(
        generation: Generation,
        media: &ResolvedMedia,
        position: Duration,
    ) -> PlaybackSnapshot {
        PlaybackSnapshot {
            generation,
            item_id: media.item_id.clone(),
            play_session_id: media.play_session_id.clone(),
            media_source_id: media.media_source_id.clone(),
            play_method: media.play_method,
            position,
            paused: false,
            volume_percent: 100,
            muted: false,
        }
    }

    fn playing_mut(&mut self, generation: Generation) -> Option<&mut PlaybackSnapshot> {
        match &mut self.playback {
            PlaybackState::Playing(snapshot) if snapshot.generation == generation => Some(snapshot),
            _ => None,
        }
    }

    fn playing_generation(&self) -> Option<Generation> {
        match &self.playback {
            PlaybackState::Playing(snapshot) => Some(snapshot.generation),
            _ => None,
        }
    }

    fn current_generation(&self) -> Option<Generation> {
        match &self.playback {
            PlaybackState::Idle => None,
            PlaybackState::Resolving { generation, .. }
            | PlaybackState::Loading { generation, .. } => Some(*generation),
            PlaybackState::Playing(snapshot) => Some(snapshot.generation),
        }
    }

    fn control_pause(&self, paused: bool, actions: &mut Vec<ReceiverAction>) {
        if let Some(generation) = self.playing_generation() {
            actions.push(ReceiverAction::SetPaused { generation, paused });
        }
    }

    fn terminate_current(&mut self, reason: StopReason, actions: &mut Vec<ReceiverAction>) {
        match mem::take(&mut self.playback) {
            PlaybackState::Idle => {}
            PlaybackState::Resolving { generation, .. } => {
                actions.push(ReceiverAction::CancelMediaResolution { generation });
            }
            PlaybackState::Loading {
                generation,
                media,
                start_position,
            } => {
                actions.push(ReceiverAction::StopPlayer { generation });
                actions.push(ReceiverAction::ReportStopped {
                    snapshot: Self::initial_snapshot(generation, &media, start_position),
                    reason,
                });
            }
            PlaybackState::Playing(snapshot) => {
                actions.push(ReceiverAction::StopPlayer {
                    generation: snapshot.generation,
                });
                actions.push(ReceiverAction::ReportStopped { snapshot, reason });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(item_id: &str, seconds: u64) -> PlayRequest {
        PlayRequest {
            item_id: item_id.to_owned(),
            start_position: Duration::from_secs(seconds),
        }
    }

    fn media(item_id: &str, session: &str) -> ResolvedMedia {
        ResolvedMedia {
            item_id: item_id.to_owned(),
            uri: format!("https://media.invalid/{item_id}"),
            play_session_id: session.to_owned(),
            media_source_id: format!("source-{item_id}"),
            play_method: PlayMethod::DirectPlay,
        }
    }

    fn begin_playing(receiver: &mut Receiver, item_id: &str) -> Generation {
        let actions = receiver.handle(ReceiverEvent::RemotePlay(request(item_id, 12)));
        let generation = match actions.as_slice() {
            [ReceiverAction::ResolveMedia { generation, .. }] => *generation,
            unexpected => panic!("unexpected actions: {unexpected:?}"),
        };

        let actions = receiver.handle(ReceiverEvent::MediaResolved {
            generation,
            media: media(item_id, &format!("session-{item_id}")),
        });
        assert!(matches!(
            actions.as_slice(),
            [ReceiverAction::LoadPlayer { .. }]
        ));

        let actions = receiver.handle(ReceiverEvent::PlayerFileLoaded { generation });
        assert!(matches!(
            actions.as_slice(),
            [ReceiverAction::ReportStarted(_)]
        ));
        generation
    }

    #[test]
    fn play_is_reported_only_after_correlated_file_loaded() {
        let mut receiver = Receiver::new();
        let actions = receiver.handle(ReceiverEvent::RemotePlay(request("item-a", 12)));
        let generation = match actions.as_slice() {
            [ReceiverAction::ResolveMedia { generation, .. }] => *generation,
            unexpected => panic!("unexpected actions: {unexpected:?}"),
        };

        let actions = receiver.handle(ReceiverEvent::MediaResolved {
            generation,
            media: media("item-a", "session-a"),
        });
        assert!(matches!(
            actions.as_slice(),
            [ReceiverAction::LoadPlayer { .. }]
        ));
        assert!(matches!(receiver.playback(), PlaybackState::Loading { .. }));

        assert!(
            receiver
                .handle(ReceiverEvent::PlayerFileLoaded {
                    generation: Generation(generation.get() + 100),
                })
                .is_empty()
        );
        assert!(matches!(receiver.playback(), PlaybackState::Loading { .. }));

        let actions = receiver.handle(ReceiverEvent::PlayerFileLoaded { generation });
        let [ReceiverAction::ReportStarted(snapshot)] = actions.as_slice() else {
            panic!("unexpected actions: {actions:?}");
        };
        assert_eq!(snapshot.position, Duration::from_secs(12));
        assert!(matches!(receiver.playback(), PlaybackState::Playing(_)));
    }

    #[test]
    fn stale_media_resolution_is_ignored_after_replacement() {
        let mut receiver = Receiver::new();
        let first = match receiver
            .handle(ReceiverEvent::RemotePlay(request("old", 0)))
            .as_slice()
        {
            [ReceiverAction::ResolveMedia { generation, .. }] => *generation,
            unexpected => panic!("unexpected actions: {unexpected:?}"),
        };
        let second_actions = receiver.handle(ReceiverEvent::RemotePlay(request("new", 0)));
        let second = match second_actions.as_slice() {
            [
                ReceiverAction::CancelMediaResolution {
                    generation: cancelled,
                },
                ReceiverAction::ResolveMedia { generation, .. },
            ] => {
                assert_eq!(*cancelled, first);
                *generation
            }
            unexpected => panic!("unexpected actions: {unexpected:?}"),
        };

        assert!(
            receiver
                .handle(ReceiverEvent::MediaResolved {
                    generation: first,
                    media: media("old", "old-session"),
                })
                .is_empty()
        );
        assert!(matches!(
            receiver.playback(),
            PlaybackState::Resolving { generation, .. } if *generation == second
        ));
    }

    #[test]
    fn replacing_active_playback_stops_old_generation_once() {
        let mut receiver = Receiver::new();
        let old_generation = begin_playing(&mut receiver, "old");

        let actions = receiver.handle(ReceiverEvent::RemotePlay(request("new", 5)));
        assert_eq!(actions.len(), 3);
        assert!(matches!(
            actions[0],
            ReceiverAction::StopPlayer { generation } if generation == old_generation
        ));
        assert!(matches!(
            &actions[1],
            ReceiverAction::ReportStopped {
                snapshot,
                reason: StopReason::Replaced,
            } if snapshot.generation == old_generation
        ));

        assert!(
            receiver
                .handle(ReceiverEvent::PlayerEnded {
                    generation: old_generation,
                })
                .is_empty()
        );
    }

    #[test]
    fn remote_stop_reports_exactly_once() {
        let mut receiver = Receiver::new();
        let generation = begin_playing(&mut receiver, "item-a");

        let actions = receiver.handle(ReceiverEvent::RemoteStop);
        assert!(matches!(
            actions.as_slice(),
            [
                ReceiverAction::StopPlayer { .. },
                ReceiverAction::ReportStopped {
                    reason: StopReason::Remote,
                    ..
                }
            ]
        ));
        assert_eq!(receiver.playback(), &PlaybackState::Idle);
        assert!(receiver.handle(ReceiverEvent::RemoteStop).is_empty());
        assert!(
            receiver
                .handle(ReceiverEvent::PlayerEnded { generation })
                .is_empty()
        );
    }

    #[test]
    fn controller_changes_are_reported_only_after_player_confirmation() {
        let mut receiver = Receiver::new();
        let generation = begin_playing(&mut receiver, "item-a");

        assert!(matches!(
            receiver.handle(ReceiverEvent::RemotePause).as_slice(),
            [ReceiverAction::SetPaused { paused: true, .. }]
        ));
        let actions = receiver.handle(ReceiverEvent::PlayerPauseChanged {
            generation,
            paused: true,
        });
        assert!(matches!(
            actions.as_slice(),
            [ReceiverAction::ReportProgress(snapshot)] if snapshot.paused
        ));

        assert!(matches!(
            receiver
                .handle(ReceiverEvent::RemoteSeek(Duration::from_secs(90)))
                .as_slice(),
            [ReceiverAction::Seek { .. }]
        ));
        assert!(
            receiver
                .handle(ReceiverEvent::PlayerPositionChanged {
                    generation,
                    position: Duration::from_secs(90),
                    seek_completed: false,
                })
                .is_empty()
        );
        assert!(matches!(
            receiver
                .handle(ReceiverEvent::PlayerPositionChanged {
                    generation,
                    position: Duration::from_secs(90),
                    seek_completed: true,
                })
                .as_slice(),
            [ReceiverAction::ReportProgress(snapshot)]
                if snapshot.position == Duration::from_secs(90)
        ));
    }

    #[test]
    fn shutdown_cancels_pending_resolution() {
        let mut receiver = Receiver::new();
        let generation = match receiver
            .handle(ReceiverEvent::RemotePlay(request("item-a", 0)))
            .as_slice()
        {
            [ReceiverAction::ResolveMedia { generation, .. }] => *generation,
            unexpected => panic!("unexpected actions: {unexpected:?}"),
        };

        assert_eq!(
            receiver.handle(ReceiverEvent::Shutdown),
            vec![ReceiverAction::CancelMediaResolution { generation }]
        );
        assert_eq!(receiver.playback(), &PlaybackState::Idle);
    }
}
