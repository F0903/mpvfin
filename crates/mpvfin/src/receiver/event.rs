//! What the reducer reacts to: remote commands, what mpv reports, the
//! server's answers and the group.

use std::time::Duration;

use jellyfin::{GroupCommand, GroupQueue, QueueRequest, RemoteCommand, ResolvedMedia, Segment};

use crate::receiver::playback::{EndReason, Generation, PlayerToken};

/// An input to the receiver reducer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReceiverEvent {
    /// A controller requested items be queued.
    RemotePlay(QueueRequest),
    /// A controller requested the next playlist entry.
    RemoteNext,
    /// A controller requested the previous playlist entry.
    RemotePrevious,
    /// A controller requested pause.
    RemotePause,
    /// A controller requested resume.
    RemoteResume,
    /// A controller requested a pause toggle.
    RemoteTogglePause,
    /// A controller requested an absolute seek.
    RemoteSeek(Duration),
    /// A controller requested stop.
    RemoteStop,
    /// A controller requested a volume.
    RemoteSetVolume(u8),
    /// A controller requested a mute state.
    RemoteSetMuted(bool),
    /// A controller requested a mute toggle.
    RemoteToggleMute,
    /// A controller requested a Jellyfin audio stream.
    RemoteSetAudioStream(i32),
    /// A controller requested a Jellyfin subtitle stream (-1 is off).
    RemoteSetSubtitleStream(i32),
    /// mpv is starting a playlist entry and holds its load hook.
    PlayerOpening {
        /// Entry and hook identifiers.
        token: PlayerToken,
        /// Jellyfin item identifier from the entry URL.
        item_id: String,
        /// `SyncPlay` queue entry the URL names, if any.
        playlist_item_id: Option<String>,
    },
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
    /// mpv confirmed the selected tracks.
    PlayerTracksChanged {
        /// Playback generation.
        generation: Generation,
        /// Confirmed Jellyfin audio index.
        audio_stream_index: Option<i32>,
        /// Confirmed Jellyfin subtitle index; `None` is off.
        subtitle_stream_index: Option<i32>,
    },
    /// mpv ended the entry.
    PlayerEnded {
        /// Playback generation.
        generation: Generation,
        /// Why it ended.
        reason: EndReason,
    },
    /// mpv reached the end of the entry and is holding there.
    PlayerReachedEnd {
        /// Playback generation.
        generation: Generation,
    },
    /// mpv ran out of stream well before the item's end.
    PlayerStoppedEarly {
        /// Playback generation.
        generation: Generation,
    },
    /// mpv paused for the cache, or resumed from it.
    PlayerBuffering {
        /// Playback generation.
        generation: Generation,
        /// Whether playback is stalled.
        buffering: bool,
    },
    /// mpv opened a file that is not a Jellyfin entry.
    PlayerOpenedForeign,
    /// mpv has nothing open and nothing coming.
    PlayerIdle,
    /// The drift corrector wants a seek.
    DriftSeek {
        /// Playback generation.
        generation: Generation,
        /// Where the group is now.
        position: Duration,
    },
    /// The drift corrector lost the group's clock to a time jump.
    DriftLost,
    /// Jellyfin returned the item's media segments.
    SegmentsResolved {
        /// Generation attached to the request.
        generation: Generation,
        /// Segments, sorted by start.
        segments: Vec<Segment>,
    },
    /// The viewer asked to skip the active segment.
    SkipRequested,
    /// mpv learned or lost the length of the entry.
    PlayerDurationChanged {
        /// Playback generation.
        generation: Generation,
        /// Length of the entry, if known.
        duration: Option<Duration>,
    },
    /// The viewer pressed Play Now on the Next Up card.
    NextUpRequested,
    /// The viewer asked to create a `SyncPlay` group.
    GroupCreate {
        /// Group name.
        name: String,
    },
    /// The viewer asked to join a `SyncPlay` group.
    GroupJoin {
        /// Group id.
        group_id: String,
    },
    /// The viewer asked to leave the group.
    GroupLeave,
    /// The session joined a `SyncPlay` group.
    GroupJoined {
        /// Group id.
        group_id: String,
    },
    /// The session left its group.
    GroupLeft,
    /// The group became unusable.
    GroupGone {
        /// User-facing reason.
        message: String,
    },
    /// The group's queue changed.
    GroupQueueChanged(GroupQueue),
    /// A group playback command arrived.
    GroupCommandReceived(GroupCommand),
    /// The delay of a [`ReceiverAction::ScheduleResume`] elapsed.
    GroupResumeDue {
        /// Generation attached to the schedule.
        generation: Generation,
    },
    /// Periodic lifecycle-reporting tick.
    ProgressTick,
    /// Receiver shutdown.
    Shutdown,
}

impl From<RemoteCommand> for ReceiverEvent {
    fn from(command: RemoteCommand) -> Self {
        match command {
            RemoteCommand::Play(request) => Self::RemotePlay(request),
            RemoteCommand::Pause => Self::RemotePause,
            RemoteCommand::Resume => Self::RemoteResume,
            RemoteCommand::TogglePause => Self::RemoteTogglePause,
            RemoteCommand::Stop => Self::RemoteStop,
            RemoteCommand::Next => Self::RemoteNext,
            RemoteCommand::Previous => Self::RemotePrevious,
            RemoteCommand::Seek(position) => Self::RemoteSeek(position),
            RemoteCommand::SetVolume(volume) => Self::RemoteSetVolume(volume),
            RemoteCommand::SetMuted(muted) => Self::RemoteSetMuted(muted),
            RemoteCommand::ToggleMute => Self::RemoteToggleMute,
            RemoteCommand::SetAudioStream(index) => Self::RemoteSetAudioStream(index),
            RemoteCommand::SetSubtitleStream(index) => Self::RemoteSetSubtitleStream(index),
        }
    }
}
