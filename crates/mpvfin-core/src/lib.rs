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

use std::fmt;
use std::mem;
use std::time::Duration;

mod group;

/// The entry id the server sends when a group has no current item.
pub const ZERO_ID: &str = "00000000000000000000000000000000";

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

/// A string that must never appear in logs; its `Debug` output is redacted.
#[derive(Clone, Eq, PartialEq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a sensitive value.
    #[must_use]
    pub const fn new(s: String) -> Self {
        Self(s)
    }

    /// Returns the sensitive value.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Where queued items go relative to mpv's playlist.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueMode {
    /// Replace the playlist and start playing.
    Replace,
    /// Insert after the current entry.
    Next,
    /// Append to the end.
    Last,
}

/// Jellyfin stream indices chosen by a controller.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StreamSelection {
    /// Jellyfin audio stream index.
    pub audio: Option<i32>,
    /// Jellyfin subtitle stream index; -1 means off.
    pub subtitle: Option<i32>,
}

/// A remote request to queue Jellyfin items.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueRequest {
    /// Jellyfin item identifiers, in order.
    pub item_ids: Vec<String>,
    /// Where to put the items.
    pub mode: QueueMode,
    /// Index of the item to start; `Replace` only, clamped to the last item.
    pub start_index: usize,
    /// Starting position of the start item; `Replace` only.
    pub start_position: Duration,
    /// Stream selection for the start item; `Replace` only.
    pub selection: StreamSelection,
}

/// Identifies the mpv playlist entry and load hook of an opening item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlayerToken {
    /// mpv playlist entry identifier.
    pub entry_id: i64,
    /// mpv load-hook identifier, to be continued exactly once.
    pub hook_id: u64,
}

/// Jellyfin stream indices in mpv track order; the mpv id is position + 1.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TrackMap {
    /// Jellyfin audio stream indices.
    pub audio: Vec<i32>,
    /// Jellyfin subtitle stream indices (embedded, then external).
    pub subtitles: Vec<i32>,
}

impl TrackMap {
    fn mpv_id(list: &[i32], index: i32) -> Option<i64> {
        let position = list.iter().position(|&i| i == index)?;
        i64::try_from(position).ok().map(|p| p + 1)
    }

    fn jellyfin_index(list: &[i32], mpv_id: i64) -> Option<i32> {
        let position = usize::try_from(mpv_id.checked_sub(1)?).ok()?;
        list.get(position).copied()
    }

    /// Maps a Jellyfin audio index to an mpv audio track id.
    #[must_use]
    pub fn mpv_audio_id(&self, index: i32) -> Option<i64> {
        Self::mpv_id(&self.audio, index)
    }

    /// Maps a Jellyfin subtitle index to an mpv subtitle track id.
    #[must_use]
    pub fn mpv_subtitle_id(&self, index: i32) -> Option<i64> {
        Self::mpv_id(&self.subtitles, index)
    }

    /// Maps an mpv audio track id to a Jellyfin audio index.
    #[must_use]
    pub fn jellyfin_audio_index(&self, mpv_id: i64) -> Option<i32> {
        Self::jellyfin_index(&self.audio, mpv_id)
    }

    /// Maps an mpv subtitle track id to a Jellyfin subtitle index.
    #[must_use]
    pub fn jellyfin_subtitle_index(&self, mpv_id: i64) -> Option<i32> {
        Self::jellyfin_index(&self.subtitles, mpv_id)
    }
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
    /// Credential-free stream URL passed to mpv.
    pub uri: String,
    /// Display title.
    pub title: String,
    /// Jellyfin playback-session identifier.
    pub play_session_id: String,
    /// Selected Jellyfin media-source identifier.
    pub media_source_id: String,
    /// Delivery method selected by Jellyfin.
    pub play_method: PlayMethod,
    /// Credential-free external subtitle URLs, after the embedded subtitles.
    pub external_subtitles: Vec<String>,
    /// Jellyfin/mpv track correspondence.
    pub tracks: TrackMap,
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

/// What a media segment is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentKind {
    /// Opening credits.
    Intro,
    /// Recap of earlier episodes.
    Recap,
    /// Closing credits.
    Outro,
    /// Preview of the next episode.
    Preview,
}

/// A skippable span of the playing item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Segment {
    /// What the segment is.
    pub kind: SegmentKind,
    /// Where it starts.
    pub start: Duration,
    /// Where it ends.
    pub end: Duration,
    /// Skipping seeks to the end of the file instead of to `end`.
    pub to_end_of_file: bool,
}

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
    /// Jellyfin returned the item's media segments.
    SegmentsResolved {
        /// Generation attached to the request.
        generation: Generation,
        /// Segments, sorted by start.
        segments: Vec<Segment>,
    },
    /// The viewer asked to skip the active segment.
    SkipRequested,
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
            position_settled: false,
            group: None,
            creating_group: false,
            opened_pid: None,
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
                actions.push(ReceiverAction::QueueItems(request));
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
                        self.position_settled = true;
                        self.group_position_settled(position, &mut actions);
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
                    actions.push(if segment.to_end_of_file {
                        ReceiverAction::SeekToEnd { generation }
                    } else {
                        ReceiverAction::Seek {
                            generation,
                            position: segment.end,
                        }
                    });
                    self.dismissed = Some(segment);
                }
            }
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

    fn allocate_generation(&mut self) -> Generation {
        let generation = Generation(self.next_generation);
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("playback generation counter exhausted");
        generation
    }

    fn initial_snapshot(generation: Generation, media: &ResolvedMedia) -> PlaybackSnapshot {
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

    fn current_item_id(&self) -> Option<String> {
        match &self.playback {
            PlaybackState::Idle => None,
            PlaybackState::Resolving { item_id, .. } => Some(item_id.clone()),
            PlaybackState::Loading { media, .. } => Some(media.item_id.clone()),
            PlaybackState::Playing(snapshot) => Some(snapshot.item_id.clone()),
        }
    }

    /// Shows, swaps or hides the skip button for the current position.
    fn evaluate_segments(&mut self, actions: &mut Vec<ReceiverAction>) {
        let PlaybackState::Playing(snapshot) = &self.playback else {
            return;
        };
        if !self.position_settled {
            return;
        }
        let (generation, position) = (snapshot.generation, snapshot.position);
        let found = self
            .segments
            .iter()
            .find(|s| s.start <= position && position < s.end)
            .cloned();
        if self.dismissed.is_some() && self.dismissed != found {
            self.dismissed = None;
        }
        let next = found.filter(|s| self.dismissed.as_ref() != Some(s));
        if next != self.active {
            if self.active.is_some() {
                actions.push(ReceiverAction::HideSkipButton { generation });
            }
            if let Some(segment) = &next {
                actions.push(ReceiverAction::ShowSkipButton {
                    generation,
                    kind: segment.kind,
                });
            }
            self.active = next;
        }
    }

    /// Stops mpv for whatever is current; a held hook is aborted first.
    fn stop_player(&mut self, actions: &mut Vec<ReceiverAction>) {
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

    fn control_pause(&self, paused: bool, actions: &mut Vec<ReceiverAction>) {
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
    fn terminate_current(&mut self, reason: StopReason, actions: &mut Vec<ReceiverAction>) {
        if self.active.is_some()
            && let Some(generation) = self.current_generation()
        {
            actions.push(ReceiverAction::HideSkipButton { generation });
        }
        self.segments.clear();
        self.active = None;
        self.dismissed = None;
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

/// One entry of a `SyncPlay` queue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupEntry {
    /// Library item id.
    pub item_id: String,
    /// The queue entry's own id, stable for its lifetime.
    pub playlist_item_id: String,
}

/// Why the group's queue update was sent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueReason {
    /// A whole new queue.
    NewPlaylist,
    /// The current item was set.
    SetCurrentItem,
    /// Moved to the next item.
    NextItem,
    /// Moved to the previous item.
    PreviousItem,
    /// Items were removed.
    RemoveItems,
    /// Anything that does not change the playing item.
    Other,
}

/// A group's play queue as the server last described it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupQueue {
    /// Why it was sent.
    pub reason: QueueReason,
    /// Server time of the update, in ms since the Unix epoch.
    pub last_update_ms: i64,
    /// The queue in server order.
    pub entries: Vec<GroupEntry>,
    /// Index of the current entry, if any.
    pub playing_index: Option<usize>,
    /// Where to start (or the extrapolated position).
    pub start_position: Duration,
    /// Whether the group is playing.
    pub is_playing: bool,
}

/// A group playback command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroupCommandKind {
    /// Start playing.
    Unpause,
    /// Pause.
    Pause,
    /// Seek.
    Seek,
    /// Stop.
    Stop,
}

/// A group playback command with its timing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupCommand {
    /// What to do.
    pub kind: GroupCommandKind,
    /// Entry it applies to; all zeros when none.
    pub playlist_item_id: String,
    /// Position at `When`.
    pub position: Duration,
    /// `When` minus server now at receipt; may be negative.
    pub delay_ms: i64,
}

/// One group in the server's list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupSummary {
    /// Group id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Distinct participants.
    pub members: usize,
    /// Server state name.
    pub state: String,
}

/// A request a member sends to the group.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GroupRequest {
    /// Create a group.
    New {
        /// Group name.
        name: String,
    },
    /// Join a group.
    Join {
        /// Group id.
        group_id: String,
    },
    /// Leave the group.
    Leave,
    /// Replace the queue.
    SetNewQueue {
        /// Items in order.
        item_ids: Vec<String>,
        /// Index to play.
        index: usize,
        /// Start position.
        start: Duration,
    },
    /// Add to the queue.
    Queue {
        /// Items to add.
        item_ids: Vec<String>,
        /// Insert after the current item instead of at the end.
        next: bool,
    },
    /// Jump to an entry.
    SetPlaylistItem {
        /// Entry id.
        pid: String,
    },
    /// Advance from an entry.
    NextItem {
        /// Entry id.
        pid: String,
    },
    /// Go back from an entry.
    PreviousItem {
        /// Entry id.
        pid: String,
    },
    /// Resume the group.
    Unpause,
    /// Pause the group.
    Pause,
    /// Stop the group.
    Stop,
    /// Seek the group.
    Seek {
        /// Target position.
        position: Duration,
    },
    /// Report ready at a position.
    Ready {
        /// Sampled position.
        position: Duration,
        /// Whether local playback is running.
        is_playing: bool,
        /// Entry id.
        pid: String,
    },
    /// Report buffering at a position.
    Buffering {
        /// Sampled position.
        position: Duration,
        /// Entry id.
        pid: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn gen_(n: u64) -> Generation {
        Generation::new(n).unwrap()
    }

    pub(super) fn token(entry_id: i64, hook_id: u64) -> PlayerToken {
        PlayerToken { entry_id, hook_id }
    }

    fn queue(
        ids: &[&str],
        start_index: usize,
        secs: u64,
        selection: StreamSelection,
    ) -> QueueRequest {
        QueueRequest {
            item_ids: ids.iter().map(|s| (*s).to_owned()).collect(),
            mode: QueueMode::Replace,
            start_index,
            start_position: Duration::from_secs(secs),
            selection,
        }
    }

    fn media(item_id: &str) -> ResolvedMedia {
        ResolvedMedia {
            item_id: item_id.to_owned(),
            uri: format!("https://media.invalid/{item_id}"),
            title: format!("title-{item_id}"),
            play_session_id: format!("session-{item_id}"),
            media_source_id: format!("source-{item_id}"),
            play_method: PlayMethod::DirectPlay,
            external_subtitles: Vec::new(),
            tracks: TrackMap::default(),
        }
    }

    pub(super) fn open(r: &mut Receiver, item: &str, hook: u64) -> Vec<ReceiverAction> {
        r.handle(ReceiverEvent::PlayerOpening {
            token: token(1, hook),
            item_id: item.to_owned(),
            playlist_item_id: None,
        })
    }

    pub(super) fn resolve(r: &mut Receiver, g: Generation, item: &str) -> Vec<ReceiverAction> {
        r.handle(ReceiverEvent::MediaResolved {
            generation: g,
            media: media(item),
        })
    }

    pub(super) fn begin_playing(r: &mut Receiver, item: &str, hook: u64) -> Generation {
        let g = match open(r, item, hook).as_slice() {
            [ReceiverAction::ResolveMedia { generation, .. }] => *generation,
            other => panic!("unexpected actions: {other:?}"),
        };
        assert!(matches!(
            resolve(r, g, item).as_slice(),
            [ReceiverAction::ContinueLoad { .. }]
        ));
        assert!(matches!(
            r.handle(ReceiverEvent::PlayerFileLoaded { generation: g })
                .as_slice(),
            [ReceiverAction::ReportStarted(_)]
        ));
        g
    }

    pub(super) fn ended(g: Generation, reason: EndReason) -> ReceiverEvent {
        ReceiverEvent::PlayerEnded {
            generation: g,
            reason,
        }
    }

    #[test]
    fn remote_play_queues_items_without_changing_state() {
        let mut r = Receiver::new();
        let req = queue(&["a", "b"], 1, 30, StreamSelection::default());
        assert_eq!(
            r.handle(ReceiverEvent::RemotePlay(req.clone())),
            vec![ReceiverAction::QueueItems(req)]
        );
        assert_eq!(r.playback(), &PlaybackState::Idle);
    }

    #[test]
    fn opening_entry_starts_resolution_with_fresh_generation() {
        let mut r = Receiver::new();
        assert_eq!(
            open(&mut r, "a", 70),
            vec![ReceiverAction::ResolveMedia {
                generation: gen_(1),
                token: token(1, 70),
                item_id: "a".into()
            }]
        );
        r.handle(ended(gen_(1), EndReason::Stopped));
        assert!(matches!(
            open(&mut r, "b", 71).as_slice(),
            [ReceiverAction::ResolveMedia { generation, .. }] if *generation == gen_(2)
        ));
    }

    #[test]
    fn resolution_continues_the_pending_hook_with_requested_selection() {
        let mut r = Receiver::new();
        let sel = StreamSelection {
            audio: Some(2),
            subtitle: Some(-1),
        };
        r.handle(ReceiverEvent::RemotePlay(queue(&["a", "b"], 0, 0, sel)));
        open(&mut r, "a", 70);
        let actions = resolve(&mut r, gen_(1), "a");
        assert!(matches!(
            actions.last(),
            Some(ReceiverAction::ContinueLoad { hook_id: 70, selection, .. }) if *selection == sel
        ));
        let g2 = match open(&mut r, "b", 80).last() {
            Some(ReceiverAction::ResolveMedia { generation, .. }) => *generation,
            other => panic!("unexpected actions: {other:?}"),
        };
        assert!(matches!(
            resolve(&mut r, g2, "b").last(),
            Some(ReceiverAction::ContinueLoad { hook_id: 80, selection, .. })
                if *selection == StreamSelection::default()
        ));
    }

    #[test]
    fn play_is_reported_only_after_correlated_file_loaded() {
        let mut r = Receiver::new();
        open(&mut r, "a", 70);
        let actions = resolve(&mut r, gen_(1), "a");
        assert!(actions.iter().all(|a| !matches!(
            a,
            ReceiverAction::ReportStarted(_) | ReceiverAction::ReportProgress(_)
        )));
        assert!(matches!(r.playback(), PlaybackState::Loading { .. }));
        assert!(
            r.handle(ReceiverEvent::PlayerFileLoaded {
                generation: gen_(101)
            })
            .is_empty()
        );
        assert!(matches!(r.playback(), PlaybackState::Loading { .. }));
        let actions = r.handle(ReceiverEvent::PlayerFileLoaded {
            generation: gen_(1),
        });
        let [ReceiverAction::ReportStarted(s)] = actions.as_slice() else {
            panic!("unexpected actions: {actions:?}");
        };
        assert_eq!(s.position, Duration::ZERO);
        assert!(matches!(r.playback(), PlaybackState::Playing(_)));
    }

    #[test]
    fn stale_resolution_is_ignored_after_next_entry_opens() {
        let mut r = Receiver::new();
        open(&mut r, "a", 70);
        let actions = open(&mut r, "b", 80);
        assert!(matches!(
            actions.as_slice(),
            [
                ReceiverAction::AbortLoad { hook_id: 70 },
                ReceiverAction::ResolveMedia { generation, .. }
            ] if *generation == gen_(2)
        ));
        assert!(resolve(&mut r, gen_(1), "a").is_empty());
    }

    #[test]
    fn every_hook_is_continued_exactly_once() {
        fn count(actions: &[ReceiverAction], hook: u64) -> usize {
            actions
                .iter()
                .filter(|a| {
                    matches!(a, ReceiverAction::ContinueLoad { hook_id, .. }
                        | ReceiverAction::AbortLoad { hook_id } if *hook_id == hook)
                })
                .count()
        }
        // resolved
        let mut r = Receiver::new();
        open(&mut r, "a", 70);
        let all = resolve(&mut r, gen_(1), "a");
        assert_eq!(count(&all, 70), 1);
        // resolution failed
        let mut r = Receiver::new();
        open(&mut r, "a", 70);
        let all = r.handle(ReceiverEvent::MediaResolutionFailed {
            generation: gen_(1),
            message: "boom".into(),
        });
        assert_eq!(count(&all, 70), 1);
        assert!(matches!(
            all.as_slice(),
            [
                ReceiverAction::AbortLoad { .. },
                ReceiverAction::PlaybackFailed { .. }
            ]
        ));
        assert_eq!(r.playback(), &PlaybackState::Idle);
        // superseded
        let mut r = Receiver::new();
        let mut all = open(&mut r, "a", 70);
        all.extend(open(&mut r, "b", 80));
        all.extend(resolve(&mut r, gen_(1), "a"));
        all.extend(resolve(&mut r, gen_(2), "b"));
        assert_eq!(count(&all, 70), 1);
        assert_eq!(count(&all, 80), 1);
        // shutdown while resolving
        let mut r = Receiver::new();
        let mut all = open(&mut r, "a", 70);
        all.extend(r.handle(ReceiverEvent::Shutdown));
        assert_eq!(count(&all, 70), 1);
    }

    #[test]
    fn end_of_file_reports_stopped_once_without_stop_command() {
        let mut r = Receiver::new();
        let g = begin_playing(&mut r, "a", 70);
        let actions = r.handle(ended(g, EndReason::Eof));
        assert!(matches!(
            actions.as_slice(),
            [ReceiverAction::ReportStopped {
                reason: StopReason::Ended,
                ..
            }]
        ));
        assert!(r.handle(ended(g, EndReason::Eof)).is_empty());
    }

    #[test]
    fn remote_stop_stops_player_then_reports_on_end() {
        let mut r = Receiver::new();
        let g = begin_playing(&mut r, "a", 70);
        assert_eq!(
            r.handle(ReceiverEvent::RemoteStop),
            vec![ReceiverAction::StopPlayer { generation: g }]
        );
        assert!(matches!(
            r.handle(ended(g, EndReason::Stopped)).as_slice(),
            [ReceiverAction::ReportStopped {
                reason: StopReason::Stopped,
                ..
            }]
        ));
    }

    fn begin_loading(r: &mut Receiver, item: &str, hook: u64) -> Generation {
        let g = match open(r, item, hook).as_slice() {
            [ReceiverAction::ResolveMedia { generation, .. }] => *generation,
            other => panic!("unexpected actions: {other:?}"),
        };
        resolve(r, g, item);
        g
    }

    #[test]
    fn next_entry_while_loading_reports_nothing() {
        let mut r = Receiver::new();
        begin_loading(&mut r, "a", 70);
        assert!(matches!(
            open(&mut r, "b", 80).as_slice(),
            [ReceiverAction::ResolveMedia { token, .. }] if token.hook_id == 80
        ));
    }

    #[test]
    fn shutdown_while_loading_reports_nothing() {
        let mut r = Receiver::new();
        begin_loading(&mut r, "a", 70);
        assert!(r.handle(ReceiverEvent::Shutdown).is_empty());
        assert_eq!(r.playback(), &PlaybackState::Idle);
    }

    #[test]
    fn remote_stop_while_loading_stops_player_and_reports_nothing() {
        let mut r = Receiver::new();
        let g = begin_loading(&mut r, "a", 70);
        assert_eq!(
            r.handle(ReceiverEvent::RemoteStop),
            vec![ReceiverAction::StopPlayer { generation: g }]
        );
        assert!(r.handle(ended(g, EndReason::Stopped)).is_empty());
        assert_eq!(r.playback(), &PlaybackState::Idle);
    }

    #[test]
    fn failed_end_while_loading_only_surfaces_the_error() {
        let mut r = Receiver::new();
        let g = begin_loading(&mut r, "a", 70);
        assert_eq!(
            r.handle(ended(g, EndReason::Failed("x".into()))),
            vec![ReceiverAction::PlaybackFailed {
                generation: g,
                item_id: "a".into(),
                message: "x".into()
            }]
        );
    }

    #[test]
    fn remote_stop_while_resolving_aborts_hook_and_stops_player() {
        let mut r = Receiver::new();
        open(&mut r, "a", 70);
        assert_eq!(
            r.handle(ReceiverEvent::RemoteStop),
            vec![
                ReceiverAction::AbortLoad { hook_id: 70 },
                ReceiverAction::StopPlayer {
                    generation: gen_(1)
                }
            ]
        );
        assert_eq!(r.playback(), &PlaybackState::Idle);
    }

    #[test]
    fn failed_end_while_resolving_only_aborts_hook() {
        let mut r = Receiver::new();
        open(&mut r, "a", 70);
        assert_eq!(
            r.handle(ended(gen_(1), EndReason::Failed("x".into()))),
            vec![ReceiverAction::AbortLoad { hook_id: 70 }]
        );
        assert_eq!(r.playback(), &PlaybackState::Idle);
    }

    #[test]
    fn opening_while_playing_reports_stopped_without_stop_command() {
        let mut r = Receiver::new();
        let g = begin_playing(&mut r, "a", 70);
        let actions = open(&mut r, "b", 80);
        assert!(matches!(
            actions.as_slice(),
            [
                ReceiverAction::ReportStopped { snapshot, reason: StopReason::Stopped },
                ReceiverAction::ResolveMedia { .. }
            ] if snapshot.generation == g
        ));
    }

    #[test]
    fn player_failure_reports_failed_stop_and_surfaces_error() {
        let mut r = Receiver::new();
        let g = begin_playing(&mut r, "a", 70);
        let actions = r.handle(ended(g, EndReason::Failed("x".into())));
        assert!(matches!(
            actions.as_slice(),
            [
                ReceiverAction::ReportStopped { reason: StopReason::Failed, .. },
                ReceiverAction::PlaybackFailed { message, .. }
            ] if message == "x"
        ));
    }

    #[test]
    fn toggles_invert_confirmed_state() {
        let mut r = Receiver::new();
        let g = begin_playing(&mut r, "a", 70);
        r.handle(ReceiverEvent::PlayerMuteChanged {
            generation: g,
            muted: true,
        });
        assert_eq!(
            r.handle(ReceiverEvent::RemoteTogglePause),
            vec![ReceiverAction::SetPaused {
                generation: g,
                paused: true
            }]
        );
        assert_eq!(
            r.handle(ReceiverEvent::RemoteToggleMute),
            vec![ReceiverAction::SetMuted {
                generation: g,
                muted: false
            }]
        );
    }

    #[test]
    fn pause_control_matching_the_confirmed_state_is_a_no_op() {
        let mut r = Receiver::new();
        let g = begin_playing(&mut r, "a", 70);
        assert!(r.handle(ReceiverEvent::RemoteResume).is_empty());
        r.handle(ReceiverEvent::PlayerPauseChanged {
            generation: g,
            paused: true,
        });
        assert!(r.handle(ReceiverEvent::RemotePause).is_empty());
        assert_eq!(
            r.handle(ReceiverEvent::RemoteResume),
            vec![ReceiverAction::SetPaused {
                generation: g,
                paused: false
            }]
        );
    }

    #[test]
    fn track_changes_are_reported_only_after_player_confirmation() {
        let mut r = Receiver::new();
        let g = begin_playing(&mut r, "a", 70);
        assert_eq!(
            r.handle(ReceiverEvent::RemoteSetAudioStream(2)),
            vec![ReceiverAction::SetAudioStream {
                generation: g,
                index: 2
            }]
        );
        let ev = ReceiverEvent::PlayerTracksChanged {
            generation: g,
            audio_stream_index: Some(2),
            subtitle_stream_index: Some(5),
        };
        let actions = r.handle(ev.clone());
        assert!(matches!(
            actions.as_slice(),
            [ReceiverAction::ReportProgress(s)]
                if s.audio_stream_index == Some(2) && s.subtitle_stream_index == Some(5)
        ));
        assert!(r.handle(ev).is_empty());
    }

    #[test]
    fn stream_commands_and_playlist_commands() {
        let mut r = Receiver::new();
        assert!(
            r.handle(ReceiverEvent::RemoteSetSubtitleStream(1))
                .is_empty()
        );
        // Idle: a phone must not skip through a local playlist.
        assert!(r.handle(ReceiverEvent::RemoteNext).is_empty());
        assert!(r.handle(ReceiverEvent::RemotePrevious).is_empty());
        begin_playing(&mut r, "a", 70);
        assert_eq!(
            r.handle(ReceiverEvent::RemoteNext),
            vec![ReceiverAction::PlaylistNext]
        );
        assert_eq!(
            r.handle(ReceiverEvent::RemotePrevious),
            vec![ReceiverAction::PlaylistPrevious]
        );
    }

    #[test]
    fn controller_changes_are_reported_only_after_player_confirmation() {
        let mut r = Receiver::new();
        let g = begin_playing(&mut r, "a", 70);
        assert!(matches!(
            r.handle(ReceiverEvent::RemotePause).as_slice(),
            [ReceiverAction::SetPaused { paused: true, .. }]
        ));
        assert!(matches!(
            r.handle(ReceiverEvent::PlayerPauseChanged {
                generation: g,
                paused: true
            })
            .as_slice(),
            [ReceiverAction::ReportProgress(s)] if s.paused
        ));
        assert!(matches!(
            r.handle(ReceiverEvent::RemoteSeek(Duration::from_secs(90)))
                .as_slice(),
            [ReceiverAction::Seek { .. }]
        ));
        assert!(
            r.handle(ReceiverEvent::PlayerPositionChanged {
                generation: g,
                position: Duration::from_secs(90),
                seek_completed: false,
            })
            .is_empty()
        );
        assert!(matches!(
            r.handle(ReceiverEvent::PlayerPositionChanged {
                generation: g,
                position: Duration::from_secs(90),
                seek_completed: true,
            })
            .as_slice(),
            [ReceiverAction::ReportProgress(s)] if s.position == Duration::from_secs(90)
        ));
    }

    #[test]
    fn shutdown_reports_active_playback_without_stop_command() {
        let mut r = Receiver::new();
        begin_playing(&mut r, "a", 70);
        assert!(matches!(
            r.handle(ReceiverEvent::Shutdown).as_slice(),
            [ReceiverAction::ReportStopped {
                reason: StopReason::Shutdown,
                ..
            }]
        ));
    }

    #[test]
    fn track_map_round_trips_embedded_and_external_indices() {
        let t = TrackMap {
            audio: vec![1, 2],
            subtitles: vec![3, 4, 7],
        };
        assert_eq!(t.mpv_audio_id(2), Some(2));
        assert_eq!(t.mpv_subtitle_id(7), Some(3));
        assert_eq!(t.jellyfin_subtitle_index(1), Some(3));
        assert_eq!(t.jellyfin_audio_index(2), Some(2));
        assert_eq!(t.mpv_audio_id(9), None);
        assert_eq!(t.jellyfin_audio_index(0), None);
    }

    fn seg(kind: SegmentKind, start: u64, end: u64, to_end_of_file: bool) -> Segment {
        Segment {
            kind,
            start: Duration::from_secs(start),
            end: Duration::from_secs(end),
            to_end_of_file,
        }
    }

    fn segments(r: &mut Receiver, g: Generation, list: Vec<Segment>) -> Vec<ReceiverAction> {
        r.handle(ReceiverEvent::SegmentsResolved {
            generation: g,
            segments: list,
        })
    }

    fn at(r: &mut Receiver, g: Generation, secs: u64) -> Vec<ReceiverAction> {
        r.handle(ReceiverEvent::PlayerPositionChanged {
            generation: g,
            position: Duration::from_secs(secs),
            seek_completed: false,
        })
    }

    /// mpv's first confirmed position for the item, which opens the button.
    pub(super) fn settle(r: &mut Receiver, g: Generation, secs: u64) -> Vec<ReceiverAction> {
        r.handle(ReceiverEvent::PlayerPositionChanged {
            generation: g,
            position: Duration::from_secs(secs),
            seek_completed: true,
        })
    }

    fn show(g: Generation, kind: SegmentKind) -> ReceiverAction {
        ReceiverAction::ShowSkipButton {
            generation: g,
            kind,
        }
    }

    fn hide(g: Generation) -> ReceiverAction {
        ReceiverAction::HideSkipButton { generation: g }
    }

    fn playing_with(list: Vec<Segment>) -> (Receiver, Generation) {
        let mut r = Receiver::new();
        let g = begin_playing(&mut r, "a", 70);
        assert!(segments(&mut r, g, list).is_empty());
        settle(&mut r, g, 0);
        (r, g)
    }

    #[test]
    fn no_button_before_the_first_settled_position() {
        let mut r = Receiver::new();
        let g = begin_playing(&mut r, "a", 70);
        assert!(at(&mut r, g, 0).is_empty());
        let list = vec![
            seg(SegmentKind::Recap, 0, 4, false),
            seg(SegmentKind::Intro, 5, 20, false),
        ];
        assert!(segments(&mut r, g, list).is_empty(), "inside the recap");
        assert!(at(&mut r, g, 8).is_empty(), "inside the intro");
        let actions = settle(&mut r, g, 8);
        assert!(
            matches!(actions.as_slice(), [ReceiverAction::ReportProgress(_), s] if *s == show(g, SegmentKind::Intro)),
            "{actions:?}"
        );
    }

    #[test]
    fn the_next_item_must_settle_again() {
        let (mut r, g) = playing_with(vec![]);
        r.handle(ended(g, EndReason::Eof));
        let g2 = begin_playing(&mut r, "b", 80);
        assert!(segments(&mut r, g2, vec![seg(SegmentKind::Intro, 0, 20, false)]).is_empty());
        assert!(at(&mut r, g2, 8).is_empty());
    }

    #[test]
    fn segment_button_shows_on_entry_and_hides_on_exit() {
        let (mut r, g) = playing_with(vec![seg(SegmentKind::Intro, 5, 20, false)]);
        assert!(at(&mut r, g, 4).is_empty());
        assert_eq!(at(&mut r, g, 5), vec![show(g, SegmentKind::Intro)]);
        assert!(at(&mut r, g, 10).is_empty());
        assert_eq!(at(&mut r, g, 20), vec![hide(g)]);
    }

    #[test]
    fn segments_arriving_mid_segment_show_the_button_at_once() {
        let mut r = Receiver::new();
        let g = begin_playing(&mut r, "a", 70);
        settle(&mut r, g, 8);
        assert_eq!(
            segments(&mut r, g, vec![seg(SegmentKind::Intro, 5, 20, false)]),
            vec![show(g, SegmentKind::Intro)]
        );
    }

    #[test]
    fn stale_segments_are_ignored() {
        let mut r = Receiver::new();
        let g = begin_playing(&mut r, "a", 70);
        settle(&mut r, g, 0);
        assert!(
            segments(
                &mut r,
                gen_(99),
                vec![seg(SegmentKind::Intro, 0, 20, false)]
            )
            .is_empty()
        );
        assert!(at(&mut r, g, 8).is_empty());
    }

    #[test]
    fn skip_seeks_to_segment_end_and_hides() {
        let (mut r, g) = playing_with(vec![seg(SegmentKind::Intro, 5, 20, false)]);
        at(&mut r, g, 10);
        assert_eq!(
            r.handle(ReceiverEvent::SkipRequested),
            vec![
                hide(g),
                ReceiverAction::Seek {
                    generation: g,
                    position: Duration::from_secs(20)
                }
            ]
        );
    }

    #[test]
    fn skipping_credits_marks_played_first() {
        let (mut r, g) = playing_with(vec![seg(SegmentKind::Outro, 90, 110, false)]);
        at(&mut r, g, 95);
        assert_eq!(
            r.handle(ReceiverEvent::SkipRequested),
            vec![
                ReceiverAction::MarkPlayed {
                    generation: g,
                    item_id: "a".into()
                },
                hide(g),
                ReceiverAction::Seek {
                    generation: g,
                    position: Duration::from_secs(110)
                }
            ]
        );
    }

    #[test]
    fn skipping_a_segment_at_end_of_file_seeks_to_end() {
        let (mut r, g) = playing_with(vec![seg(SegmentKind::Preview, 90, 100, true)]);
        at(&mut r, g, 95);
        assert_eq!(
            r.handle(ReceiverEvent::SkipRequested),
            vec![hide(g), ReceiverAction::SeekToEnd { generation: g }]
        );
    }

    #[test]
    fn dismissed_segment_stays_hidden_while_the_seek_lands() {
        let (mut r, g) = playing_with(vec![seg(SegmentKind::Intro, 5, 20, false)]);
        at(&mut r, g, 10);
        r.handle(ReceiverEvent::SkipRequested);
        assert!(at(&mut r, g, 12).is_empty());
    }

    #[test]
    fn seeking_back_into_a_dismissed_segment_shows_it_again() {
        let (mut r, g) = playing_with(vec![seg(SegmentKind::Intro, 5, 20, false)]);
        at(&mut r, g, 10);
        r.handle(ReceiverEvent::SkipRequested);
        assert!(at(&mut r, g, 25).is_empty());
        assert_eq!(at(&mut r, g, 10), vec![show(g, SegmentKind::Intro)]);
    }

    #[test]
    fn skipping_credits_lands_on_an_adjacent_segment_and_shows_its_button() {
        let (mut r, g) = playing_with(vec![
            seg(SegmentKind::Outro, 90, 110, false),
            seg(SegmentKind::Preview, 110, 120, false),
        ]);
        assert_eq!(at(&mut r, g, 95), vec![show(g, SegmentKind::Outro)]);
        assert_eq!(
            r.handle(ReceiverEvent::SkipRequested),
            vec![
                ReceiverAction::MarkPlayed {
                    generation: g,
                    item_id: "a".into()
                },
                hide(g),
                ReceiverAction::Seek {
                    generation: g,
                    position: Duration::from_secs(110)
                }
            ]
        );
        assert_eq!(at(&mut r, g, 110), vec![show(g, SegmentKind::Preview)]);
    }

    #[test]
    fn adjacent_segments_swap_the_button() {
        let (mut r, g) = playing_with(vec![
            seg(SegmentKind::Outro, 90, 110, false),
            seg(SegmentKind::Preview, 110, 120, false),
        ]);
        assert_eq!(at(&mut r, g, 109), vec![show(g, SegmentKind::Outro)]);
        assert_eq!(
            at(&mut r, g, 111),
            vec![hide(g), show(g, SegmentKind::Preview)]
        );
    }

    #[test]
    fn skip_outside_a_segment_does_nothing() {
        let (mut r, g) = playing_with(vec![seg(SegmentKind::Intro, 5, 20, false)]);
        at(&mut r, g, 30);
        assert!(r.handle(ReceiverEvent::SkipRequested).is_empty());
        let mut idle = Receiver::new();
        assert!(idle.handle(ReceiverEvent::SkipRequested).is_empty());
    }

    #[test]
    fn item_end_hides_the_button() {
        let (mut r, g) = playing_with(vec![seg(SegmentKind::Outro, 90, 110, false)]);
        at(&mut r, g, 95);
        let actions = r.handle(ended(g, EndReason::Eof));
        assert!(matches!(
            actions.as_slice(),
            [
                ReceiverAction::HideSkipButton { .. },
                ReceiverAction::ReportStopped { .. }
            ]
        ));
        assert!(at(&mut r, g, 96).is_empty());
    }

    #[test]
    fn next_item_starts_without_old_segments() {
        let (mut r, g) = playing_with(vec![seg(SegmentKind::Intro, 5, 20, false)]);
        at(&mut r, g, 10);
        r.handle(ended(g, EndReason::Eof));
        let g2 = begin_playing(&mut r, "b", 80);
        settle(&mut r, g2, 0);
        assert!(at(&mut r, g2, 10).is_empty());
    }

    #[test]
    fn secret_debug_is_redacted() {
        let s = format!("{:?}", Secret::new("tok".into()));
        assert!(!s.contains("tok"));
    }
}
