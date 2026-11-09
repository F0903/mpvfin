//! The plugin thread's event loop: mpv and session events go to the reducer,
//! and its actions go back to mpv and the session.

use std::collections::HashMap;
use std::ffi::{c_char, c_int};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use mpvfin_core::{
    Generation, GroupEntry, GroupRequest, GroupSummary, PlayerToken, Receiver, ReceiverAction,
    ReceiverEvent, ResolvedMedia, SegmentKind, StopReason, StreamSelection,
};
use mpvfin_jellyfin::{Job, Paths, Report, SessionEvent, SessionHandle};

use crate::adapter::{self, SubtitleStep, SubtitleWait, TrackKind};
use crate::drift::{DriftAction, DriftCorrector};
use crate::ffi::{
    MPV_EVENT_CLIENT_MESSAGE, MPV_EVENT_COMMAND_REPLY, MPV_EVENT_END_FILE, MPV_EVENT_FILE_LOADED,
    MPV_EVENT_HOOK, MPV_EVENT_PLAYBACK_RESTART, MPV_EVENT_PROPERTY_CHANGE, MPV_EVENT_SEEK,
    MPV_EVENT_SHUTDOWN, MPV_EVENT_START_FILE, MPV_FORMAT_NONE, MPV_FORMAT_STRING, MpvEvent,
    MpvEventClientMessage, MpvEventEndFile, MpvEventHook, MpvEventProperty, MpvEventStartFile,
};
use crate::group_menu::{self, ConsoleEvent, MenuItem, UoscEvent};
use crate::mpv::{Mpv, from_c};
use crate::skip_button::{self, CLICK_SECTION, KEY_SECTION};

/// Interval of the reducer's progress tick.
const TICK: Duration = Duration::from_secs(10);

/// How long a resolve may hold mpv's load hook. mpv cannot quit until the hook
/// continues, so this bounds quitting while the server stalls.
const RESOLVE_DEADLINE: Duration = Duration::from_secs(4);

/// How long the skip input stays ours after a skip press, so the second press
/// of a double press can't reach mpv's own bindings.
const SKIP_LINGER: Duration = Duration::from_millis(400);

/// uosc's open-menu marker. It is absent before the first menu, and reads as
/// the string `null` once a menu has closed.
const UOSC_MENU: &str = "user-data/uosc/menu/type";

/// How long a message about the group stays on the OSD.
const MESSAGE_DURATION: Duration = Duration::from_secs(5);

/// Runs the plugin until mpv shuts down; the entry point's return value.
pub fn run(mpv: Mpv) -> c_int {
    mpv.log("loaded");
    for name in [
        "pause",
        "volume",
        "mute",
        "time-pos",
        "aid",
        "sid",
        "osd-width",
        "osd-height",
        "speed",
        "eof-reached",
        "paused-for-cache",
        "idle-active",
    ] {
        mpv.observe(name, MPV_FORMAT_STRING);
    }
    // Only a change signal: the coordinates are read while a button is up.
    for name in ["mouse-pos", UOSC_MENU] {
        mpv.observe(name, MPV_FORMAT_NONE);
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
        subtitle_wait: None,
        skip_button: None,
        skip_sections_off_at: None,
        resume_at: None,
        drift: DriftCorrector::new(),
        drift_generation: None,
        seeking: false,
        group_list_pending: false,
        menu_items: None,
    };
    runtime.define_skip_sections();
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
    Seek,
    Restart,
    Property(String, Option<String>),
    ClientMessage(Vec<String>),
    CommandReply {
        reply_id: u64,
        error: c_int,
    },
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
                MPV_EVENT_SEEK => Self::Seek,
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
                MPV_EVENT_CLIENT_MESSAGE => {
                    data.cast::<MpvEventClientMessage>()
                        .as_ref()
                        .map_or(Self::Other, |m| {
                            let count = usize::try_from(m.num_args).unwrap_or(0);
                            // Non-UTF-8 becomes empty, so positions stay meaningful.
                            Self::ClientMessage(
                                (0..count)
                                    .map(|i| from_c(*m.args.add(i)).unwrap_or_default())
                                    .collect(),
                            )
                        })
                }
                MPV_EVENT_COMMAND_REPLY => Self::CommandReply {
                    reply_id: event.reply_userdata,
                    error: event.error,
                },
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
    /// 0 while unknown.
    osd_width: i64,
    /// 0 while unknown.
    osd_height: i64,
    speed: f64,
    eof_reached: bool,
}

/// The skip button on screen.
#[derive(Clone, Copy)]
struct SkipButton {
    kind: SegmentKind,
    hovered: bool,
}

/// A playlist entry mpvfin resolves, from `ResolveMedia` until its end.
struct Entry {
    generation: Generation,
    /// Media and selection from `ContinueLoad`.
    media: Option<(ResolvedMedia, StreamSelection)>,
    /// `keep-open` before the group hook replaced it; `None` outside a group.
    keep_open_before: Option<String>,
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
    subtitle_wait: Option<SubtitleWait>,
    skip_button: Option<SkipButton>,
    /// When the skip input sections go off; `None` leaves them as they are.
    skip_sections_off_at: Option<Instant>,
    /// When a group's scheduled start is due, for that generation.
    resume_at: Option<(Instant, Generation)>,
    drift: DriftCorrector,
    /// The generation the drift corrector follows.
    drift_generation: Option<Generation>,
    /// Between `SEEK` and `PLAYBACK_RESTART`.
    seeking: bool,
    /// A `ListGroups` is out; the session answers each one, even a failure.
    group_list_pending: bool,
    /// What the watch-together menu last offered, until a choice.
    menu_items: Option<Vec<MenuItem>>,
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
            let resolve = self
                .pending_resolve
                .as_ref()
                .filter(|pending| !pending.expired)
                .map(|pending| pending.deadline);
            let resume = self.resume_at.map(|(at, _)| at);
            let wake = [resolve, self.skip_sections_off_at, resume]
                .into_iter()
                .flatten()
                .fold(next_tick, Instant::min);
            let timeout = wake.saturating_duration_since(Instant::now());
            let Some(event) = self.mpv.wait_event(timeout.as_secs_f64()).map(Event::read) else {
                self.mpv.log("mpv_wait_event returned no event");
                return;
            };
            match event {
                Event::StartFile(entry_id) => {
                    self.current = Some(entry_id);
                    self.seeking = false;
                }
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
                Event::Seek => self.seeking = true,
                Event::Restart => {
                    self.seeking = false;
                    if let Some(generation) = self.current_generation() {
                        // mpv delivers events ahead of property changes, so the
                        // observed position can still be from before the seek.
                        let position = parse_position(self.mpv.get_property("time-pos").as_deref())
                            .unwrap_or(self.observed.position);
                        self.feed(ReceiverEvent::PlayerPositionChanged {
                            generation,
                            position,
                            seek_completed: true,
                        });
                    }
                }
                Event::Property(name, value) => self.property(&name, value),
                Event::ClientMessage(args) => self.client_message(&args),
                Event::CommandReply { reply_id, error } => self.sub_add_reply(reply_id, error),
                Event::Shutdown => return,
                Event::Other => {}
            }
            self.drain();
            self.expire_resolve();
            self.expire_skip_sections();
            self.expire_resume();
            if Instant::now() >= next_tick {
                next_tick = Instant::now() + TICK;
                self.feed(ReceiverEvent::ProgressTick);
            }
        }
    }

    fn client_message(&mut self, args: &[String]) {
        match adapter::key_press_name(args) {
            Some("skip") => {
                let had_button = self.skip_button.is_some();
                self.feed(ReceiverEvent::SkipRequested);
                if had_button && self.skip_button.is_none() {
                    self.skip_sections_off_at = Some(Instant::now() + SKIP_LINGER);
                }
            }
            Some("watch-together") => self.request_group_menu(),
            _ => {}
        }
        // A close needs nothing: it can be the echo of our own close-menu,
        // landing after a newer menu opened, and the items wait for the next one.
        if let [name, json] = args
            && name == group_menu::UOSC_MESSAGE
            && let Some(UoscEvent::Activate(index)) = group_menu::parse_uosc_event(json)
        {
            self.run(&group_menu::uosc_close_command());
            self.menu_chosen(index);
        }
        if let Some(ConsoleEvent::Submit(index)) = group_menu::parse_console_event(args) {
            self.menu_chosen(index);
        }
    }

    /// mpv sends SHUTDOWN only after every hook continued, so none is held
    /// here (`RESOLVE_DEADLINE` bounds that wait); this queues the final stop report.
    fn shutdown(mut self) -> c_int {
        self.feed(ReceiverEvent::Shutdown);
        self.mpv.waker().disarm();
        // shortcut: a stop report queued behind a stalled request is dropped
        // here, losing that item's last position; upgrade if quits during
        // server stalls turn out to matter.
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

    fn expire_skip_sections(&mut self) {
        if self
            .skip_sections_off_at
            .is_some_and(|at| Instant::now() >= at)
        {
            self.skip_sections_off_at = None;
            self.run(&skip_button::set_section_command(KEY_SECTION, false));
            self.run(&skip_button::set_section_command(CLICK_SECTION, false));
        }
    }

    fn expire_resume(&mut self) {
        if let Some((at, generation)) = self.resume_at
            && Instant::now() >= at
        {
            self.resume_at = None;
            self.feed(ReceiverEvent::GroupResumeDue { generation });
        }
    }

    fn on_load(&mut self, hook_id: u64) {
        let filename = self.mpv.get_property("stream-open-filename");
        let entry = filename.as_deref().and_then(adapter::entry_from_uri);
        if let (Some(entry_id), Some((item_id, pid))) = (self.current, entry) {
            self.feed(ReceiverEvent::PlayerOpening {
                token: PlayerToken { entry_id, hook_id },
                item_id: item_id.to_owned(),
                playlist_item_id: pid.map(str::to_owned),
            });
        } else {
            // Not an mpvfin entry: mpv must not wait for us.
            self.mpv.hook_continue(hook_id);
            self.feed(ReceiverEvent::PlayerOpenedForeign);
        }
    }

    fn file_loaded(&mut self) {
        let Some(entry) = self.current.and_then(|id| self.entries.get(&id)) else {
            return;
        };
        let generation = entry.generation;
        let (sub_adds, selection) = entry
            .media
            .as_ref()
            .map(|(media, selection)| {
                (
                    adapter::sub_add_commands(media),
                    adapter::selection_commands(media, selection),
                )
            })
            .unwrap_or_default();
        self.feed(ReceiverEvent::PlayerFileLoaded { generation });
        self.subtitle_wait = Some(SubtitleWait::new(generation, sub_adds, selection));
        self.advance_subtitles();
        for name in ["pause", "volume", "mute", "aid"] {
            if let Some(event) = self.player_event(generation, name) {
                self.feed(event);
            }
        }
    }

    /// Issues the next `sub-add` off the plugin thread (a stalled subtitle must
    /// not freeze it), or applies the selection once none remain.
    fn advance_subtitles(&mut self) {
        while let Some(wait) = self.subtitle_wait.as_mut() {
            let generation = wait.generation();
            match wait.step() {
                SubtitleStep::Add(command) => {
                    let args: Vec<&str> = command.iter().map(String::as_str).collect();
                    match self.mpv.command_async(generation.get(), &args) {
                        Ok(()) => return,
                        Err(e) => self.mpv.log(&e),
                    }
                }
                SubtitleStep::Select(selection) => {
                    self.subtitle_wait = None;
                    for command in selection {
                        self.run(&command);
                    }
                }
            }
        }
    }

    fn sub_add_reply(&mut self, reply_id: u64, error: c_int) {
        if error < 0 {
            self.mpv
                .log(&format!("sub-add: {}", self.mpv.error_string(error)));
        }
        let current = self.current_generation();
        if self
            .subtitle_wait
            .as_ref()
            .is_some_and(|wait| wait.accepts(reply_id) && Some(wait.generation()) == current)
        {
            self.advance_subtitles();
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
                let position = parse_position(v);
                o.position = position.unwrap_or_default();
                if position.is_none() {
                    // No file: the reducer keeps the last real position for its stop report.
                    return;
                }
            }
            ("aid", _) => o.aid = value,
            ("sid", _) => o.sid = value,
            ("osd-width", v) => {
                o.osd_width = osd_length(v);
                return self.skip_button_resized();
            }
            ("osd-height", v) => {
                o.osd_height = osd_length(v);
                return self.skip_button_resized();
            }
            ("mouse-pos" | UOSC_MENU, _) => return self.update_skip_hover(),
            ("speed", v) => {
                if let Some(speed) = v.and_then(|v| v.parse().ok()) {
                    o.speed = speed;
                }
                return;
            }
            ("eof-reached", v) => {
                o.eof_reached = v == Some("yes");
                if o.eof_reached
                    && let Some(generation) = self.current_generation()
                {
                    self.feed(self.end_event(generation));
                }
                return;
            }
            ("paused-for-cache", v) => {
                if let Some(generation) = self.current_generation() {
                    self.feed(ReceiverEvent::PlayerBuffering {
                        generation,
                        buffering: v == Some("yes"),
                    });
                }
                return;
            }
            ("idle-active", Some("yes")) => return self.feed(ReceiverEvent::PlayerIdle),
            _ => return,
        }
        if let Some(event) = self
            .current_generation()
            .and_then(|generation| self.player_event(generation, name))
        {
            self.feed(event);
        }
        if name == "time-pos" {
            self.correct_drift();
        }
    }

    fn end_event(&self, generation: Generation) -> ReceiverEvent {
        // mpv delivers events ahead of property changes, so ask for the position.
        let position = parse_position(self.mpv.get_property("time-pos").as_deref())
            .unwrap_or(self.observed.position);
        let duration = parse_position(self.mpv.get_property("duration").as_deref());
        if adapter::is_true_end(position, duration) {
            ReceiverEvent::PlayerReachedEnd { generation }
        } else {
            ReceiverEvent::PlayerStoppedEarly { generation }
        }
    }

    /// Not while a seek settles (`time-pos` is at the target before `playback-restart`,
    /// so a viewer's seek would read as drift), nor while paused or held at the end.
    fn correct_drift(&mut self) {
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

    fn apply_drift(&mut self, generation: Generation, action: DriftAction) {
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
                SessionEvent::GroupList(groups) => {
                    self.group_list_pending = false;
                    self.open_group_menu(&groups);
                }
                SessionEvent::GroupListFailed => self.group_list_pending = false,
            }
        }
    }

    fn feed(&mut self, event: ReceiverEvent) {
        let was_grouped = self.receiver.group_id().is_some();
        for action in self.receiver.handle(event) {
            self.execute(action);
        }
        let grouped = self.receiver.group_id().is_some();
        if grouped != was_grouped {
            self.apply_group_keep_open(grouped);
        }
    }

    /// The group hook makes its entry wait at the end for the server. Out of
    /// the group nothing would move it on, so it gets the viewer's value back;
    /// rejoining with the same entry still open makes it wait again.
    fn apply_group_keep_open(&self, grouped: bool) {
        let entry = self.current.and_then(|id| self.entries.get(&id));
        if let Some(before) = entry.and_then(|entry| entry.keep_open_before.as_deref()) {
            let value = if grouped {
                adapter::GROUP_KEEP_OPEN
            } else {
                before
            };
            self.set("file-local-options/keep-open", value);
        }
    }

    /// A press while a list is on its way does nothing, or two menus would open.
    fn request_group_menu(&mut self) {
        if !self.group_list_pending {
            self.group_list_pending = true;
            self.session.submit(Job::ListGroups);
        }
    }

    fn open_group_menu(&mut self, groups: &[GroupSummary]) {
        let Some(client) = self.mpv.client_name() else {
            self.mpv.log("mpv reported no client name: no menu");
            return;
        };
        let items = group_menu::menu_items(groups, self.receiver.group_id());
        let opened = if self.accepted(&group_menu::uosc_probe_command()) {
            self.accepted(&group_menu::uosc_open_command(&items, &client))
        } else {
            let legacy = self
                .mpv
                .get_property("mpv-version")
                .is_some_and(|version| group_menu::console_is_legacy(&version));
            self.accepted(&group_menu::console_open_command(
                &items,
                &client,
                group_menu::CONSOLE_HANDLER,
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
    fn menu_chosen(&mut self, index: usize) {
        let chosen = self.menu_items.take();
        let Some(item) = chosen.and_then(|items| items.into_iter().nth(index)) else {
            return;
        };
        let grouped = self.receiver.group_id().is_some();
        let device_name = mpvfin_jellyfin::device_name();
        for event in group_menu::choice_events(item.choice, grouped, &device_name) {
            self.feed(event);
        }
    }

    // One arm per action keeps the adapter auditable against the reducer.
    #[allow(clippy::too_many_lines)]
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
                    keep_open_before: None,
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
                group,
            } => {
                let keep_open_before = group
                    .is_some()
                    .then(|| self.mpv.get_property("keep-open"))
                    .flatten();
                let group_properties = group
                    .map(|options| adapter::group_hook_properties(options.start))
                    .unwrap_or_default();
                for (name, value) in adapter::hook_properties(&media, &self.watch_later_dir)
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
                self.run(&adapter::seek_command(position));
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
            ReceiverAction::Group(request) => self.session.submit(Job::Group(request)),
            ReceiverAction::GroupLoad { entries, index } => self.group_load(&entries, index),
            ReceiverAction::GroupSyncPlaylist { entries, index } => {
                for command in adapter::group_sync_commands(&entries, index) {
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

    /// If mpv refuses the entry no hook will ever fire, and the server would
    /// hold the whole group for a Ready that can't come.
    fn group_load(&mut self, entries: &[GroupEntry], index: usize) {
        let commands = adapter::group_load_commands(entries, index);
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

    fn publish_queue(&self, position: Duration) {
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
        let (item_ids, index) = adapter::published_queue(&filenames, current);
        self.session.submit(Job::Group(GroupRequest::SetNewQueue {
            item_ids,
            index,
            start: position,
        }));
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

    /// Defines the button's input sections; they stay inert until enabled.
    fn define_skip_sections(&self) {
        let Some(client) = self.mpv.client_name() else {
            self.mpv.log("mpv reported no client name: no skip input");
            return;
        };
        for command in skip_button::define_section_commands(&client) {
            self.run(&command);
        }
    }

    fn show_skip_button(&mut self, kind: SegmentKind) {
        // A pending disable ends here; the click section must follow the pointer again.
        if self.skip_sections_off_at.take().is_some() {
            self.run(&skip_button::set_section_command(CLICK_SECTION, false));
        }
        self.skip_button = Some(SkipButton {
            kind,
            hovered: false,
        });
        self.run(&skip_button::set_section_command(KEY_SECTION, true));
        self.draw_skip_button();
        // The pointer may already be there, and a still pointer sends no change.
        self.update_skip_hover();
    }

    fn hide_skip_button(&mut self) {
        self.skip_button = None;
        self.run(&skip_button::clear_command());
        // At once, unless the press that hid the button pushes this back.
        self.skip_sections_off_at = Some(Instant::now());
    }

    fn draw_skip_button(&self) {
        if let Some(button) = self.skip_button {
            let (w, h) = osd_size(&self.observed);
            self.run(&skip_button::draw_command(
                button.kind,
                w,
                h,
                button.hovered,
            ));
        }
    }

    fn skip_button_resized(&mut self) {
        self.draw_skip_button();
        self.update_skip_hover();
    }

    /// Rereads the pointer while a button is up; when it crosses the button's
    /// edge, moves the click section and redraws.
    fn update_skip_hover(&mut self) {
        let Some(button) = self.skip_button else {
            return;
        };
        // uosc's menu draws over the button and owns the click there.
        let menu_open = self
            .mpv
            .get_property(UOSC_MENU)
            .is_some_and(|menu| menu != "null");
        let hovered = !menu_open && self.pointer_over_skip_button();
        if hovered != button.hovered {
            self.skip_button = Some(SkipButton { hovered, ..button });
            self.run(&skip_button::set_section_command(CLICK_SECTION, hovered));
            self.draw_skip_button();
        }
    }

    fn pointer_over_skip_button(&self) -> bool {
        let coordinate = |name| self.mpv.get_property(name)?.parse::<i64>().ok();
        let (w, h) = osd_size(&self.observed);
        self.mpv.get_property("mouse-pos/hover").as_deref() == Some("yes")
            && coordinate("mouse-pos/x")
                .zip(coordinate("mouse-pos/y"))
                .is_some_and(|(x, y)| skip_button::button_rect(w, h).contains(x, y))
    }

    /// Runs an mpv command; a failure is logged.
    fn run<S: AsRef<str>>(&self, args: &[S]) {
        self.try_run(args);
    }

    /// As [`Self::run`], and says whether the command succeeded.
    fn try_run<S: AsRef<str>>(&self, args: &[S]) -> bool {
        let args: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
        let result = self.mpv.command(&args);
        if let Err(e) = &result {
            self.mpv.log(e);
        }
        result.is_ok()
    }

    /// Whether mpv takes the command; a refusal here is an answer, not a failure to log.
    fn accepted(&self, args: &[String]) -> bool {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.mpv.command(&args).is_ok()
    }

    /// Sets an mpv property; a failure is logged (without the value).
    fn set(&self, name: &str, value: &str) {
        if let Err(e) = self.mpv.set_property(name, value) {
            self.mpv.log(&e);
        }
    }
}

/// A `time-pos` value; `None` when unavailable.
fn parse_position(value: Option<&str>) -> Option<Duration> {
    value
        .and_then(|v| v.parse::<f64>().ok())
        .and_then(|secs| Duration::try_from_secs_f64(secs.max(0.0)).ok())
}

fn yes_no(flag: bool) -> &'static str {
    if flag { "yes" } else { "no" }
}

/// An `osd-width` or `osd-height` value; 0 when missing or unparsable.
fn osd_length(value: Option<&str>) -> i64 {
    value.and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// The OSD size to lay the button out on: without a video output mpv
/// reports none, and the hit test must still work.
fn osd_size(observed: &Observed) -> (i64, i64) {
    match (observed.osd_width, observed.osd_height) {
        (w, h) if w > 0 && h > 0 => (w, h),
        _ => (1280, 720),
    }
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
    fn a_missing_or_zero_osd_counts_as_720p() {
        let size = |osd_width, osd_height| {
            osd_size(&Observed {
                osd_width,
                osd_height,
                ..Observed::default()
            })
        };
        assert_eq!(size(1920, 1080), (1920, 1080));
        assert_eq!(size(0, 0), (1280, 720));
        assert_eq!(size(1920, 0), (1280, 720));
        assert_eq!(osd_length(None), 0);
        assert_eq!(osd_length(Some("1920")), 1920);
    }

    #[test]
    fn volume_is_clamped_and_rounded() {
        assert_eq!(percent(130.0), 100);
        assert_eq!(percent(-1.0), 0);
        assert_eq!(percent(49.6), 50);
    }
}
