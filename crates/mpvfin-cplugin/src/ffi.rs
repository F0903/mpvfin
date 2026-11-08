//! Raw mpv client API items.
//!
//! Constants and struct layouts are copied from mpv's public
//! `include/mpv/client.h` (client API 2.6, mpv commit 4255130). Event structs
//! are prefixes: only the leading fields this plugin reads are declared, and
//! the plugin only ever reads them through pointers mpv hands out.

use std::ffi::{c_char, c_int, c_void};

/// Opaque mpv client handle owned by the host.
#[repr(C)]
pub struct MpvHandle {
    _private: [u8; 0],
}

// enum mpv_format
pub const MPV_FORMAT_NONE: c_int = 0;
pub const MPV_FORMAT_STRING: c_int = 1;
pub const MPV_FORMAT_OSD_STRING: c_int = 2;
pub const MPV_FORMAT_FLAG: c_int = 3;
pub const MPV_FORMAT_INT64: c_int = 4;
pub const MPV_FORMAT_DOUBLE: c_int = 5;
pub const MPV_FORMAT_NODE: c_int = 6;
pub const MPV_FORMAT_NODE_ARRAY: c_int = 7;
pub const MPV_FORMAT_NODE_MAP: c_int = 8;
pub const MPV_FORMAT_BYTE_ARRAY: c_int = 9;

// enum mpv_event_id
pub const MPV_EVENT_NONE: c_int = 0;
pub const MPV_EVENT_SHUTDOWN: c_int = 1;
pub const MPV_EVENT_LOG_MESSAGE: c_int = 2;
pub const MPV_EVENT_GET_PROPERTY_REPLY: c_int = 3;
pub const MPV_EVENT_SET_PROPERTY_REPLY: c_int = 4;
pub const MPV_EVENT_COMMAND_REPLY: c_int = 5;
pub const MPV_EVENT_START_FILE: c_int = 6;
pub const MPV_EVENT_END_FILE: c_int = 7;
pub const MPV_EVENT_FILE_LOADED: c_int = 8;
pub const MPV_EVENT_IDLE: c_int = 11;
pub const MPV_EVENT_TICK: c_int = 14;
pub const MPV_EVENT_CLIENT_MESSAGE: c_int = 16;
pub const MPV_EVENT_VIDEO_RECONFIG: c_int = 17;
pub const MPV_EVENT_AUDIO_RECONFIG: c_int = 18;
pub const MPV_EVENT_SEEK: c_int = 20;
pub const MPV_EVENT_PLAYBACK_RESTART: c_int = 21;
pub const MPV_EVENT_PROPERTY_CHANGE: c_int = 22;
pub const MPV_EVENT_QUEUE_OVERFLOW: c_int = 24;
pub const MPV_EVENT_HOOK: c_int = 25;

// enum mpv_end_file_reason
pub const MPV_END_FILE_REASON_EOF: c_int = 0;
pub const MPV_END_FILE_REASON_STOP: c_int = 2;
pub const MPV_END_FILE_REASON_QUIT: c_int = 3;
pub const MPV_END_FILE_REASON_ERROR: c_int = 4;
pub const MPV_END_FILE_REASON_REDIRECT: c_int = 5;

/// `mpv_event`.
#[repr(C)]
pub struct MpvEvent {
    pub event_id: c_int,
    pub error: c_int,
    pub reply_userdata: u64,
    pub data: *mut c_void,
}

/// `mpv_event_property`.
#[repr(C)]
pub struct MpvEventProperty {
    pub name: *const c_char,
    pub format: c_int,
    pub data: *mut c_void,
}

/// `mpv_event_start_file`.
#[repr(C)]
pub struct MpvEventStartFile {
    pub playlist_entry_id: i64,
}

/// Prefix of `mpv_event_end_file`.
#[repr(C)]
pub struct MpvEventEndFile {
    pub reason: c_int,
    pub error: c_int,
    pub playlist_entry_id: i64,
}

/// `mpv_event_hook`.
#[repr(C)]
pub struct MpvEventHook {
    pub name: *const c_char,
    pub id: u64,
}

/// `mpv_event_client_message`.
#[repr(C)]
pub struct MpvEventClientMessage {
    pub num_args: c_int,
    pub args: *const *const c_char,
}

/// `mpv_node`; only `u.string` is ever read.
#[repr(C)]
pub struct MpvNode {
    pub u: MpvNodeValue,
    pub format: c_int,
}

/// The anonymous union `mpv_node.u`.
#[repr(C)]
pub union MpvNodeValue {
    pub string: *mut c_char,
    pub flag: c_int,
    pub int64: i64,
    pub double_: f64,
    pub list: *mut c_void,
    pub ba: *mut c_void,
}

// Guard against field typos: offsets and sizes as a C compiler lays them out.
#[cfg(target_pointer_width = "64")]
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<MpvEvent>() == 24 && offset_of!(MpvEvent, data) == 16);
    assert!(offset_of!(MpvEventProperty, data) == 16);
    assert!(offset_of!(MpvEventEndFile, playlist_entry_id) == 8);
    assert!(offset_of!(MpvEventHook, id) == 8);
    assert!(offset_of!(MpvEventClientMessage, args) == 8);
    assert!(size_of::<MpvNode>() == 16 && offset_of!(MpvNode, format) == 8);
};

pub type MpvWaitEventFn = unsafe extern "C" fn(*mut MpvHandle, f64) -> *mut MpvEvent;
pub type MpvWakeupFn = unsafe extern "C" fn(*mut MpvHandle);
pub type MpvCommandFn = unsafe extern "C" fn(*mut MpvHandle, *mut *const c_char) -> c_int;
pub type MpvCommandStringFn = unsafe extern "C" fn(*mut MpvHandle, *const c_char) -> c_int;
pub type MpvCommandAsyncFn = unsafe extern "C" fn(*mut MpvHandle, u64, *mut *const c_char) -> c_int;
pub type MpvCommandRetFn =
    unsafe extern "C" fn(*mut MpvHandle, *mut *const c_char, *mut MpvNode) -> c_int;
pub type MpvFreeNodeContentsFn = unsafe extern "C" fn(*mut MpvNode);
pub type MpvSetPropertyStringFn =
    unsafe extern "C" fn(*mut MpvHandle, *const c_char, *const c_char) -> c_int;
pub type MpvGetPropertyStringFn =
    unsafe extern "C" fn(*mut MpvHandle, *const c_char) -> *mut c_char;
pub type MpvFreeFn = unsafe extern "C" fn(*mut c_void);
pub type MpvObservePropertyFn =
    unsafe extern "C" fn(*mut MpvHandle, u64, *const c_char, c_int) -> c_int;
pub type MpvHookAddFn = unsafe extern "C" fn(*mut MpvHandle, u64, *const c_char, c_int) -> c_int;
pub type MpvHookContinueFn = unsafe extern "C" fn(*mut MpvHandle, u64) -> c_int;
pub type MpvClientNameFn = unsafe extern "C" fn(*mut MpvHandle) -> *const c_char;
pub type MpvErrorStringFn = unsafe extern "C" fn(c_int) -> *const c_char;

// mpv initializes exported `pfn_mpv_*` slots before calling
// `mpv_open_cplugin`. This mirrors the public `MPV_CPLUGIN_DYNAMIC_SYM`
// contract without linking the plugin to a particular mpv.exe or libmpv DLL.
#[unsafe(no_mangle)]
pub static mut pfn_mpv_wait_event: Option<MpvWaitEventFn> = None;
#[unsafe(no_mangle)]
pub static mut pfn_mpv_wakeup: Option<MpvWakeupFn> = None;
#[unsafe(no_mangle)]
pub static mut pfn_mpv_command: Option<MpvCommandFn> = None;
#[unsafe(no_mangle)]
pub static mut pfn_mpv_command_string: Option<MpvCommandStringFn> = None;
#[unsafe(no_mangle)]
pub static mut pfn_mpv_command_async: Option<MpvCommandAsyncFn> = None;
#[unsafe(no_mangle)]
pub static mut pfn_mpv_command_ret: Option<MpvCommandRetFn> = None;
#[unsafe(no_mangle)]
pub static mut pfn_mpv_free_node_contents: Option<MpvFreeNodeContentsFn> = None;
#[unsafe(no_mangle)]
pub static mut pfn_mpv_set_property_string: Option<MpvSetPropertyStringFn> = None;
#[unsafe(no_mangle)]
pub static mut pfn_mpv_get_property_string: Option<MpvGetPropertyStringFn> = None;
#[unsafe(no_mangle)]
pub static mut pfn_mpv_free: Option<MpvFreeFn> = None;
#[unsafe(no_mangle)]
pub static mut pfn_mpv_observe_property: Option<MpvObservePropertyFn> = None;
#[unsafe(no_mangle)]
pub static mut pfn_mpv_hook_add: Option<MpvHookAddFn> = None;
#[unsafe(no_mangle)]
pub static mut pfn_mpv_hook_continue: Option<MpvHookContinueFn> = None;
#[unsafe(no_mangle)]
pub static mut pfn_mpv_client_name: Option<MpvClientNameFn> = None;
#[unsafe(no_mangle)]
pub static mut pfn_mpv_error_string: Option<MpvErrorStringFn> = None;
