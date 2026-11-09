//! The playback lifecycle: generations, snapshots, and why an item ended.

use std::time::Duration;

use jellyfin::{PlayMethod, PlaybackProgress, ResolvedMedia, StreamSelection};

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
