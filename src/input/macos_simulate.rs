use crate::protocol::MouseButton;
use anyhow::Result;
use core_graphics::display::CGDisplay;
use core_graphics::event::{CGEvent, CGEventFlags, CGEventType, CGMouseButton, EventField};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;

use super::simulate::InputSimulator;

pub struct MacOsSimulator {
    held: std::collections::BTreeSet<MouseButton>,
    current_x: f64,
    current_y: f64,
    /// Cached HID event source. Creating a new `CGEventSource` is expensive
    /// (hundreds of microseconds of allocation/setup); cloning reuses the
    /// existing one through `CFRetain`, which is a single atomic increment.
    /// `CGEvent::new_*` takes the source by value, so we clone per call.
    source: CGEventSource,
}

// `CGEventSource` wraps a `NonNull<CGEventSource>` which is not `Send` by
// default. Core Foundation objects like `CGEventSourceRef` are documented
// as thread-safe (retain/release use atomic ops), and in practice the
// simulator is owned by exactly one thread — the client event loop — at a
// time. Declaring this Send lets the simulator be boxed behind the
// `Box<dyn InputSimulator>` trait object which requires Send.
unsafe impl Send for MacOsSimulator {}

impl MacOsSimulator {
    pub fn new() -> Result<Self> {
        extern "C" {
            fn CGPreflightPostEventAccess() -> bool;
        }
        anyhow::ensure!(
            unsafe { CGPreflightPostEventAccess() },
            "Enable Accessibility access for mouse share to receive input"
        );
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow::anyhow!("Cannot create an input event source"))?;
        // Couple the hardware cursor to mouse events once. This is a mode
        // toggle, not a per-event operation — the old code called it on
        // every warp, which was a major source of stutter at high event rates.
        if let Err(e) = CGDisplay::associate_mouse_and_mouse_cursor_position(true) {
            log::warn!("CGAssociateMouseAndMouseCursorPosition failed: {:?}", e);
        }
        Ok(Self {
            held: Default::default(),
            current_x: 0.0,
            current_y: 0.0,
            source,
        })
    }

    fn post_mouse_event(
        &self,
        event_type: CGEventType,
        point: CGPoint,
        button: MouseButton,
    ) -> Result<()> {
        let event = CGEvent::new_mouse_event(
            self.source.clone(),
            event_type,
            point,
            Self::map_button(&button),
        )
        .map_err(|_| anyhow::anyhow!("failed to create mouse event"))?;
        if let MouseButton::Other(button @ (1 | 2)) = button {
            event.set_integer_value_field(
                EventField::MOUSE_EVENT_BUTTON_NUMBER,
                i64::from(button) + 2,
            );
        }
        event.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, 0x4d53_5632);
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
        CGDisplay::warp_mouse_cursor_position(point)
            .map_err(|e| anyhow::anyhow!("Cannot move cursor: {e:?}"))?;
        let button = self.held.first().copied().unwrap_or(MouseButton::Left);
        let kind = match self.held.first() {
            None => CGEventType::MouseMoved,
            Some(MouseButton::Left) => CGEventType::LeftMouseDragged,
            Some(MouseButton::Right) => CGEventType::RightMouseDragged,
            Some(_) => CGEventType::OtherMouseDragged,
        };
        self.post_mouse_event(kind, point, button)
    }

    fn move_relative(&mut self, dx: f64, dy: f64) -> Result<()> {
        let (x, y) =
            crate::screen::get_screen_info()?.clamp((self.current_x + dx, self.current_y + dy));
        self.move_to(x, y)
    }

    fn button_down(&mut self, button: MouseButton) -> Result<()> {
        let event_type = match button {
            MouseButton::Left => CGEventType::LeftMouseDown,
            MouseButton::Right => CGEventType::RightMouseDown,
            _ => CGEventType::OtherMouseDown,
        };
        self.post_mouse_event(event_type, self.current_point(), button)?;
        self.held.insert(button);
        Ok(())
    }

    fn button_up(&mut self, button: MouseButton) -> Result<()> {
        let event_type = match button {
            MouseButton::Left => CGEventType::LeftMouseUp,
            MouseButton::Right => CGEventType::RightMouseUp,
            _ => CGEventType::OtherMouseUp,
        };
        self.post_mouse_event(event_type, self.current_point(), button)?;
        self.held.remove(&button);
        Ok(())
    }

    fn scroll(&mut self, dx: f64, dy: f64) -> Result<()> {
        // Create a generic event and set scroll fields manually
        let event = CGEvent::new(self.source.clone())
            .map_err(|_| anyhow::anyhow!("failed to create event"))?;
        event.set_type(CGEventType::ScrollWheel);
        event.set_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2, dx as i64);
        event.set_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1, dy as i64);
        event.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, 0x4d53_5632);
        event.post(core_graphics::event::CGEventTapLocation::HID);
        Ok(())
    }

    fn key_event(&mut self, keycode: u32, down: bool, flags: u64) -> Result<()> {
        let event = CGEvent::new_keyboard_event(self.source.clone(), keycode as u16, down)
            .map_err(|_| anyhow::anyhow!("failed to create keyboard event"))?;
        // Preserve modifier state from the server so shift+letter, cmd+c,
        // ctrl+space etc. produce the right character / shortcut locally.
        event.set_flags(CGEventFlags::from_bits_truncate(flags));
        event.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, 0x4d53_5632);
        if matches!(keycode, 54..=63) {
            event.set_type(CGEventType::FlagsChanged);
        }
        event.post(core_graphics::event::CGEventTapLocation::HID);
        Ok(())
    }
}
