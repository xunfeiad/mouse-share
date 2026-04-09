use crate::protocol::{MouseButton, MouseEvent, MouseEventType};
use anyhow::Result;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType, EventField,
};
use crossbeam_channel::Sender;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::capture::InputCapture;

pub struct MacOsCapture {
    suppressing: Arc<AtomicBool>,
}

impl MacOsCapture {
    pub fn new() -> Self {
        Self {
            suppressing: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl InputCapture for MacOsCapture {
    fn run(&mut self, sender: Sender<MouseEvent>) -> Result<()> {
        let suppressing = self.suppressing.clone();

        // Initialize with current cursor position to avoid initial delta spike
        let (init_x, init_y) = super::capture::get_cursor_position().unwrap_or((0.0, 0.0));
        let last_x = std::cell::Cell::new(init_x);
        let last_y = std::cell::Cell::new(init_y);

        let events_of_interest = vec![
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGEventType::ScrollWheel,
            CGEventType::LeftMouseDragged,
            CGEventType::RightMouseDragged,
            CGEventType::OtherMouseDragged,
        ];

        let tap = CGEventTap::new(
            CGEventTapLocation::HID,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            events_of_interest,
            move |_proxy: CGEventTapProxy, event_type: CGEventType, event: &CGEvent| {
                let pos = event.location();
                let prev_x = last_x.get();
                let prev_y = last_y.get();
                let dx = pos.x - prev_x;
                let dy = pos.y - prev_y;
                last_x.set(pos.x);
                last_y.set(pos.y);

                let event_type_mapped = map_event_type(event_type, event);

                if let Some(evt_type) = event_type_mapped {
                    let mouse_event = MouseEvent::now(dx, dy, evt_type);
                    let _ = sender.try_send(mouse_event);
                }

                if suppressing.load(Ordering::Relaxed) {
                    None // Suppress: swallow the event
                } else {
                    Some(event.clone()) // Pass through
                }
            },
        )
        .map_err(|_| {
            anyhow::anyhow!(
                "Failed to create event tap. \
                 Please grant Accessibility permission in \
                 System Preferences > Privacy & Security > Accessibility"
            )
        })?;

        let loop_source = tap
            .mach_port
            .create_runloop_source(0)
            .map_err(|_| anyhow::anyhow!("Failed to create run loop source"))?;

        let run_loop = CFRunLoop::get_current();
        run_loop.add_source(&loop_source, unsafe { kCFRunLoopCommonModes });
        tap.enable();

        log::info!("macOS event tap started, entering run loop");
        CFRunLoop::run_current();

        Ok(())
    }

    fn suppress_handle(&self) -> Arc<AtomicBool> {
        self.suppressing.clone()
    }
}

fn map_event_type(event_type: CGEventType, event: &CGEvent) -> Option<MouseEventType> {
    match event_type {
        CGEventType::MouseMoved
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged => Some(MouseEventType::Move),

        CGEventType::LeftMouseDown => Some(MouseEventType::ButtonDown(MouseButton::Left)),
        CGEventType::LeftMouseUp => Some(MouseEventType::ButtonUp(MouseButton::Left)),
        CGEventType::RightMouseDown => Some(MouseEventType::ButtonDown(MouseButton::Right)),
        CGEventType::RightMouseUp => Some(MouseEventType::ButtonUp(MouseButton::Right)),

        CGEventType::OtherMouseDown => {
            let btn_num =
                event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
            let button = match btn_num {
                2 => MouseButton::Middle,
                n => MouseButton::Other(n as u8),
            };
            Some(MouseEventType::ButtonDown(button))
        }
        CGEventType::OtherMouseUp => {
            let btn_num =
                event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
            let button = match btn_num {
                2 => MouseButton::Middle,
                n => MouseButton::Other(n as u8),
            };
            Some(MouseEventType::ButtonUp(button))
        }

        CGEventType::ScrollWheel => {
            let scroll_dy = event
                .get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1)
                as f64;
            let scroll_dx = event
                .get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2)
                as f64;
            Some(MouseEventType::Scroll {
                dx: scroll_dx,
                dy: scroll_dy,
            })
        }

        _ => None,
    }
}
