//! Minimal native mpv C-plugin entry point.
//!
//! This slice proves that the host can initialize mpv's dynamic symbol table,
//! call the Rust DLL entry point, and drive the plugin event loop. The event
//! loop built on [`mpv`] and [`adapter`] comes next.

pub mod adapter;
pub mod ffi;
pub mod mpv;

use std::ffi::c_int;
use std::panic::{self, AssertUnwindSafe};

use ffi::{MPV_EVENT_SHUTDOWN, MpvHandle};

/// Native mpv C-plugin entry point.
///
/// mpv owns `handle`; this function must not destroy it and must remain active
/// until the host emits `MPV_EVENT_SHUTDOWN`.
#[unsafe(no_mangle)]
pub extern "C" fn mpv_open_cplugin(handle: *mut MpvHandle) -> c_int {
    if handle.is_null() {
        return -1;
    }

    if let Ok(result) = panic::catch_unwind(AssertUnwindSafe(|| run_plugin(handle))) {
        result
    } else {
        eprintln!("[mpvfin] plugin panicked; refusing to unwind across the C boundary");
        -1
    }
}

fn run_plugin(handle: *mut MpvHandle) -> c_int {
    // SAFETY: mpv writes these function pointers before invoking the plugin
    // entry point and never mutates them afterward. We copy them once on the
    // plugin thread and do not retain references to the mutable statics.
    let (wait_event, command_string, client_name) = unsafe {
        (
            ffi::pfn_mpv_wait_event,
            ffi::pfn_mpv_command_string,
            ffi::pfn_mpv_client_name,
        )
    };
    let (Some(wait_event), Some(command_string), Some(client_name)) =
        (wait_event, command_string, client_name)
    else {
        eprintln!("[mpvfin] mpv did not initialize the required C-plugin symbols");
        return -1;
    };

    // mpv may wait for a plugin's first event poll before allowing playback to
    // continue. Poll immediately before doing any setup work.
    // SAFETY: `handle` is non-null and owned by mpv for this call's lifetime;
    // the function pointer was initialized by the mpv loader.
    let initial_event = unsafe { wait_event(handle, 0.0) };
    if initial_event.is_null() {
        eprintln!("[mpvfin] mpv_wait_event unexpectedly returned null");
        return -1;
    }

    // SAFETY: same host-owned handle and loader-initialized function pointer.
    let name = unsafe { client_name(handle) };
    if name.is_null() {
        eprintln!("[mpvfin] native C-plugin loaded");
    } else {
        // Avoid decoding a foreign string in the smoke-test slice. The pointer
        // check still proves that the symbol and handle work together.
        eprintln!("[mpvfin] native C-plugin loaded with an mpv client handle");
    }

    // SAFETY: the command is a static, NUL-terminated C string.
    let result = unsafe {
        command_string(
            handle,
            c"show-text \"mpvfin native plugin loaded\" 3000".as_ptr(),
        )
    };
    if result < 0 {
        eprintln!("[mpvfin] mpv rejected the smoke-test OSD command ({result})");
    }

    loop {
        // SAFETY: only this thread calls `mpv_wait_event` for this handle, and
        // mpv keeps both the handle and returned event alive until the next
        // call.
        let event = unsafe { wait_event(handle, -1.0) };
        if event.is_null() {
            eprintln!("[mpvfin] mpv_wait_event unexpectedly returned null");
            return -1;
        }

        // SAFETY: mpv documents a non-null event pointer valid until the next
        // wait call.
        if unsafe { (*event).event_id } == MPV_EVENT_SHUTDOWN {
            return 0;
        }
    }
}
