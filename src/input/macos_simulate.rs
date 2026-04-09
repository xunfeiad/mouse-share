use crate::protocol::MouseButton;
use anyhow::Result;
use core_graphics::event::{CGEvent, CGEventType, CGMouseButton, EventField};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;

use super::simulate::InputSimulator;

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
        self.post_mouse_event(
            CGEventType::MouseMoved,
            self.current_point(),
            CGMouseButton::Left,
        )
    }

    fn move_relative(&mut self, dx: f64, dy: f64) -> Result<()> {
        self.current_x += dx;
        self.current_y += dy;
        // Clamp to reasonable screen bounds (max 16K resolution)
        self.current_x = self.current_x.clamp(0.0, 16384.0);
        self.current_y = self.current_y.clamp(0.0, 16384.0);
        self.post_mouse_event(
            CGEventType::MouseMoved,
            self.current_point(),
            CGMouseButton::Left,
        )
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
}
