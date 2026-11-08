//! Native mpv C-plugin that makes mpv a Jellyfin cast target.
//!
//! The entry point wraps mpv's handle in [`mpv::Mpv`] and hands it to
//! [`runtime::run`], which drives the event loop until mpv shuts down.

pub mod adapter;
mod drift;
pub mod ffi;
pub mod group_menu;
pub mod mpv;
pub mod runtime;
pub mod skip_button;

use std::ffi::c_int;
use std::panic::{self, AssertUnwindSafe};

use ffi::MpvHandle;
use mpv::Mpv;

/// Native mpv C-plugin entry point.
///
/// mpv owns `handle`; this function must not destroy it and must remain active
/// until the host emits `MPV_EVENT_SHUTDOWN`.
///
/// # Safety
///
/// Only mpv calls this, with its live client handle, after filling the
/// `pfn_mpv_*` slots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mpv_open_cplugin(handle: *mut MpvHandle) -> c_int {
    if handle.is_null() {
        return -1;
    }

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: per this function's contract the handle is live for this
        // call, and `run` consumes the `Mpv`, so it is dropped before we return.
        let Some(mpv) = (unsafe { Mpv::new(handle) }) else {
            eprintln!("[mpvfin] mpv did not initialize the required C-plugin symbols");
            return -1;
        };
        runtime::run(mpv)
    }));
    result.unwrap_or_else(|_| {
        eprintln!("[mpvfin] plugin panicked; refusing to unwind across the C boundary");
        -1
    })
}
