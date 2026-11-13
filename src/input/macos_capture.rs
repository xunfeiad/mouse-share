use crate::protocol::{KeyEvent, MouseButton, MouseEvent};
use anyhow::Result;
use core_foundation::runloop::{
    kCFRunLoopCommonModes, kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopRunResult,
};
use core_graphics::event::{
    CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType, EventField,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::capture::{CaptureQueue, CapturedInput, InputCapture, InputRouting, Suppression};

pub struct MacOsCapture {
    suppressing: Arc<Suppression>,
}

impl MacOsCapture {
    pub fn new() -> Self {
        Self {
            suppressing: Arc::new(Suppression::default()),
        }
    }
}

impl InputCapture for MacOsCapture {
    fn run(&mut self, sender: CaptureQueue, shutdown: Arc<AtomicBool>) -> Result<()> {
        let suppressing = self.suppressing.clone();
        let routing = std::cell::RefCell::new(InputRouting::default());

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
                if event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA) == 0x4d53_5632
                {
                    return Some(event.clone());
                }
                let swallow = match event_type {
                    CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                        suppressing.fail_open();
                        return Some(event.clone());
                    }
                    CGEventType::KeyDown | CGEventType::KeyUp | CGEventType::FlagsChanged => {
                        let keycode = event
                            .get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE)
                            as u32;
                        let flags = event.get_flags().bits();
                        if keycode == 57 && matches!(event_type, CGEventType::FlagsChanged) {
                            // Caps Lock is a toggle, never a continuously held key.
                            let remote = suppressing.keyboard.load(Ordering::Acquire);
                            if remote {
                                for down in [true, false] {
                                    sender.send(CapturedInput::Key(KeyEvent {
                                        keycode,
                                        down,
                                        flags,
                                    }));
                                }
                            }
                            remote
                        } else {
                            let down = match event_type {
                                CGEventType::FlagsChanged => modifier_down(keycode, flags),
                                CGEventType::KeyDown => true,
                                _ => false,
                            };
                            let (swallow, send) =
                                routing.borrow_mut().key(keycode, down, &suppressing);
                            if send {
                                sender.send(CapturedInput::Key(KeyEvent {
                                    keycode,
                                    down,
                                    flags,
                                }));
                            }
                            swallow
                        }
                    }
                    _ => {
                        let dx =
                            event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X) as f64;
                        let dy =
                            event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y) as f64;
                        if let Some(mouse) = map_event(event_type, event, dx, dy) {
                            let (swallow, send) = routing.borrow_mut().mouse(&mouse, &suppressing);
                            if send {
                                let loc = event.location();
                                sender.send(CapturedInput::Mouse {
                                    event: mouse,
                                    abs_x: loc.x,
                                    abs_y: loc.y,
                                });
                            }
                            swallow
                        } else {
                            false
                        }
                    }
                };
                if swallow {
                    None
                } else {
                    Some(event.clone())
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
        self.suppressing.ready.store(true, Ordering::Release);
        log::info!("macOS input tap started");
        while !shutdown.load(Ordering::Acquire) && !self.suppressing.failed.load(Ordering::Acquire)
        {
            let result = CFRunLoop::run_in_mode(
                unsafe { kCFRunLoopDefaultMode },
                Duration::from_millis(16),
                false,
            );
            if matches!(
                result,
                CFRunLoopRunResult::Finished | CFRunLoopRunResult::Stopped
            ) {
                break;
            }
        }

        run_loop.remove_source(&loop_source, unsafe { kCFRunLoopCommonModes });
        // Dropping `tap` disables and releases it.
        drop(tap);
        Ok(())
    }

    fn suppress_handle(&self) -> Arc<Suppression> {
        self.suppressing.clone()
    }
}

// Quartz exposes side-specific device modifier flags in addition to the
// combined modifier flags. A combined flag alone cannot distinguish sides.
fn modifier_down(keycode: u32, flags: u64) -> bool {
    let mask = match keycode {
        54 => 0x10,
        55 => 0x08, // Command
        56 => 0x02,
        60 => 0x04, // Shift
        58 => 0x20,
        61 => 0x40, // Option
        59 => 0x01,
        62 => 0x2000,   // Control
        57 => 0x10000,  // Caps Lock
        63 => 0x800000, // Fn
        _ => 0,
    };
    flags & mask != 0
}

fn map_event(event_type: CGEventType, event: &CGEvent, dx: f64, dy: f64) -> Option<MouseEvent> {
    match event_type {
        CGEventType::MouseMoved
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged => Some(MouseEvent::Move { dx, dy }),

        CGEventType::LeftMouseDown => Some(MouseEvent::ButtonDown(MouseButton::Left)),
        CGEventType::LeftMouseUp => Some(MouseEvent::ButtonUp(MouseButton::Left)),
        CGEventType::RightMouseDown => Some(MouseEvent::ButtonDown(MouseButton::Right)),
        CGEventType::RightMouseUp => Some(MouseEvent::ButtonUp(MouseButton::Right)),

        CGEventType::OtherMouseDown => {
            let btn_num = event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
            let button = match btn_num {
                2 => MouseButton::Middle,
                3 => MouseButton::Other(1),
                4 => MouseButton::Other(2),
                _ => return None,
            };
            Some(MouseEvent::ButtonDown(button))
        }
        CGEventType::OtherMouseUp => {
            let btn_num = event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
            let button = match btn_num {
                2 => MouseButton::Middle,
                3 => MouseButton::Other(1),
                4 => MouseButton::Other(2),
                _ => return None,
            };
            Some(MouseEvent::ButtonUp(button))
        }

        CGEventType::ScrollWheel => {
            let scroll_dy =
                event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1) as f64;
            let scroll_dx =
                event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2) as f64;
            Some(MouseEvent::Scroll {
                dx: scroll_dx,
                dy: scroll_dy,
            })
        }

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::modifier_down;

    #[test]
    fn releasing_left_shift_keeps_right_shift_held() {
        let both = 0x0002_0006;
        let right = 0x0002_0004;
        assert!(modifier_down(56, both));
        assert!(modifier_down(60, both));
        assert!(!modifier_down(56, right));
        assert!(modifier_down(60, right));
    }
}
