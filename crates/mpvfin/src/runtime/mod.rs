//! The plugin thread's event loop: mpv and session events go to the reducer,
//! and its actions go back to mpv and the session.
//!
//! `mpv` is private to this module: nothing else in the plugin can reach mpv.

mod actions;
mod event;
mod group;
mod load;
mod mpv;
mod overlays;
mod playback;
mod playlist;

use std::collections::HashMap;
use std::ffi::c_int;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use jellyfin::{ResolvedMedia, SegmentKind, StreamSelection};

use self::event::{Event, key_press_name};
use self::load::{SubtitleWait, end_reason};
use self::mpv::Mpv;
use self::mpv::ffi::{MPV_FORMAT_NONE, MPV_FORMAT_STRING};
use self::overlays::{SKIP_LINGER, UOSC_MENU};
use crate::next_up::thumbnail::Thumbnail;
use crate::receiver::{Generation, Receiver, ReceiverEvent};
use crate::server::session::{self, Paths, SessionEvent, SessionHandle};
use crate::watch_together::drift::DriftCorrector;
use crate::watch_together::menu::{self, ConsoleEvent, MenuItem, UoscEvent};

/// Interval of the reducer's progress tick.
const TICK: Duration = Duration::from_secs(10);

/// How long a resolve may hold mpv's load hook. mpv cannot quit until the hook
/// continues, so this bounds quitting while the server stalls.
const RESOLVE_DEADLINE: Duration = Duration::from_secs(4);

/// The same while grouped, where a failed resolve ejects the viewer: a resolve
/// is two requests and one alone can take 3.6 s. Quitting can take this long.
const GROUP_RESOLVE_DEADLINE: Duration = Duration::from_secs(12);

/// How long a message about the group stays on the OSD.
const MESSAGE_DURATION: Duration = Duration::from_secs(5);

/// A group seek held back by the debounce.
#[derive(Default)]
struct PendingSeek(Option<(Instant, Duration)>);

pub(crate) use mpv::ffi::MpvHandle;

/// Starts the plugin on mpv's handle and runs it until mpv shuts down.
///
/// # Safety
///
/// `handle` must be mpv's live client handle, for the length of this call,
/// with the `pfn_mpv_*` slots filled.
pub unsafe fn open(handle: *mut MpvHandle) -> c_int {
    // SAFETY: per this function's contract the handle is live for this
    // call, and `run` consumes the `Mpv`, so it is dropped before we return.
    let Some(mpv) = (unsafe { Mpv::new(handle) }) else {
        eprintln!("[mpvfin] mpv did not initialize the required C-plugin symbols");
        return -1;
    };
    run(mpv)
}

/// Runs the plugin until mpv shuts down; the entry point's return value.
fn run(mpv: Mpv) -> c_int {
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
        "duration",
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
    let session = session::start(
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
        next_up: None,
        next_up_sections_off_at: None,
        resume_at: None,
        pending_seek: PendingSeek::default(),
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
    duration: Option<Duration>,
}

/// The skip button on screen.
#[derive(Clone, Copy)]
struct SkipButton {
    kind: SegmentKind,
    hovered: bool,
}

/// The Next Up card, from `ShowNextUp` until `HideNextUp`.
#[derive(Default)]
struct NextUp {
    /// The next playlist entry, while it is a Jellyfin item.
    item_id: Option<String>,
    /// What the card says; the card is on screen once this is known.
    label: Option<String>,
    /// mpv has this memory's address while the card is on screen.
    thumbnail: Option<Thumbnail>,
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
    next_up: Option<NextUp>,
    /// When the card's input sections go off; `None` leaves them as they are.
    next_up_sections_off_at: Option<Instant>,
    /// When a group's scheduled start is due, for that generation.
    resume_at: Option<(Instant, Generation)>,
    pending_seek: PendingSeek,
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
            let wake = [
                resolve,
                self.skip_sections_off_at,
                self.next_up_sections_off_at,
                resume,
                self.pending_seek.due_at(),
            ]
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
                        let reason = end_reason(reason, Some(self.mpv.error_string(error)));
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
            self.expire_next_up_sections();
            self.expire_resume();
            self.expire_group_seek();
            if Instant::now() >= next_tick {
                next_tick = Instant::now() + TICK;
                self.feed(ReceiverEvent::ProgressTick);
                self.refresh_next_up();
            }
        }
    }

    fn client_message(&mut self, args: &[String]) {
        match key_press_name(args) {
            Some("skip") => {
                let had_button = self.skip_button.is_some();
                self.feed(ReceiverEvent::SkipRequested);
                if had_button && self.skip_button.is_none() {
                    self.skip_sections_off_at = Some(Instant::now() + SKIP_LINGER);
                }
            }
            // Only with a card up: without one there may be no next entry to go to.
            Some("next-up") if self.next_up_on_screen() => {
                self.feed(ReceiverEvent::NextUpRequested);
                if self.next_up.is_none() {
                    self.next_up_sections_off_at = Some(Instant::now() + SKIP_LINGER);
                }
            }
            Some("watch-together") => self.request_group_menu(),
            _ => {}
        }
        // A close needs nothing: it can be the echo of our own close-menu,
        // landing after a newer menu opened, and the items wait for the next one.
        if let [name, json] = args
            && name == menu::UOSC_MESSAGE
            && let Some(UoscEvent::Activate(index)) = menu::parse_uosc_event(json)
        {
            self.run(&menu::uosc_close_command());
            self.menu_chosen(index);
        }
        if let Some(ConsoleEvent::Submit(index)) = menu::parse_console_event(args) {
            self.menu_chosen(index);
        }
    }

    /// mpv sends SHUTDOWN only after every hook continued, so none is held
    /// here (the resolve deadlines bound that wait); this queues the final stop report.
    fn shutdown(mut self) -> c_int {
        // A held group seek is dropped on purpose: moving the group as we leave it helps no one.
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
                SessionEvent::NextUpCard {
                    item_id,
                    label,
                    thumbnail,
                } => self.next_up_ready(&item_id, label, thumbnail),
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
        if !grouped {
            // Out of the group a held seek has no one to go to; the server
            // would answer NotInGroup and the viewer would be told so.
            self.pending_seek = PendingSeek::default();
        }
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
