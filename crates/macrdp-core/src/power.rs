//! Power and lock-state helpers for remote sessions.
//!
//! Unlike Windows RDP, macOS gives third-party apps no way to type into the lock screen or
//! login window: synthetic CGEvents are ignored there and the loginwindow session is isolated
//! from user apps. So a remote session can only start on an awakened, unlocked desktop. What
//! this module does:
//!
//! - keep the *system* awake while the server runs, so the machine stays reachable (the
//!   display is still allowed to sleep, as a Windows RDP host's screen would);
//! - wake the display when a client connects and hold an assertion while that session lives,
//!   which is when ScreenCaptureKit can actually capture;
//! - report whether the screen is locked so the session can explain what the client sees.

use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};
use core_foundation_sys::base::CFRelease;
use std::ffi::c_void;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: CFStringRef,
        assertion_level: u32,
        assertion_name: CFStringRef,
        assertion_id: *mut u32,
    ) -> i32;
    fn IOPMAssertionRelease(assertion_id: u32) -> i32;
    fn IOPMAssertionDeclareUserActivity(
        assertion_name: CFStringRef,
        user_type: u32,
        assertion_id: *mut u32,
    ) -> i32;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn CGSessionCopyCurrentDictionary() -> core_foundation_sys::dictionary::CFDictionaryRef;
}

const K_IOPM_ASSERTION_LEVEL_ON: u32 = 255;
/// `kIOPMUserActiveLocal`: user activity that wakes the display (unlike `Remote`, which only
/// resets idle timers).
const K_IOPM_USER_ACTIVE_LOCAL: u32 = 0;

/// Keeps a power assertion alive until dropped.
struct Assertion(u32);

impl Assertion {
    fn create(kind: &str, name: &str) -> Option<Self> {
        let kind = CFString::new(kind);
        let name = CFString::new(name);
        let mut id = 0u32;
        let status = unsafe {
            IOPMAssertionCreateWithName(
                kind.as_concrete_TypeRef(),
                K_IOPM_ASSERTION_LEVEL_ON,
                name.as_concrete_TypeRef(),
                &mut id,
            )
        };
        if status != 0 {
            tracing::warn!(status, assertion_kind = kind.to_string(), "Failed to create power assertion");
            return None;
        }
        Some(Self(id))
    }
}

impl Drop for Assertion {
    fn drop(&mut self) {
        unsafe { IOPMAssertionRelease(self.0) };
    }
}

/// Held for as long as the server runs: the system does not idle-sleep, so the machine stays
/// reachable. The display may still sleep (and with it, on default settings, the screen locks).
pub struct RemoteSessionPower {
    _system: Option<Assertion>,
}

impl RemoteSessionPower {
    pub fn hold() -> Self {
        Self {
            _system: Assertion::create("PreventUserIdleSystemSleep", "macrdp remote access"),
        }
    }
}

/// Held while a client is connected: keeps the display on so the session stays capturable
/// instead of blacking out mid-stream.
pub struct DisplaySleepGuard {
    _display: Option<Assertion>,
}

impl DisplaySleepGuard {
    pub fn hold() -> Self {
        Self {
            _display: Assertion::create("PreventUserIdleDisplaySleep", "macrdp remote session"),
        }
    }
}

/// Wake a sleeping display. A client can connect at any time, and ScreenCaptureKit only lists
/// awakened displays, so the session has to wake the panel before it can capture anything.
pub fn wake_display() -> bool {
    let name = CFString::new("macrdp wake display for remote session");
    let mut id = 0u32;
    let status =
        unsafe { IOPMAssertionDeclareUserActivity(name.as_concrete_TypeRef(), K_IOPM_USER_ACTIVE_LOCAL, &mut id) };
    if status != 0 {
        tracing::warn!(status, "Failed to declare user activity (wake display)");
        return false;
    }
    // The activity is recorded; the assertion itself is not needed afterwards.
    unsafe { IOPMAssertionRelease(id) };
    tracing::info!("Woke the display for an incoming remote session");
    true
}

/// True while the screen is locked (`CGSSessionScreenIsLocked`).
pub fn screen_is_locked() -> bool {
    use core_foundation_sys::number::{CFBooleanGetValue, CFBooleanRef};
    unsafe {
        let session = CGSessionCopyCurrentDictionary();
        if session.is_null() {
            return false;
        }
        let key = CFString::new("CGSSessionScreenIsLocked");
        let value = core_foundation_sys::dictionary::CFDictionaryGetValue(
            session,
            key.as_concrete_TypeRef() as *const c_void,
        );
        let locked = !value.is_null() && CFBooleanGetValue(value as CFBooleanRef);
        CFRelease(session as _);
        locked
    }
}
