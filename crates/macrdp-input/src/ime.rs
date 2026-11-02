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
use std::sync::Mutex;

type TISInputSourceRef = *mut c_void;

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

/// True when at least one non-ASCII (IME) input source is enabled. Callers use this to leave
/// Caps Lock alone on systems that have no input method to switch to.
pub fn has_input_method() -> bool {
    SourceList::enabled_keyboard_sources()
        .is_some_and(|list| list.sources.iter().any(|s| !s.ascii))
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
/// in the system UI. Returns true when the input source was changed.
pub fn toggle_input_source() -> bool {
    let Some(list) = SourceList::enabled_keyboard_sources() else {
        return false;
    };
    let Some(current) = list.current() else {
        return false;
    };

    let target = if current.ascii {
        pick(&list, &LAST_NON_ASCII, false)
    } else {
        pick(&list, &LAST_ASCII, true)
    };
    let Some(target) = target else {
        return false;
    };

    // Remember where we came from so the next toggle returns here.
    let remember = if current.ascii { &LAST_ASCII } else { &LAST_NON_ASCII };
    if let Ok(mut slot) = remember.lock() {
        *slot = Some(current.id.clone());
    }

    let status = unsafe { TISSelectInputSource(target.reference) };
    if status != 0 {
        tracing::warn!(status, source = %target.id, "TISSelectInputSource failed");
        return false;
    }
    tracing::debug!(source = %target.id, "Input source switched");
    true
}
