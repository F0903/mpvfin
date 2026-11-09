//! The playback lifecycle: generations, snapshots, and why an item ended.

use std::mem;
use std::time::Duration;

use jellyfin::{PlayMethod, PlaybackProgress, ResolvedMedia, StreamSelection};

use crate::receiver::Receiver;
use crate::receiver::action::ReceiverAction;
use crate::receiver::segments::NextUpState;

/// Identifies one playback attempt.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Generation(pub(super) u64);

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

/// Identifies the mpv playlist entry and load hook of an opening item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlayerToken {
    /// mpv playlist entry identifier.
    pub entry_id: i64,
    /// mpv load-hook identifier, to be continued exactly once.
    pub hook_id: u64,
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
    /// Last player-confirmed Jellyfin audio stream index.
    pub audio_stream_index: Option<i32>,
    /// Last player-confirmed Jellyfin subtitle stream index; `None` is off.
    pub subtitle_stream_index: Option<i32>,
}

/// Why mpv ended an entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EndReason {
    /// Natural end of file.
    Eof,
    /// Stopped or replaced.
    Stopped,
    /// Failed; the message carries no credentials or URLs.
    Failed(String),
}

/// Why a playback report says playback stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
    /// mpv reached the natural end of the item.
    Ended,
    /// The item was stopped or replaced.
    Stopped,
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
    /// Jellyfin negotiation is in progress and the load hook is held.
    Resolving {
        /// Playback generation.
        generation: Generation,
        /// Entry and hook being resolved.
        token: PlayerToken,
        /// Jellyfin item identifier.
        item_id: String,
        /// Requested stream selection.
        selection: StreamSelection,
    },
    /// The hook was continued and mpv has not confirmed the load yet.
    Loading {
        /// Playback generation.
        generation: Generation,
        /// Negotiated media.
        media: ResolvedMedia,
    },
    /// mpv confirmed that the negotiated item loaded.
    Playing(PlaybackSnapshot),
}

impl From<&PlaybackSnapshot> for PlaybackProgress {
    fn from(s: &PlaybackSnapshot) -> Self {
        Self {
            item_id: s.item_id.clone(),
            play_session_id: s.play_session_id.clone(),
            media_source_id: s.media_source_id.clone(),
            play_method: s.play_method,
            position: s.position,
            paused: s.paused,
            volume_percent: s.volume_percent,
            muted: s.muted,
            audio_stream_index: s.audio_stream_index,
            subtitle_stream_index: s.subtitle_stream_index,
        }
    }
}

impl Receiver {
    pub(super) fn allocate_generation(&mut self) -> Generation {
        let generation = Generation(self.next_generation);
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("playback generation counter exhausted");
        generation
    }

    pub(super) fn initial_snapshot(
        generation: Generation,
        media: &ResolvedMedia,
    ) -> PlaybackSnapshot {
        PlaybackSnapshot {
            generation,
            item_id: media.item_id.clone(),
            play_session_id: media.play_session_id.clone(),
            media_source_id: media.media_source_id.clone(),
            play_method: media.play_method,
            position: Duration::ZERO,
            paused: false,
            volume_percent: 100,
            muted: false,
            audio_stream_index: None,
            subtitle_stream_index: None,
        }
    }

    pub(super) fn playing_mut(&mut self, generation: Generation) -> Option<&mut PlaybackSnapshot> {
        match &mut self.playback {
            PlaybackState::Playing(snapshot) if snapshot.generation == generation => Some(snapshot),
            _ => None,
        }
    }

    pub(super) fn playing_generation(&self) -> Option<Generation> {
        match &self.playback {
            PlaybackState::Playing(snapshot) => Some(snapshot.generation),
            _ => None,
        }
    }

    pub(super) fn current_generation(&self) -> Option<Generation> {
        match &self.playback {
            PlaybackState::Idle => None,
            PlaybackState::Resolving { generation, .. }
            | PlaybackState::Loading { generation, .. } => Some(*generation),
            PlaybackState::Playing(snapshot) => Some(snapshot.generation),
        }
    }

    pub(super) fn current_item_id(&self) -> Option<String> {
        match &self.playback {
            PlaybackState::Idle => None,
            PlaybackState::Resolving { item_id, .. } => Some(item_id.clone()),
            PlaybackState::Loading { media, .. } => Some(media.item_id.clone()),
            PlaybackState::Playing(snapshot) => Some(snapshot.item_id.clone()),
        }
    }

    /// Stops mpv for whatever is current; a held hook is aborted first.
    pub(super) fn stop_player(&mut self, actions: &mut Vec<ReceiverAction>) {
        match &self.playback {
            PlaybackState::Idle => {}
            PlaybackState::Loading { generation, .. } => {
                actions.push(ReceiverAction::StopPlayer {
                    generation: *generation,
                });
            }
            PlaybackState::Playing(snapshot) => {
                actions.push(ReceiverAction::StopPlayer {
                    generation: snapshot.generation,
                });
            }
            PlaybackState::Resolving { generation, .. } => {
                let generation = *generation;
                self.terminate_current(StopReason::Stopped, actions);
                actions.push(ReceiverAction::StopPlayer { generation });
            }
        }
    }

    pub(super) fn control_pause(&self, paused: bool, actions: &mut Vec<ReceiverAction>) {
        // A no-op must stay one: re-pausing would back up `pause=yes` per file.
        if let PlaybackState::Playing(snapshot) = &self.playback
            && snapshot.paused != paused
        {
            actions.push(ReceiverAction::SetPaused {
                generation: snapshot.generation,
                paused,
            });
        }
    }

    /// Goes idle. Resolving continues its hook with an abort; Playing reports
    /// a stop. mpv owns the actual entry, so no `StopPlayer`.
    pub(super) fn terminate_current(
        &mut self,
        reason: StopReason,
        actions: &mut Vec<ReceiverAction>,
    ) {
        if self.active.is_some()
            && let Some(generation) = self.current_generation()
        {
            actions.push(ReceiverAction::HideSkipButton { generation });
        }
        if self.next_up == NextUpState::Shown
            && let Some(generation) = self.current_generation()
        {
            actions.push(ReceiverAction::HideNextUp { generation });
        }
        self.segments.clear();
        self.active = None;
        self.dismissed = None;
        self.duration = None;
        self.next_up = NextUpState::Hidden;
        match mem::take(&mut self.playback) {
            // Loading sent no `ReportStarted`, and a stop without a start
            // resets the item's resume position on the server.
            PlaybackState::Idle | PlaybackState::Loading { .. } => {}
            PlaybackState::Resolving { token, .. } => {
                actions.push(ReceiverAction::AbortLoad {
                    hook_id: token.hook_id,
                });
            }
            PlaybackState::Playing(snapshot) => {
                actions.push(ReceiverAction::ReportStopped { snapshot, reason });
            }
        }
    }
}
