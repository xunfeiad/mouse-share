use crate::protocol::{KeyEvent, MouseButton, MouseEvent, MouseEventType};
use anyhow::Result;
use core_foundation::runloop::{
    kCFRunLoopCommonModes, kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopRunResult,
};
use core_graphics::event::{
    CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType, EventField,
};
use crossbeam_channel::Sender;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::capture::{CapturedInput, InputCapture};

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
    fn run(&mut self, sender: Sender<CapturedInput>, shutdown: Arc<AtomicBool>) -> Result<()> {
        let suppressing = self.suppressing.clone();

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
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
        ];

        let tap = CGEventTap::new(
            CGEventTapLocation::HID,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            events_of_interest,
            move |_proxy: CGEventTapProxy, event_type: CGEventType, event: &CGEvent| {
                match event_type {
                    CGEventType::KeyDown
                    | CGEventType::KeyUp
                    | CGEventType::FlagsChanged => {
                        // Keyboard. FlagsChanged is fired for modifier key
                        // (shift/ctrl/opt/cmd) transitions and has no down/up;
                        // we treat it as a keydown and rely on the flags
                        // snapshot to carry the current modifier state.
                        let keycode = event.get_integer_value_field(
                            EventField::KEYBOARD_EVENT_KEYCODE,
                        ) as u32;
                        let flags = event.get_flags().bits() as u64;
                        let down = !matches!(event_type, CGEventType::KeyUp);
                        log::info!(
                            "key captured: code={} down={} flags=0x{:x}",
                            keycode, down, flags
                        );
                        let key_event = KeyEvent { keycode, down, flags };
                        let _ = sender.try_send(CapturedInput::Key(key_event));
                    }
                    _ => {
                        // Mouse: read raw HID delta from event fields. This
                        // is the relative movement as reported by the mouse
                        // hardware, independent of cursor position clamping
                        // at screen edges — crucial because when suppression
                        // is ON, the OS freezes the cursor and
                        // event.location() stops changing.
                        let dx = event
                            .get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X)
                            as f64;
                        let dy = event
                            .get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y)
                            as f64;

                        if let Some(evt_type) = map_event_type(event_type, event) {
                            let mouse_event = MouseEvent::now(dx, dy, evt_type);
                            let _ = sender.try_send(CapturedInput::Mouse(mouse_event));
                        }
                    }
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
        // Poll the runloop in short slices so we can notice `shutdown` and
        // break out cleanly. `run_current()` would block forever and leave
        // no way for the UI to stop the backend.
        loop {
            if shutdown.load(Ordering::SeqCst) {
                log::info!("macOS event tap: shutdown requested, exiting run loop");
                break;
            }
            let result = CFRunLoop::run_in_mode(
                unsafe { kCFRunLoopDefaultMode },
                Duration::from_millis(100),
                false,
            );
            if matches!(result, CFRunLoopRunResult::Finished | CFRunLoopRunResult::Stopped) {
                break;
            }
        }

        run_loop.remove_source(&loop_source, unsafe { kCFRunLoopCommonModes });
        // Dropping `tap` disables and releases it.
        drop(tap);
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
