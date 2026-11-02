use std::cell::Cell;

use anyhow::Result;
use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, CGKeyCode};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

use crate::ime;
use crate::keymap::scancode_to_keycode;

/// Scancodes the input layer treats specially (Set 1, as sent by RDP clients).
const SC_LSHIFT: u8 = 0x2A;
const SC_RSHIFT: u8 = 0x36;
const SC_CTRL: u8 = 0x1D;
const SC_ALT: u8 = 0x38;
const SC_CAPSLOCK: u8 = 0x3A;
const SC_SPACE: u8 = 0x39;
const SC_LWIN: u8 = 0x5B;
const SC_RWIN: u8 = 0x5C;

fn modifier_flag(scancode: u8, extended: bool) -> Option<CGEventFlags> {
    match scancode {
        SC_LSHIFT | SC_RSHIFT if !extended => Some(CGEventFlags::CGEventFlagShift),
        SC_CTRL => Some(CGEventFlags::CGEventFlagControl),
        SC_ALT => Some(CGEventFlags::CGEventFlagAlternate),
        SC_LWIN | SC_RWIN if extended => Some(CGEventFlags::CGEventFlagCommand),
        _ => None,
    }
}

pub struct KeyboardInjector {
    /// Modifiers currently held. Posted events carry them explicitly: macOS derives flags from
    /// the event itself, and events synthesized through the HID system state do not inherit
    /// synthetic modifier presses.
    held: Cell<CGEventFlags>,
    /// Release events for keys we handled ourselves must not leak to the system.
    caps_swallowing: Cell<bool>,
    space_swallowing: Cell<bool>,
}

impl KeyboardInjector {
    pub fn new() -> Result<Self> {
        // Verify we can create an event source (permission check)
        let _ = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow::anyhow!("Failed to create CGEventSource — check Accessibility permission"))?;
        Ok(Self {
            held: Cell::new(CGEventFlags::CGEventFlagNull),
            caps_swallowing: Cell::new(false),
            space_swallowing: Cell::new(false),
        })
    }

    fn source() -> Result<CGEventSource> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow::anyhow!("Failed to create CGEventSource"))
    }

    /// Inject a key press or release event
    pub fn inject_key(&self, scancode: u8, extended: bool, pressed: bool) -> Result<()> {
        // Caps Lock and Ctrl+Space mean "switch input source" on macOS. The system only acts
        // on the real HID keys, so injected ones are mapped here instead.
        //
        // Caps Lock is left alone (and forwarded as caps lock) on systems without an input
        // method, where it has no other meaning.
        if !extended && scancode == SC_CAPSLOCK {
            if ime::has_input_method() {
                if pressed {
                    if !self.caps_swallowing.get() {
                        ime::toggle_input_source();
                        self.caps_swallowing.set(true);
                    }
                } else if self.caps_swallowing.replace(false) {
                    // Swallowed release: the press never reached the system either.
                }
                return Ok(());
            }
        }

        if !extended
            && scancode == SC_SPACE
            && self.held.get().contains(CGEventFlags::CGEventFlagControl)
            && ime::has_input_method()
        {
            if pressed {
                if !self.space_swallowing.get() {
                    ime::toggle_input_source();
                    self.space_swallowing.set(true);
                }
            } else if self.space_swallowing.replace(false) {
                // Swallowed release.
            }
            return Ok(());
        }

        let keycode = match scancode_to_keycode(scancode, extended) {
            Some(kc) => kc,
            None => {
                tracing::warn!(scancode, extended, "Unknown scancode, ignoring");
                return Ok(());
            }
        };

        let source = Self::source()?;
        let event = CGEvent::new_keyboard_event(source, keycode as CGKeyCode, pressed)
            .map_err(|_| anyhow::anyhow!("Failed to create keyboard event"))?;

        // A modifier's own key-down carries its flag; its release does not.
        let held = self.held.get();
        let flags = match modifier_flag(scancode, extended) {
            Some(flag) if pressed => {
                let flags = held | flag;
                self.held.set(flags);
                flags
            }
            Some(flag) => {
                let mut flags = held;
                flags.remove(flag);
                self.held.set(flags);
                flags
            }
            None => held,
        };
        event.set_flags(flags);

        event.post(CGEventTapLocation::HID);
        tracing::trace!(scancode, keycode, pressed, "Keyboard event injected");
        Ok(())
    }

    /// Inject a unicode character press/release
    pub fn inject_unicode(&self, ch: u16, pressed: bool) -> Result<()> {
        let source = Self::source()?;
        let event = CGEvent::new_keyboard_event(source, 0, pressed)
            .map_err(|_| anyhow::anyhow!("Failed to create unicode event"))?;

        if pressed {
            event.set_string_from_utf16_unchecked(&[ch]);
        }
        event.set_flags(self.held.get());

        event.post(CGEventTapLocation::HID);
        Ok(())
    }
}
