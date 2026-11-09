//! Native mpv C-plugin that makes mpv a Jellyfin cast target.
//!
//! The entry point hands mpv's handle to [`runtime::open`], which drives the
//! event loop until mpv shuts down.

mod next_up;
mod receiver;
mod runtime;
mod server;
mod skip_button;
mod watch_together;

use std::ffi::c_int;
use std::panic::{self, AssertUnwindSafe};

use runtime::MpvHandle;

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
        // SAFETY: per this function's contract the handle is live for this call.
        unsafe { runtime::open(handle) }
    }));
    result.unwrap_or_else(|_| {
        eprintln!("[mpvfin] plugin panicked; refusing to unwind across the C boundary");
        -1
    })
}
