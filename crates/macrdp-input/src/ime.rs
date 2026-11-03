//! Switch the macOS input source (keyboard input method) from injected input.
//!
//! macOS handles its own "use Caps Lock / Ctrl+Space to switch input source" shortcuts at a
//! layer that synthetic CGEvents do not reach, so a remote client's Caps Lock or Ctrl+Space
//! would otherwise do nothing but toggle caps lock. Selecting the source through HIToolbox
//! (Text Input Sources, the same API the Input menu uses) makes remote switching reliable.

use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use core_foundation_sys::base::{CFRelease, CFTypeRef};
use core_foundation_sys::string::CFStringRef;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

type TISInputSourceRef = *mut c_void;
type DispatchQueue = *mut c_void;

// HIToolbox's TIS functions call `dispatch_assert_queue(dispatch_get_main_queue())` and trap
// (EXC_BREAKPOINT) when invoked from another thread — the RDP loop runs on its own thread, so
// everything below is marshalled onto the main queue.
#[link(name = "System")]
extern "C" {
    /// The main queue. `dispatch_get_main_queue()` is an inline wrapper in the SDK; the
    /// exported symbol is this global (DISPATCH_EXPORT in dispatch/queue.h).
    static _dispatch_main_q: u8;
    fn dispatch_async_f(
        queue: DispatchQueue,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
    fn pthread_main_np() -> i32;
}

type MainBlock = Box<dyn FnOnce() + Send + 'static>;

/// Run `f` on the main thread: directly when already there, otherwise asynchronously on the
/// main queue (never `dispatch_sync`, which could deadlock with the UI thread).
fn on_main(f: impl FnOnce() + Send + 'static) {
    if unsafe { pthread_main_np() } != 0 {
        f();
        return;
    }
    let block: Box<MainBlock> = Box::new(Box::new(f));
    let context = Box::into_raw(block) as *mut c_void;
    let main_queue = std::ptr::addr_of!(_dispatch_main_q) as DispatchQueue;
    unsafe { dispatch_async_f(main_queue, context, run_on_main) };
}

extern "C" fn run_on_main(context: *mut c_void) {
    let block = unsafe { Box::from_raw(context as *mut MainBlock) };
    // Never let a panic cross the C frame.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| block()));
}

/// Whether an input method is enabled, cached by [`prime`] / [`toggle_input_source`] so the
/// keyboard path can consult it without touching HIToolbox from the RDP thread.
static HAS_INPUT_METHOD: AtomicBool = AtomicBool::new(false);

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    static kTISPropertyInputSourceCategory: CFTypeRef;
    static kTISPropertyInputSourceID: CFTypeRef;
    static kTISPropertyInputSourceIsASCIICapable: CFTypeRef;
    static kTISPropertyInputSourceIsEnabled: CFTypeRef;
    static kTISPropertyInputSourceIsSelectCapable: CFTypeRef;
    static kTISCategoryKeyboardInputSource: CFTypeRef;

    /// Returns a +1 array of input sources; pass NULL properties for "all" (still filtered by
    /// `include_all_installed`).
    fn TISCreateInputSourceList(properties: CFTypeRef, include_all_installed: bool) -> CFTypeRef;
    fn TISCopyCurrentKeyboardInputSource() -> TISInputSourceRef;
    fn TISGetInputSourceProperty(source: TISInputSourceRef, property_key: CFTypeRef) -> CFTypeRef;
    fn TISSelectInputSource(source: TISInputSourceRef) -> i32;
}

/// Last non-ASCII source we left, so switching back returns to the user's IME (not merely the
/// first one installed) — mirrors how the system remembers the previous input source.
static LAST_NON_ASCII: Mutex<Option<String>> = Mutex::new(None);
static LAST_ASCII: Mutex<Option<String>> = Mutex::new(None);

struct Source {
    id: String,
    ascii: bool,
    reference: TISInputSourceRef,
}

/// Owns the TISCreateInputSourceList array so its sources stay alive while we use them.
struct SourceList {
    array: CFTypeRef,
    sources: Vec<Source>,
}

impl SourceList {
    fn enabled_keyboard_sources() -> Option<Self> {
        let array = unsafe { TISCreateInputSourceList(std::ptr::null(), false) };
        if array.is_null() {
            return None;
        }
        let count = unsafe { core_foundation_sys::array::CFArrayGetCount(array as _) };
        let mut sources = Vec::with_capacity(count.max(0) as usize);
        for index in 0..count {
            let reference =
                unsafe { core_foundation_sys::array::CFArrayGetValueAtIndex(array as _, index) }
                    as TISInputSourceRef;
            if reference.is_null() {
                continue;
            }
            if !prop_bool(reference, unsafe { kTISPropertyInputSourceIsEnabled })
                || !prop_bool(reference, unsafe { kTISPropertyInputSourceIsSelectCapable })
            {
                continue;
            }
            if !same_property(reference, unsafe { kTISPropertyInputSourceCategory }, unsafe {
                kTISCategoryKeyboardInputSource
            }) {
                continue;
            }
            let Some(id) = prop_string(reference, unsafe { kTISPropertyInputSourceID }) else {
                continue;
            };
            sources.push(Source {
                id,
                ascii: prop_bool(reference, unsafe { kTISPropertyInputSourceIsASCIICapable }),
                reference,
            });
        }
        Some(Self { array, sources })
    }

    fn current(&self) -> Option<&Source> {
        let current = unsafe { TISCopyCurrentKeyboardInputSource() };
        if current.is_null() {
            return None;
        }
        let id = prop_string(current, unsafe { kTISPropertyInputSourceID });
        unsafe { CFRelease(current as _) };
        let id = id?;
        self.sources.iter().find(|s| s.id == id)
    }
}

impl Drop for SourceList {
    fn drop(&mut self) {
        unsafe { CFRelease(self.array as _) };
    }
}

fn prop_string(source: TISInputSourceRef, key: CFTypeRef) -> Option<String> {
    let value = unsafe { TISGetInputSourceProperty(source, key) };
    if value.is_null() {
        return None;
    }
    let string = unsafe { CFString::wrap_under_get_rule(value as CFStringRef) };
    Some(string.to_string())
}

fn prop_bool(source: TISInputSourceRef, key: CFTypeRef) -> bool {
    let value = unsafe { TISGetInputSourceProperty(source, key) };
    !value.is_null() && unsafe { core_foundation_sys::number::CFBooleanGetValue(value as _) }
}

fn same_property(source: TISInputSourceRef, key: CFTypeRef, expected: CFTypeRef) -> bool {
    let value = unsafe { TISGetInputSourceProperty(source, key) };
    !value.is_null() && unsafe { core_foundation_sys::base::CFEqual(value, expected) } != 0
}

/// Detect whether at least one non-ASCII (IME) input source is enabled, and cache the result.
///
/// Call once at startup (from the main thread, or any thread — the work is marshalled). The
/// keyboard injector then reads the cached flag instead of calling HIToolbox from the RDP
/// thread, which would trap on the TIS main-queue assertion.
pub fn prime() {
    on_main(|| {
        let has = SourceList::enabled_keyboard_sources()
            .is_some_and(|list| list.sources.iter().any(|s| !s.ascii));
        HAS_INPUT_METHOD.store(has, Ordering::Relaxed);
        tracing::debug!(has_input_method = has, "Cached input method availability");
    });
}

/// True when at least one non-ASCII (IME) input source is enabled. Callers use this to leave
/// Caps Lock alone on systems that have no input method to switch to.
pub fn has_input_method() -> bool {
    HAS_INPUT_METHOD.load(Ordering::Relaxed)
}

/// Preferred source of the requested kind: the one we last used, else the first enabled one.
fn pick<'a>(
    list: &'a SourceList,
    remembered: &Mutex<Option<String>>,
    want_ascii: bool,
) -> Option<&'a Source> {
    let remembered = remembered.lock().ok().and_then(|id| id.clone());
    if let Some(id) = remembered {
        if let Some(source) = list.sources.iter().find(|s| s.id == id && s.ascii == want_ascii) {
            return Some(source);
        }
    }
    list.sources.iter().find(|s| s.ascii == want_ascii)
}

/// Switch between the current IME and the ASCII keyboard, the way Caps Lock and Ctrl+Space do
/// in the system UI. Runs on the main thread (HIToolbox requirement); the caller — the RDP
/// input thread — is never blocked.
pub fn toggle_input_source() {
    on_main(|| {
        let Some(list) = SourceList::enabled_keyboard_sources() else {
            return;
        };
        let has_input_method = list.sources.iter().any(|s| !s.ascii);
        HAS_INPUT_METHOD.store(has_input_method, Ordering::Relaxed);
        if !has_input_method {
            return;
        }
        let Some(current) = list.current() else {
            return;
        };

        let target = if current.ascii {
            pick(&list, &LAST_NON_ASCII, false)
        } else {
            pick(&list, &LAST_ASCII, true)
        };
        let Some(target) = target else {
            return;
        };

        // Remember where we came from so the next toggle returns here.
        let remember = if current.ascii { &LAST_ASCII } else { &LAST_NON_ASCII };
        if let Ok(mut slot) = remember.lock() {
            *slot = Some(current.id.clone());
        }

        let status = unsafe { TISSelectInputSource(target.reference) };
        if status != 0 {
            tracing::warn!(status, source = %target.id, "TISSelectInputSource failed");
            return;
        }
        tracing::debug!(source = %target.id, "Input source switched");
    });
}
