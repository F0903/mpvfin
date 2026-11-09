//! Safe wrapper over the mpv client API.

pub mod ffi;

use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::{CStr, CString, c_char, c_int};
use std::ptr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use self::ffi::{
    MPV_EVENT_HOOK, MPV_FORMAT_NONE, MPV_FORMAT_STRING, MpvClientNameFn, MpvCommandAsyncFn,
    MpvCommandFn, MpvCommandRetFn, MpvErrorStringFn, MpvEvent, MpvEventHook, MpvFreeFn,
    MpvFreeNodeContentsFn, MpvGetPropertyStringFn, MpvHandle, MpvHookAddFn, MpvHookContinueFn,
    MpvNode, MpvNodeValue, MpvObservePropertyFn, MpvSetPropertyStringFn, MpvWaitEventFn,
    MpvWakeupFn,
};

/// The plugin's mpv client handle.
///
/// The raw handle makes this `!Send`: it stays on the plugin thread, the only
/// thread allowed to call `mpv_wait_event`. Other threads use a [`Waker`].
pub struct Mpv {
    handle: *mut MpvHandle,
    wait_event: MpvWaitEventFn,
    command: MpvCommandFn,
    command_async: MpvCommandAsyncFn,
    command_ret: MpvCommandRetFn,
    free_node_contents: MpvFreeNodeContentsFn,
    set_property_string: MpvSetPropertyStringFn,
    get_property_string: MpvGetPropertyStringFn,
    free: MpvFreeFn,
    observe_property: MpvObservePropertyFn,
    hook_add: MpvHookAddFn,
    hook_continue: MpvHookContinueFn,
    client_name: MpvClientNameFn,
    error_string: MpvErrorStringFn,
    pending_hooks: PendingHooks,
    waker: Waker,
}

impl Mpv {
    /// Wraps `handle` with the `pfn_mpv_*` slots mpv filled before calling
    /// the entry point; `None` if any slot this wrapper uses is missing.
    ///
    /// # Safety
    ///
    /// `handle` must be the live handle mpv passed to `mpv_open_cplugin`, and
    /// the `Mpv` must be dropped before that call returns.
    #[must_use]
    pub unsafe fn new(handle: *mut MpvHandle) -> Option<Self> {
        // SAFETY: mpv writes the slots before calling the entry point and
        // never afterwards; they are copied by value, no reference is kept.
        unsafe {
            Some(Self {
                handle,
                wait_event: ffi::pfn_mpv_wait_event?,
                command: ffi::pfn_mpv_command?,
                command_async: ffi::pfn_mpv_command_async?,
                command_ret: ffi::pfn_mpv_command_ret?,
                free_node_contents: ffi::pfn_mpv_free_node_contents?,
                set_property_string: ffi::pfn_mpv_set_property_string?,
                get_property_string: ffi::pfn_mpv_get_property_string?,
                free: ffi::pfn_mpv_free?,
                observe_property: ffi::pfn_mpv_observe_property?,
                hook_add: ffi::pfn_mpv_hook_add?,
                hook_continue: ffi::pfn_mpv_hook_continue?,
                client_name: ffi::pfn_mpv_client_name?,
                error_string: ffi::pfn_mpv_error_string?,
                pending_hooks: PendingHooks::default(),
                waker: Waker::new(handle, ffi::pfn_mpv_wakeup?),
            })
        }
    }

    /// Waits up to `timeout` seconds (negative: forever) for the next event.
    pub fn wait_event(&mut self, timeout: f64) -> Option<&MpvEvent> {
        // SAFETY: the handle is live and only this thread waits on it. mpv
        // keeps the event valid until the next wait, which needs `&mut self`
        // and so cannot happen while the returned borrow lives.
        let event = unsafe { (self.wait_event)(self.handle, timeout).as_ref() }?;
        if event.event_id == MPV_EVENT_HOOK {
            // SAFETY: client.h: a HOOK event's data is an `mpv_event_hook`,
            // valid until the next wait.
            if let Some(hook) = unsafe { event.data.cast::<MpvEventHook>().as_ref() } {
                self.pending_hooks.add(hook.id);
            }
        }
        Some(event)
    }

    /// Runs a command.
    ///
    /// # Errors
    ///
    /// Names the command and why mpv rejected it, or the NUL byte that
    /// kept it from being sent.
    pub fn command(&self, args: &[&str]) -> Result<(), String> {
        let name = args.first().copied().unwrap_or_default();
        let strings = c_strings(args).map_err(|e| format!("{name}: {e}"))?;
        let mut ptrs = c_argv(&strings);
        // SAFETY: `ptrs` is a NULL-terminated array of NUL-terminated strings
        // that outlive the call.
        let code = unsafe { (self.command)(self.handle, ptrs.as_mut_ptr()) };
        self.check(code).map_err(|e| format!("{name}: {e}"))
    }

    /// Queues a command; `MPV_EVENT_COMMAND_REPLY` carries `reply_id` and its
    /// outcome.
    ///
    /// # Errors
    ///
    /// Names the command and why mpv rejected it; no reply follows.
    pub fn command_async(&self, reply_id: u64, args: &[&str]) -> Result<(), String> {
        let name = args.first().copied().unwrap_or_default();
        let strings = c_strings(args).map_err(|e| format!("{name}: {e}"))?;
        let mut ptrs = c_argv(&strings);
        // SAFETY: as in `command`; mpv copies the arguments before returning.
        let code = unsafe { (self.command_async)(self.handle, reply_id, ptrs.as_mut_ptr()) };
        self.check(code).map_err(|e| format!("{name}: {e}"))
    }

    /// Runs a command and returns its string result (`mpv_command_ret`).
    #[must_use]
    pub fn command_string_result(&self, args: &[&str]) -> Option<String> {
        let strings = c_strings(args).ok()?;
        let mut ptrs = c_argv(&strings);
        let mut node = MpvNode {
            u: MpvNodeValue { int64: 0 },
            format: MPV_FORMAT_NONE,
        };
        // SAFETY: as in `command`; `node` is a valid out-parameter.
        let code = unsafe { (self.command_ret)(self.handle, ptrs.as_mut_ptr(), &raw mut node) };
        if code < 0 {
            // mpv leaves `node` unset on error, so there is nothing to free.
            return None;
        }
        let result = if node.format == MPV_FORMAT_STRING {
            // SAFETY: format STRING makes `u.string` the active member, a
            // NUL-terminated string owned by the node.
            unsafe { from_c(node.u.string) }
        } else {
            None
        };
        // SAFETY: mpv filled `node` on success; it is freed exactly once and
        // not read afterwards.
        unsafe { (self.free_node_contents)(&raw mut node) };
        result
    }

    /// Sets a property from a string.
    ///
    /// # Errors
    ///
    /// Names the property and why mpv rejected it. Never contains `value`,
    /// which may be a proxy URL carrying its path secret.
    pub fn set_property(&self, name: &str, value: &str) -> Result<(), String> {
        let result = c_string(name).and_then(|c_name| {
            let c_value = c_string(value)?;
            // SAFETY: both are NUL-terminated strings that outlive the call.
            self.check(unsafe {
                (self.set_property_string)(self.handle, c_name.as_ptr(), c_value.as_ptr())
            })
        });
        result.map_err(|e| format!("{name}: {e}"))
    }

    /// Reads a property as a string; `None` if unavailable.
    #[must_use]
    pub fn get_property(&self, name: &str) -> Option<String> {
        let c_name = c_string(name).ok()?;
        // SAFETY: NUL-terminated name that outlives the call.
        let raw = unsafe { (self.get_property_string)(self.handle, c_name.as_ptr()) };
        // SAFETY: mpv returned NULL or a NUL-terminated string the caller owns.
        let value = unsafe { from_c(raw) };
        // SAFETY: freed exactly once with mpv's allocator (NULL is allowed)
        // and not used afterwards.
        unsafe { (self.free)(raw.cast()) };
        value
    }

    /// Observes a property; changes arrive as `MPV_EVENT_PROPERTY_CHANGE`.
    pub fn observe(&self, name: &str, format: c_int) {
        let result = c_string(name).and_then(|c_name| {
            // SAFETY: NUL-terminated name that outlives the call.
            self.check(unsafe { (self.observe_property)(self.handle, 0, c_name.as_ptr(), format) })
        });
        self.log_failure("observe", name, result);
    }

    /// Registers a hook; it arrives as `MPV_EVENT_HOOK`.
    pub fn hook_add(&self, name: &str, priority: c_int) {
        let result = c_string(name).and_then(|c_name| {
            // SAFETY: NUL-terminated name that outlives the call.
            self.check(unsafe { (self.hook_add)(self.handle, 0, c_name.as_ptr(), priority) })
        });
        self.log_failure("hook_add", name, result);
    }

    /// Continues a hook received from `wait_event`; any other id, or a second
    /// continue of the same id, is logged and ignored.
    pub fn hook_continue(&self, id: u64) {
        if !self.pending_hooks.take(id) {
            self.log(&format!("hook_continue {id}: not a pending hook"));
            return;
        }
        // SAFETY: `id` arrived in a HOOK event on this handle and was just
        // removed from the pending set, so it is continued exactly once.
        let result = self.check(unsafe { (self.hook_continue)(self.handle, id) });
        self.log_failure("hook_continue", &id.to_string(), result);
    }

    /// Prints `[mpvfin] <line>` to mpv's terminal output, literally:
    /// `mpv_command` does not expand `${...}` properties.
    pub fn log(&self, line: &str) {
        let text = format!("[mpvfin] {line}");
        if self.command(&["print-text", &text]).is_err() {
            eprintln!("{text}");
        }
    }

    /// Shows `text` literally on the OSD for `duration`.
    pub fn show_text(&self, text: &str, duration: Duration) {
        let ms = duration.as_millis().to_string();
        if let Err(e) = self.command(&["show-text", text, &ms]) {
            self.log(&e);
        }
    }

    /// A [`Waker`] for this handle, disarmed when the `Mpv` is dropped.
    #[must_use]
    pub fn waker(&self) -> Waker {
        self.waker.clone()
    }

    /// This client's name, which `script-binding` commands address it by.
    #[must_use]
    pub fn client_name(&self) -> Option<String> {
        // SAFETY: the handle is live; mpv returns a NUL-terminated string
        // that lives as long as the handle, and it is copied here.
        unsafe { from_c((self.client_name)(self.handle)) }
    }

    /// mpv's description of an error code.
    #[must_use]
    pub fn error_string(&self, code: c_int) -> String {
        // SAFETY: mpv_error_string returns a static string for any code.
        let message = unsafe { from_c((self.error_string)(code)) };
        message.unwrap_or_else(|| format!("error {code}"))
    }

    fn check(&self, code: c_int) -> Result<(), String> {
        if code >= 0 {
            return Ok(());
        }
        Err(self.error_string(code))
    }

    fn log_failure(&self, op: &str, target: &str, result: Result<(), String>) {
        if let Err(e) = result {
            self.log(&format!("{op} {target}: {e}"));
        }
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        // Wakers may outlive this wrapper, but must never reach a destroyed handle.
        self.waker.disarm();
    }
}

fn c_string(s: &str) -> Result<CString, String> {
    CString::new(s).map_err(|_| "argument contains a NUL byte".to_owned())
}

fn c_strings(args: &[&str]) -> Result<Vec<CString>, String> {
    args.iter().map(|arg| c_string(arg)).collect()
}

fn c_argv(strings: &[CString]) -> Vec<*const c_char> {
    strings
        .iter()
        .map(|s| s.as_ptr())
        .chain([ptr::null()])
        .collect()
}

/// Copies a C string; `None` for NULL or non-UTF-8 text.
///
/// # Safety
///
/// `raw` must be NULL or a valid NUL-terminated string.
pub(crate) unsafe fn from_c(raw: *const c_char) -> Option<String> {
    if raw.is_null() {
        return None;
    }
    // SAFETY: non-null and NUL-terminated per the caller's contract.
    unsafe { CStr::from_ptr(raw) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

/// Hook ids received from `wait_event` and not yet continued. mpv makes
/// continuing any other id, or one id twice, undefined behaviour.
#[derive(Default)]
struct PendingHooks(RefCell<HashSet<u64>>);

impl PendingHooks {
    fn add(&mut self, id: u64) {
        self.0.get_mut().insert(id);
    }

    /// Removes `id`; `false` if it was not pending.
    fn take(&self, id: u64) -> bool {
        self.0.borrow_mut().remove(&id)
    }
}

/// Handle pointer that may cross threads.
struct SendPtr(*mut MpvHandle);

// SAFETY: the pointer is only passed to mpv_wakeup, which client.h allows
// from any thread ("generally fully thread-safe"), and only while armed.
unsafe impl Send for SendPtr {}

/// Interrupts the plugin thread's `wait_event`; callable from any thread.
#[derive(Clone)]
pub struct Waker(Arc<Mutex<Option<(SendPtr, MpvWakeupFn)>>>);

impl Waker {
    fn new(handle: *mut MpvHandle, wakeup: MpvWakeupFn) -> Self {
        Self(Arc::new(Mutex::new(Some((SendPtr(handle), wakeup)))))
    }

    /// Wakes the plugin thread; a no-op once disarmed.
    pub fn wake(&self) {
        let slot = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((handle, wakeup)) = &*slot {
            // SAFETY: armed means the handle is live; the lock is held during
            // the call, so `disarm` cannot return while a wake is in flight.
            unsafe { wakeup(handle.0) };
        }
    }

    /// Makes every clone inert; call before the handle is destroyed.
    pub fn disarm(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static WAKES: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn count_wake(_: *mut MpvHandle) {
        WAKES.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn hooks_continue_once_and_unknown_ids_are_rejected() {
        let mut pending = PendingHooks::default();
        pending.add(7);
        assert!(pending.take(7));
        assert!(!pending.take(7), "second continue of the same id");
        assert!(!pending.take(8), "id that never arrived");
    }

    #[test]
    fn waker_is_inert_after_disarm() {
        let waker = Waker::new(ptr::null_mut(), count_wake);
        let clone = waker.clone();
        clone.wake();
        assert_eq!(WAKES.load(Ordering::SeqCst), 1);
        waker.disarm();
        clone.wake();
        assert_eq!(WAKES.load(Ordering::SeqCst), 1);
    }
}
