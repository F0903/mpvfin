//! What the reducer asks the adapters to do.

use std::time::Duration;

use jellyfin::{
    GroupEntry, GroupRequest, QueueRequest, ResolvedMedia, SegmentKind, StreamSelection,
};

use crate::receiver::playback::{Generation, PlaybackSnapshot, PlayerToken, StopReason};

/// A side effect for an adapter to execute.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReceiverAction {
    /// Add `mpvfin://` entries to mpv's playlist.
    QueueItems(QueueRequest),
    /// Advance mpv's playlist.
    PlaylistNext,
    /// Step back in mpv's playlist.
    PlaylistPrevious,
    /// Ask Jellyfin to negotiate playback.
    ResolveMedia {
        /// New playback generation.
        generation: Generation,
        /// Entry and hook being resolved.
        token: PlayerToken,
        /// Jellyfin item identifier.
        item_id: String,
    },
    /// Continue the held load hook with the negotiated media.
    ContinueLoad {
        /// Playback generation.
        generation: Generation,
        /// Hook to continue.
        hook_id: u64,
        /// Negotiated media.
        media: ResolvedMedia,
        /// Stream selection for this entry.
        selection: StreamSelection,
        /// How to load the entry for a `SyncPlay` group; `None` outside one.
        group: Option<GroupLoadOptions>,
    },
    /// Continue the held load hook without media, failing the entry.
    AbortLoad {
        /// Hook to continue.
        hook_id: u64,
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
    /// Select a Jellyfin audio stream.
    SetAudioStream {
        /// Playback generation.
        generation: Generation,
        /// Jellyfin audio index.
        index: i32,
    },
    /// Select a Jellyfin subtitle stream (-1 is off).
    SetSubtitleStream {
        /// Playback generation.
        generation: Generation,
        /// Jellyfin subtitle index.
        index: i32,
    },
    /// Stop mpv playback for the current generation.
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
    /// Show the skip button for a segment.
    ShowSkipButton {
        /// Playback generation.
        generation: Generation,
        /// Kind of the segment entered.
        kind: SegmentKind,
    },
    /// Hide the skip button.
    HideSkipButton {
        /// Playback generation.
        generation: Generation,
    },
    /// The Next Up card is due, if the playlist has a next entry.
    ShowNextUp {
        /// Playback generation.
        generation: Generation,
    },
    /// Hide the Next Up card.
    HideNextUp {
        /// Playback generation.
        generation: Generation,
    },
    /// Seek mpv to the end of the file.
    SeekToEnd {
        /// Playback generation.
        generation: Generation,
    },
    /// Mark the item played on the server.
    MarkPlayed {
        /// Playback generation.
        generation: Generation,
        /// Jellyfin item identifier.
        item_id: String,
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
    /// Send a request to the `SyncPlay` group.
    Group(GroupRequest),
    /// Replace mpv's playlist with the group's queue and start at `index`.
    GroupLoad {
        /// The queue in server order.
        entries: Vec<GroupEntry>,
        /// Entry to start.
        index: usize,
    },
    /// Re-mirror the group's queue around the current entry.
    GroupSyncPlaylist {
        /// The queue in server order.
        entries: Vec<GroupEntry>,
        /// Index of the current entry.
        index: usize,
    },
    /// Read mpv's playlist and send it to the group as its new queue.
    GroupPublishQueue {
        /// Start position of the current entry.
        position: Duration,
    },
    /// Feed [`ReceiverEvent::GroupResumeDue`] after a delay.
    ScheduleResume {
        /// Playback generation.
        generation: Generation,
        /// How long to wait.
        after: Duration,
    },
    /// Forget any scheduled resume.
    CancelResume,
    /// Start or stop correcting drift against the group's clock.
    DriftAnchor {
        /// Playback generation.
        generation: Generation,
        /// Position right now; `None` stops correcting.
        position: Option<Duration>,
    },
    /// Show a message on the OSD.
    ShowMessage {
        /// Text to show.
        text: String,
    },
}

/// How to load an entry for a `SyncPlay` group.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupLoadOptions {
    /// Where to start.
    pub start: Duration,
}
