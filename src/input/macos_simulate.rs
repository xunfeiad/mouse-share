use crate::protocol::MouseButton;
use anyhow::Result;
use core_graphics::display::CGDisplay;
use core_graphics::event::{CGEvent, CGEventFlags, CGEventType, CGMouseButton, EventField};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;

use super::simulate::InputSimulator;

/// Warp the visible cursor to a given point and re-couple it to mouse events.
/// `CGEvent::post(MouseMoved)` alone only notifies apps of a move — it does NOT
/// reliably move the visible cursor on the client Mac when there is no local
/// HID input. This function does the actual visual move.
///
/// NOTE: this function does NOT call `show_cursor`. Cursor visibility is
/// managed explicitly at the Enter/Leave boundary by the caller, using
/// `capture::hide_local_cursor` / `show_local_cursor`. Calling show_cursor on
/// every move_relative would push the refcount arbitrarily negative and make
/// later hide calls ineffective.
fn warp_cursor(point: CGPoint) {
    if let Err(e) = CGDisplay::warp_mouse_cursor_position(point) {
        log::error!(
            "CGWarpMouseCursorPosition failed: {:?} at ({:.0},{:.0}) \
             — check Accessibility permission for this binary",
            e, point.x, point.y
        );
    }
    // Re-couple cursor to future mouse events (no-op if already coupled).
    if let Err(e) = CGDisplay::associate_mouse_and_mouse_cursor_position(true) {
        log::warn!("CGAssociateMouseAndMouseCursorPosition failed: {:?}", e);
    }
}

pub struct MacOsSimulator {
    current_x: f64,
    current_y: f64,
}

impl MacOsSimulator {
    pub fn new() -> Self {
        Self {
            current_x: 0.0,
            current_y: 0.0,
        }
    }

    fn source(&self) -> Result<CGEventSource> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow::anyhow!("failed to create CGEventSource"))
    }

    fn post_mouse_event(
        &self,
        event_type: CGEventType,
        point: CGPoint,
        button: CGMouseButton,
    ) -> Result<()> {
        let source = self.source()?;
        let event = CGEvent::new_mouse_event(source, event_type, point, button)
            .map_err(|_| anyhow::anyhow!("failed to create mouse event"))?;
        event.post(core_graphics::event::CGEventTapLocation::HID);
        Ok(())
    }

    fn map_button(button: &MouseButton) -> CGMouseButton {
        match button {
            MouseButton::Left => CGMouseButton::Left,
            MouseButton::Right => CGMouseButton::Right,
            MouseButton::Middle | MouseButton::Other(_) => CGMouseButton::Center,
        }
    }

    fn current_point(&self) -> CGPoint {
        CGPoint::new(self.current_x, self.current_y)
    }
}

impl InputSimulator for MacOsSimulator {
    fn move_to(&mut self, x: f64, y: f64) -> Result<()> {
        self.current_x = x;
        self.current_y = y;
        let point = self.current_point();
        // Actually move + show the visible cursor first, then post the
        // MouseMoved event so apps observe the move.
        warp_cursor(point);
        self.post_mouse_event(CGEventType::MouseMoved, point, CGMouseButton::Left)
    }

    fn move_relative(&mut self, dx: f64, dy: f64) -> Result<()> {
        self.current_x += dx;
        self.current_y += dy;
        // Clamp to reasonable screen bounds (max 16K resolution)
        self.current_x = self.current_x.clamp(0.0, 16384.0);
        self.current_y = self.current_y.clamp(0.0, 16384.0);
        let point = self.current_point();
        warp_cursor(point);
        self.post_mouse_event(CGEventType::MouseMoved, point, CGMouseButton::Left)
    }

    fn button_down(&mut self, button: MouseButton) -> Result<()> {
        let cg_btn = Self::map_button(&button);
        let event_type = match button {
            MouseButton::Left => CGEventType::LeftMouseDown,
            MouseButton::Right => CGEventType::RightMouseDown,
            _ => CGEventType::OtherMouseDown,
        };
        self.post_mouse_event(event_type, self.current_point(), cg_btn)
    }

    fn button_up(&mut self, button: MouseButton) -> Result<()> {
        let cg_btn = Self::map_button(&button);
        let event_type = match button {
            MouseButton::Left => CGEventType::LeftMouseUp,
            MouseButton::Right => CGEventType::RightMouseUp,
            _ => CGEventType::OtherMouseUp,
        };
        self.post_mouse_event(event_type, self.current_point(), cg_btn)
    }

    fn scroll(&mut self, _dx: f64, dy: f64) -> Result<()> {
        let source = self.source()?;
        // Create a generic event and set scroll fields manually
        let event = CGEvent::new(source)
            .map_err(|_| anyhow::anyhow!("failed to create event"))?;
        event.set_type(CGEventType::ScrollWheel);
        event.set_integer_value_field(
            EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1,
            dy as i64,
        );
        event.post(core_graphics::event::CGEventTapLocation::HID);
        Ok(())
    }

    fn key_event(&mut self, keycode: u32, down: bool, flags: u64) -> Result<()> {
        let source = self.source()?;
        let event = CGEvent::new_keyboard_event(source, keycode as u16, down)
            .map_err(|_| anyhow::anyhow!("failed to create keyboard event"))?;
        // Preserve modifier state from the server so shift+letter, cmd+c,
        // ctrl+space etc. produce the right character / shortcut locally.
        event.set_flags(CGEventFlags::from_bits_truncate(flags));
        event.post(core_graphics::event::CGEventTapLocation::HID);
        Ok(())
    }
}
