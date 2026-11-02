//! Power assertions held while the RDP server runs.
//!
//! A sleeping display is unusable for remote sessions: ScreenCaptureKit only lists awakened
//! displays, and this Mac locks as soon as the display turns off (password delay "immediate"),
//! so a client that connects while the display is asleep sees nothing and may be dropped.
//! Holding the idle assertions keeps the machine reachable and the desktop capturable.

use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: CFStringRef,
        assertion_level: u32,
        assertion_name: CFStringRef,
        assertion_id: *mut u32,
    ) -> i32;
    fn IOPMAssertionRelease(assertion_id: u32) -> i32;
}

const K_IOPM_ASSERTION_LEVEL_ON: u32 = 255;

/// Keeps an assertion alive until dropped.
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

/// Held for as long as the server runs: the display stays on (so the screen lock never kicks
/// in) and the system does not idle-sleep, which would make the machine unreachable.
pub struct RemoteSessionPower {
    _display: Option<Assertion>,
    _system: Option<Assertion>,
}

impl RemoteSessionPower {
    pub fn hold() -> Self {
        let display = Assertion::create("PreventUserIdleDisplaySleep", "macrdp remote access");
        let system = Assertion::create("PreventUserIdleSystemSleep", "macrdp remote access");
        if display.is_some() {
            tracing::info!("Holding display-sleep assertion while the server runs");
        }
        Self { _display: display, _system: system }
    }
}
