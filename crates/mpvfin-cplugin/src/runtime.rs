//! The plugin thread's event loop: mpv and session events go to the reducer,
//! and its actions go back to mpv and the session.

use std::collections::HashMap;
use std::ffi::{c_char, c_int};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use mpvfin_core::{
    Generation, PlayerToken, Receiver, ReceiverAction, ReceiverEvent, ResolvedMedia, StopReason,
    StreamSelection,
};
use mpvfin_jellyfin::{Job, Paths, Report, SessionEvent, SessionHandle};

use crate::adapter::{self, TrackKind};
use crate::ffi::{
    MPV_EVENT_END_FILE, MPV_EVENT_FILE_LOADED, MPV_EVENT_HOOK, MPV_EVENT_PLAYBACK_RESTART,
    MPV_EVENT_PROPERTY_CHANGE, MPV_EVENT_SHUTDOWN, MPV_EVENT_START_FILE, MPV_FORMAT_STRING,
    MpvEvent, MpvEventEndFile, MpvEventHook, MpvEventProperty, MpvEventStartFile,
};
use crate::mpv::{Mpv, from_c};

/// Interval of the reducer's progress tick.
const TICK: Duration = Duration::from_secs(10);

/// How long a resolve may hold mpv's load hook. mpv cannot quit until the hook
/// continues, so this bounds quitting while the server stalls.
const RESOLVE_DEADLINE: Duration = Duration::from_secs(4);

/// Runs the plugin until mpv shuts down; the entry point's return value.
pub fn run(mpv: Mpv) -> c_int {
    mpv.log("loaded");
    for name in ["pause", "volume", "mute", "time-pos", "aid", "sid"] {
        mpv.observe(name, MPV_FORMAT_STRING);
    }
    mpv.hook_add("on_load", 50);
    // Left unexpanded on failure: such a path never exists, so the session
    // reports that no server is configured.
    let path = |file: &str| {
        PathBuf::from(
            mpv.command_string_result(&["expand-path", file])
                .unwrap_or_else(|| file.to_owned()),
        )
    };
    let paths = Paths {
        config_file: path("~~/script-opts/mpvfin.conf"),
        state_file: path("~~/script-opts/mpvfin-state.json"),
    };
    // Under a regular file, so mpv can never create it.
    let watch_later_dir = paths.state_file.join("watch_later").display().to_string();
    let (tx, rx) = mpsc::channel();
    let waker = mpv.waker();
    let session = mpvfin_jellyfin::start(
        paths,
        Arc::new(move |event| {
            let _ = tx.send(event);
            waker.wake();
        }),
    );
    let mut runtime = Runtime {
        mpv,
        session,
        rx,
        receiver: Receiver::new(),
        entries: HashMap::new(),
        current: None,
        observed: Observed::default(),
        pending_resolve: None,
        watch_later_dir,
    };
    runtime.event_loop();
    runtime.shutdown()
}

/// The parts of an mpv event the runtime uses, copied out of mpv's memory.
enum Event {
    StartFile(i64),
    Hook(u64),
    FileLoaded,
    EndFile {
        entry_id: i64,
        reason: c_int,
        error: c_int,
    },
    Restart,
    Property(String, Option<String>),
    Shutdown,
    /// A wakeup, a timeout, or an event the runtime does not use.
    Other,
}

impl Event {
    fn read(event: &MpvEvent) -> Self {
        let data = event.data;
        // SAFETY: client.h fixes `data`'s type for each event id (`as_ref`
        // covers NULL), and mpv keeps it valid until the next wait, which
        // cannot happen while `event` is borrowed.
        unsafe {
            match event.event_id {
                MPV_EVENT_START_FILE => data
                    .cast::<MpvEventStartFile>()
                    .as_ref()
                    .map_or(Self::Other, |e| Self::StartFile(e.playlist_entry_id)),
                // `on_load` is the only hook registered.
                MPV_EVENT_HOOK => data
                    .cast::<MpvEventHook>()
                    .as_ref()
                    .map_or(Self::Other, |e| Self::Hook(e.id)),
                MPV_EVENT_FILE_LOADED => Self::FileLoaded,
                MPV_EVENT_END_FILE => {
                    data.cast::<MpvEventEndFile>()
                        .as_ref()
                        .map_or(Self::Other, |e| Self::EndFile {
                            entry_id: e.playlist_entry_id,
                            reason: e.reason,
                            error: e.error,
                        })
                }
                MPV_EVENT_PLAYBACK_RESTART => Self::Restart,
                MPV_EVENT_PROPERTY_CHANGE => data
                    .cast::<MpvEventProperty>()
                    .as_ref()
                    .and_then(|p| {
                        // Format NONE: the property is unavailable.
                        let value = (p.format == MPV_FORMAT_STRING)
                            .then(|| p.data.cast::<*const c_char>().as_ref())
                            .flatten()
                            .and_then(|s| from_c(*s));
                        Some(Self::Property(from_c(p.name)?, value))
                    })
                    .unwrap_or(Self::Other),
                MPV_EVENT_SHUTDOWN => Self::Shutdown,
                _ => Self::Other,
            }
        }
    }
}

/// Latest values of the observed properties.
#[derive(Default)]
struct Observed {
    paused: bool,
    volume_percent: u8,
    muted: bool,
    position: Duration,
    aid: Option<String>,
    sid: Option<String>,
}

/// A playlist entry mpvfin resolves, from `ResolveMedia` until its end.
struct Entry {
    generation: Generation,
    /// Media and selection from `ContinueLoad`.
    media: Option<(ResolvedMedia, StreamSelection)>,
}

struct Runtime {
    mpv: Mpv,
    session: SessionHandle,
    rx: mpsc::Receiver<SessionEvent>,
    receiver: Receiver,
    /// mpvfin entries by mpv playlist entry id.
    entries: HashMap<i64, Entry>,
    /// Entry id of the latest `START_FILE`.
    current: Option<i64>,
    observed: Observed,
    pending_resolve: Option<PendingResolve>,
    watch_later_dir: String,
}

/// The latest `ResolveMedia`, until the job thread answers it.
struct PendingResolve {
    generation: Generation,
    deadline: Instant,
    expired: bool,
}

impl Runtime {
    fn event_loop(&mut self) {
        let mut next_tick = Instant::now() + TICK;
        loop {
            let wake = match &self.pending_resolve {
                Some(pending) if !pending.expired => pending.deadline.min(next_tick),
                _ => next_tick,
            };
            let timeout = wake.saturating_duration_since(Instant::now());
            let Some(event) = self.mpv.wait_event(timeout.as_secs_f64()).map(Event::read) else {
                self.mpv.log("mpv_wait_event returned no event");
                return;
            };
            match event {
                Event::StartFile(entry_id) => self.current = Some(entry_id),
                Event::Hook(hook_id) => self.on_load(hook_id),
                Event::FileLoaded => self.file_loaded(),
                Event::EndFile {
                    entry_id,
                    reason,
                    error,
                } => {
                    if let Some(entry) = self.entries.remove(&entry_id) {
                        let reason =
                            adapter::end_reason(reason, Some(self.mpv.error_string(error)));
                        self.feed(ReceiverEvent::PlayerEnded {
                            generation: entry.generation,
                            reason,
                        });
                    }
                }
                Event::Restart => {
                    if let Some(generation) = self.current_generation() {
                        self.feed(ReceiverEvent::PlayerPositionChanged {
                            generation,
                            position: self.observed.position,
                            seek_completed: true,
                        });
                    }
                }
                Event::Property(name, value) => self.property(&name, value),
                Event::Shutdown => return,
                Event::Other => {}
            }
            self.drain();
            self.expire_resolve();
            if Instant::now() >= next_tick {
                next_tick = Instant::now() + TICK;
                self.feed(ReceiverEvent::ProgressTick);
            }
        }
    }

    /// mpv sends SHUTDOWN only after every hook continued, so none is held
    /// here (`RESOLVE_DEADLINE` bounds that wait); this queues the final stop report.
    fn shutdown(mut self) -> c_int {
        self.feed(ReceiverEvent::Shutdown);
        self.mpv.waker().disarm();
        // Jobs run in order and playback needs its resolve answered, so behind
        // a resolve stuck past its deadline there is no report to wait for.
        let stalled = self.pending_resolve.as_ref().is_some_and(|p| p.expired);
        let flush = if stalled {
            Duration::ZERO
        } else {
            Duration::from_secs(2)
        };
        self.session.shutdown(flush);
        // Log lines from the final flush; any other event is too late to act on.
        for event in self.rx.try_iter() {
            if let SessionEvent::Log(line) = event {
                self.mpv.log(&line);
            }
        }
        0
    }

    fn expire_resolve(&mut self) {
        let Some(pending) = &mut self.pending_resolve else {
            return;
        };
        if !pending.expired && Instant::now() >= pending.deadline {
            pending.expired = true;
            let generation = pending.generation;
            self.feed(ReceiverEvent::MediaResolutionFailed {
                generation,
                message: "Jellyfin took too long to respond".to_owned(),
            });
        }
    }

    fn on_load(&mut self, hook_id: u64) {
        let filename = self.mpv.get_property("stream-open-filename");
        match (
            self.current,
            filename.as_deref().and_then(adapter::item_id_from_entry),
        ) {
            (Some(entry_id), Some(item_id)) => self.feed(ReceiverEvent::PlayerOpening {
                token: PlayerToken { entry_id, hook_id },
                item_id: item_id.to_owned(),
            }),
            // Not an mpvfin entry: mpv must not wait for us.
            _ => self.mpv.hook_continue(hook_id),
        }
    }

    fn file_loaded(&mut self) {
        let Some(entry) = self.current.and_then(|id| self.entries.get(&id)) else {
            return;
        };
        let generation = entry.generation;
        let commands = entry
            .media
            .as_ref()
            .map(|(media, selection)| adapter::after_load_commands(media, selection))
            .unwrap_or_default();
        self.feed(ReceiverEvent::PlayerFileLoaded { generation });
        for command in commands {
            self.run(&command);
        }
        for name in ["pause", "volume", "mute", "aid"] {
            if let Some(event) = self.player_event(generation, name) {
                self.feed(event);
            }
        }
    }

    fn property(&mut self, name: &str, value: Option<String>) {
        let o = &mut self.observed;
        match (name, value.as_deref()) {
            ("pause", Some(v)) => o.paused = v == "yes",
            ("mute", Some(v)) => o.muted = v == "yes",
            ("volume", Some(v)) => {
                if let Ok(volume) = v.parse() {
                    o.volume_percent = percent(volume);
                }
            }
            ("time-pos", v) => {
                let position = v
                    .and_then(|v| v.parse::<f64>().ok())
                    .and_then(|secs| Duration::try_from_secs_f64(secs.max(0.0)).ok());
                o.position = position.unwrap_or_default();
                if position.is_none() {
                    // No file: the reducer keeps the last real position for its stop report.
                    return;
                }
            }
            ("aid", _) => o.aid = value,
            ("sid", _) => o.sid = value,
            _ => return,
        }
        if let Some(event) = self
            .current_generation()
            .and_then(|generation| self.player_event(generation, name))
        {
            self.feed(event);
        }
    }

    /// The `Player*Changed` event carrying the latest value of property `name`.
    fn player_event(&self, generation: Generation, name: &str) -> Option<ReceiverEvent> {
        let o = &self.observed;
        Some(match name {
            "pause" => ReceiverEvent::PlayerPauseChanged {
                generation,
                paused: o.paused,
            },
            "volume" => ReceiverEvent::PlayerVolumeChanged {
                generation,
                volume_percent: o.volume_percent,
            },
            "mute" => ReceiverEvent::PlayerMuteChanged {
                generation,
                muted: o.muted,
            },
            "time-pos" => ReceiverEvent::PlayerPositionChanged {
                generation,
                position: o.position,
                seek_completed: false,
            },
            "aid" | "sid" => {
                let tracks = &self.media(generation)?.0.tracks;
                // "no" and "auto" are not track ids, so they map to `None`.
                let id = |value: &Option<String>| value.as_deref()?.parse::<i64>().ok();
                ReceiverEvent::PlayerTracksChanged {
                    generation,
                    audio_stream_index: id(&o.aid).and_then(|id| tracks.jellyfin_audio_index(id)),
                    subtitle_stream_index: id(&o.sid)
                        .and_then(|id| tracks.jellyfin_subtitle_index(id)),
                }
            }
            _ => return None,
        })
    }

    fn current_generation(&self) -> Option<Generation> {
        self.entries
            .get(&self.current?)
            .map(|entry| entry.generation)
    }

    fn media(&self, generation: Generation) -> Option<&(ResolvedMedia, StreamSelection)> {
        let entry = self.entries.values().find(|e| e.generation == generation)?;
        entry.media.as_ref()
    }

    /// Handles everything the session threads queued.
    fn drain(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                SessionEvent::Receiver(event) => {
                    if let ReceiverEvent::MediaResolved { generation, .. }
                    | ReceiverEvent::MediaResolutionFailed { generation, .. } = &event
                        && self
                            .pending_resolve
                            .as_ref()
                            .is_some_and(|p| p.generation == *generation)
                    {
                        self.pending_resolve = None;
                    }
                    self.feed(event);
                }
                SessionEvent::Osd { text, duration } => self.mpv.show_text(&text, duration),
                SessionEvent::Log(line) => self.mpv.log(&line),
            }
        }
    }

    fn feed(&mut self, event: ReceiverEvent) {
        for action in self.receiver.handle(event) {
            self.execute(action);
        }
    }

    fn execute(&mut self, action: ReceiverAction) {
        match action {
            ReceiverAction::QueueItems(request) => {
                for command in adapter::queue_commands(&request) {
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
                };
                self.entries.insert(token.entry_id, entry);
                self.pending_resolve = Some(PendingResolve {
                    generation,
                    deadline: Instant::now() + RESOLVE_DEADLINE,
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
            } => {
                for (name, value) in adapter::hook_properties(&media, &self.watch_later_dir) {
                    self.set(name, &value);
                }
                if let Some(entry) = self
                    .entries
                    .values_mut()
                    .find(|e| e.generation == generation)
                {
                    entry.media = Some((media, selection));
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
                self.run(&["seek", &adapter::format_seconds(position), "absolute"]);
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
            ReceiverAction::PlaybackFailed { message, .. } => {
                self.mpv
                    .show_text(&format!("mpvfin: {message}"), Duration::from_secs(5));
            }
        }
    }

    fn select_track(&self, generation: Generation, kind: TrackKind, index: i32) {
        let command = self
            .media(generation)
            .and_then(|(media, _)| adapter::track_command(media, kind, index));
        match command {
            Some(command) => self.run(&command),
            None => self.mpv.log(&format!(
                "no mpv track for Jellyfin {kind:?} stream {index}"
            )),
        }
    }

    /// Runs an mpv command; a failure is logged.
    fn run<S: AsRef<str>>(&self, args: &[S]) {
        let args: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
        if let Err(e) = self.mpv.command(&args) {
            self.mpv.log(&e);
        }
    }

    /// Sets an mpv property; a failure is logged (without the value).
    fn set(&self, name: &str, value: &str) {
        if let Err(e) = self.mpv.set_property(name, value) {
            self.mpv.log(&e);
        }
    }
}

fn yes_no(flag: bool) -> &'static str {
    if flag { "yes" } else { "no" }
}

/// mpv's volume (up to 130) as a Jellyfin percentage.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped to 0..=100 first
fn percent(volume: f64) -> u8 {
    volume.clamp(0.0, 100.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_is_clamped_and_rounded() {
        assert_eq!(percent(130.0), 100);
        assert_eq!(percent(-1.0), 0);
        assert_eq!(percent(49.6), 50);
    }
}
