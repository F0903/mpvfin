//! Starting the plugin: what to observe, the session, and the event loop.

use std::collections::HashMap;
use std::ffi::c_int;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};

use super::mpv::Mpv;
use super::mpv::ffi::{MPV_FORMAT_NONE, MPV_FORMAT_STRING, MpvHandle};
use super::overlays::UOSC_MENU;
use crate::receiver::Receiver;
use crate::runtime::{Observed, PendingSeek, Runtime};
use crate::server::session::{self, Paths};
use crate::watch_together::drift::DriftCorrector;

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
pub(super) fn run(mpv: Mpv) -> c_int {
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
