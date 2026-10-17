use anyhow::Result;
use core_graphics::event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, EventField};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use foreign_types::ForeignType;
use std::time::{Duration, Instant};

extern "C" {
    fn CGEventCreateScrollWheelEvent2(
        source: *mut core_graphics::sys::CGEventSource,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> *mut core_graphics::sys::CGEvent;
}

/// ScrollEventUnit::Line
const SCROLL_UNIT_LINE: u32 = 1;

/// RDP sends ±120 wheel units per physical notch (MS-RDPBCGR 2.2.8.1.1.3.1.1.3).
pub const WHEEL_UNITS_PER_NOTCH: i32 = 120;
/// Lines scrolled per notch; matches the Windows default.
pub const LINES_PER_NOTCH: i32 = 3;

/// Max interval between clicks to count as a multi-click.
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);
/// Max cursor movement (px, per axis) between clicks to count as a multi-click.
const MULTI_CLICK_SLOP: u16 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    fn bit(self) -> u8 {
        1 << (self as u8)
    }

    fn cg(self) -> CGMouseButton {
        match self {
            MouseButton::Left => CGMouseButton::Left,
            MouseButton::Right => CGMouseButton::Right,
            MouseButton::Middle => CGMouseButton::Center,
        }
    }

    fn dragged_type(self) -> CGEventType {
        match self {
            MouseButton::Left => CGEventType::LeftMouseDragged,
            MouseButton::Right => CGEventType::RightMouseDragged,
            MouseButton::Middle => CGEventType::OtherMouseDragged,
        }
    }

    fn down_up_type(self, pressed: bool) -> CGEventType {
        match (self, pressed) {
            (MouseButton::Left, true) => CGEventType::LeftMouseDown,
            (MouseButton::Left, false) => CGEventType::LeftMouseUp,
            (MouseButton::Right, true) => CGEventType::RightMouseDown,
            (MouseButton::Right, false) => CGEventType::RightMouseUp,
            (MouseButton::Middle, true) => CGEventType::OtherMouseDown,
            (MouseButton::Middle, false) => CGEventType::OtherMouseUp,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct LastClick {
    button: MouseButton,
    at: Instant,
    x: u16,
    y: u16,
    count: i64,
}

/// Per-connection mouse state needed to synthesize correct CGEvents.
#[derive(Debug, Default)]
struct MouseState {
    /// Bitset of currently held buttons (see `MouseButton::bit`).
    pressed: u8,
    last_click: Option<LastClick>,
    /// Sub-notch wheel remainders, in RDP units.
    wheel_acc_v: i32,
    wheel_acc_h: i32,
}

/// Compute the `kCGMouseEventClickState` for a new press.
fn next_click_count(prev: Option<&LastClick>, button: MouseButton, now: Instant, x: u16, y: u16) -> i64 {
    match prev {
        Some(p)
            if p.button == button
                && now.duration_since(p.at) <= MULTI_CLICK_INTERVAL
                && p.x.abs_diff(x) <= MULTI_CLICK_SLOP
                && p.y.abs_diff(y) <= MULTI_CLICK_SLOP =>
        {
            p.count + 1
        }
        _ => 1,
    }
}

/// Convert RDP wheel units into whole scroll lines, carrying the sub-notch
/// remainder in `acc` so high-resolution wheels still add up correctly.
fn wheel_units_to_lines(acc: &mut i32, units: i32) -> i32 {
    *acc += units;
    let notches = *acc / WHEEL_UNITS_PER_NOTCH;
    *acc -= notches * WHEEL_UNITS_PER_NOTCH;
    notches * LINES_PER_NOTCH
}

pub struct MouseInjector {
    state: MouseState,
}

impl MouseInjector {
    pub fn new() -> Result<Self> {
        let _ = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow::anyhow!("Failed to create CGEventSource — check Accessibility permission"))?;
        Ok(Self { state: MouseState::default() })
    }

    fn source() -> Result<CGEventSource> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow::anyhow!("Failed to create CGEventSource"))
    }

    /// Highest-priority held button, if any (Left > Right > Middle).
    fn held_button(&self) -> Option<MouseButton> {
        [MouseButton::Left, MouseButton::Right, MouseButton::Middle]
            .into_iter()
            .find(|b| self.state.pressed & b.bit() != 0)
    }

    pub fn move_to(&mut self, x: u16, y: u16) -> Result<()> {
        let point = CGPoint::new(x as f64, y as f64);
        let source = Self::source()?;
        let (event_type, cg_button) = match self.held_button() {
            Some(b) => (b.dragged_type(), b.cg()),
            None => (CGEventType::MouseMoved, CGMouseButton::Left),
        };
        let event = CGEvent::new_mouse_event(source, event_type, point, cg_button)
            .map_err(|_| anyhow::anyhow!("Failed to create mouse move event"))?;

        if let Some(b) = self.held_button() {
            event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, b.cg() as i64);
            if let Some(lc) = self.state.last_click.filter(|lc| lc.button == b) {
                event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, lc.count);
            }
        }

        event.post(CGEventTapLocation::HID);
        tracing::trace!(x, y, ?event_type, "Mouse moved");
        Ok(())
    }

    pub fn button_event(&mut self, button: MouseButton, pressed: bool, x: u16, y: u16) -> Result<()> {
        let point = CGPoint::new(x as f64, y as f64);

        let click_count = if pressed {
            let now = Instant::now();
            let count = next_click_count(self.state.last_click.as_ref(), button, now, x, y);
            self.state.last_click = Some(LastClick { button, at: now, x, y, count });
            self.state.pressed |= button.bit();
            count
        } else {
            self.state.pressed &= !button.bit();
            self.state
                .last_click
                .filter(|lc| lc.button == button)
                .map_or(1, |lc| lc.count)
        };

        let source = Self::source()?;
        let event = CGEvent::new_mouse_event(source, button.down_up_type(pressed), point, button.cg())
            .map_err(|_| anyhow::anyhow!("Failed to create mouse button event"))?;
        event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, click_count);
        event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, button.cg() as i64);

        event.post(CGEventTapLocation::HID);
        tracing::trace!(?button, pressed, x, y, click_count, "Mouse button event");
        Ok(())
    }

    /// Scroll by RDP wheel units (±120 per notch). Positive vertical = away from user (scroll up).
    pub fn scroll(&mut self, vertical_units: i16, horizontal_units: i16) -> Result<()> {
        let lines_v = wheel_units_to_lines(&mut self.state.wheel_acc_v, vertical_units as i32);
        let lines_h = wheel_units_to_lines(&mut self.state.wheel_acc_h, horizontal_units as i32);
        if lines_v == 0 && lines_h == 0 {
            return Ok(());
        }
        unsafe {
            let event_ref = CGEventCreateScrollWheelEvent2(
                std::ptr::null_mut(),
                SCROLL_UNIT_LINE,
                2,
                lines_v,
                lines_h,
                0,
            );
            if event_ref.is_null() {
                return Err(anyhow::anyhow!("Failed to create scroll event"));
            }
            let event = CGEvent::from_ptr(event_ref);
            event.post(CGEventTapLocation::HID);
        }
        tracing::trace!(vertical_units, horizontal_units, lines_v, lines_h, "Mouse scroll");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn click(button: MouseButton, at: Instant, x: u16, y: u16, count: i64) -> LastClick {
        LastClick { button, at, x, y, count }
    }

    #[test]
    fn click_count_single_then_double_then_triple() {
        let t0 = Instant::now();
        assert_eq!(next_click_count(None, MouseButton::Left, t0, 10, 10), 1);
        let first = click(MouseButton::Left, t0, 10, 10, 1);
        let t1 = t0 + Duration::from_millis(200);
        assert_eq!(next_click_count(Some(&first), MouseButton::Left, t1, 12, 9), 2);
        let second = click(MouseButton::Left, t1, 12, 9, 2);
        let t2 = t1 + Duration::from_millis(300);
        assert_eq!(next_click_count(Some(&second), MouseButton::Left, t2, 12, 9), 3);
    }

    #[test]
    fn click_count_resets_on_timeout_distance_or_other_button() {
        let t0 = Instant::now();
        let first = click(MouseButton::Left, t0, 10, 10, 1);
        let late = t0 + Duration::from_millis(501);
        assert_eq!(next_click_count(Some(&first), MouseButton::Left, late, 10, 10), 1);
        let t1 = t0 + Duration::from_millis(100);
        assert_eq!(next_click_count(Some(&first), MouseButton::Left, t1, 10, 15), 1);
        assert_eq!(next_click_count(Some(&first), MouseButton::Right, t1, 10, 10), 1);
    }

    #[test]
    fn wheel_one_notch_is_three_lines_and_keeps_sign() {
        let mut acc = 0;
        assert_eq!(wheel_units_to_lines(&mut acc, 120), LINES_PER_NOTCH);
        assert_eq!(wheel_units_to_lines(&mut acc, -120), -LINES_PER_NOTCH);
        assert_eq!(wheel_units_to_lines(&mut acc, 240), 2 * LINES_PER_NOTCH);
        assert_eq!(acc, 0);
    }

    #[test]
    fn wheel_sub_notch_deltas_accumulate() {
        let mut acc = 0;
        assert_eq!(wheel_units_to_lines(&mut acc, 40), 0);
        assert_eq!(wheel_units_to_lines(&mut acc, 40), 0);
        assert_eq!(wheel_units_to_lines(&mut acc, 40), LINES_PER_NOTCH);
        assert_eq!(acc, 0);
        assert_eq!(wheel_units_to_lines(&mut acc, -130), -LINES_PER_NOTCH);
        assert_eq!(acc, -10);
        assert_eq!(wheel_units_to_lines(&mut acc, 10), 0);
        assert_eq!(acc, 0);
    }

    #[test]
    fn button_bits_are_distinct() {
        let bits = [MouseButton::Left.bit(), MouseButton::Right.bit(), MouseButton::Middle.bit()];
        assert_eq!(bits, [1, 2, 4]);
    }
}
