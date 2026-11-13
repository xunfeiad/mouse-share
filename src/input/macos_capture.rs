use crate::protocol::{KeyEvent, MouseButton, MouseEvent};
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

use super::capture::{CaptureControl, CapturedInput, InputCapture, InputRouting};

pub struct MacOsCapture {
    suppressing: Arc<CaptureControl>,
}

impl MacOsCapture {
    pub fn new() -> Self {
        Self {
            suppressing: Arc::new(CaptureControl::default()),
        }
    }
}

impl InputCapture for MacOsCapture {
    fn run(&mut self, sender: Sender<CapturedInput>, shutdown: Arc<AtomicBool>) -> Result<()> {
        let suppressing = self.suppressing.clone();
        // Set by the tap callback when macOS disables the tap (timeout or
        // user-input stall). The outer run-loop polls this and re-enables.
        // Without this, a single slow callback permanently kills the tap:
        // events flow through unsuppressed, the local cursor tracks the
        // physical mouse even though the server thinks it's forwarding,
        // and the user sees "cursor still moving on the server even though
        // mouse is supposed to be on the client".
        let tap_disabled = Arc::new(AtomicBool::new(false));
        let tap_disabled_cb = tap_disabled.clone();

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

        let routing = std::cell::RefCell::new(InputRouting::default());
        let tap = CGEventTap::new(
            CGEventTapLocation::HID,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            events_of_interest,
            move |_proxy: CGEventTapProxy, event_type: CGEventType, event: &CGEvent| {
                let mut swallow = false;
                match event_type {
                    CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                        suppressing.release();
                        suppressing.fault.store(true, Ordering::SeqCst);
                        tap_disabled_cb.store(true, Ordering::SeqCst);
                        return Some(event.clone());
                    }
                    CGEventType::KeyDown | CGEventType::KeyUp | CGEventType::FlagsChanged => {
                        let keycode = event
                            .get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE)
                            as u32;
                        let flags = event.get_flags().bits();
                        let down = if matches!(event_type, CGEventType::FlagsChanged) {
                            modifier_down(keycode, flags)
                        } else {
                            matches!(event_type, CGEventType::KeyDown)
                        };
                        if keycode == 0x39 && matches!(event_type, CGEventType::FlagsChanged) {
                            // Caps Lock is a toggle, not a physically held modifier.
                            let enabled = suppressing.keyboard.load(Ordering::SeqCst);
                            let (suppress, remote) =
                                routing.borrow_mut().key(keycode, true, enabled);
                            suppressing.send(
                                &sender,
                                CapturedInput::Key {
                                    event: KeyEvent {
                                        keycode,
                                        down: true,
                                        flags,
                                    },
                                    remote,
                                },
                            );
                            routing.borrow_mut().key(keycode, false, enabled);
                            suppressing.send(
                                &sender,
                                CapturedInput::Key {
                                    event: KeyEvent {
                                        keycode,
                                        down: false,
                                        flags,
                                    },
                                    remote,
                                },
                            );
                            return if suppress { None } else { Some(event.clone()) };
                        }
                        let (suppress, remote) = routing.borrow_mut().key(
                            keycode,
                            down,
                            suppressing.keyboard.load(Ordering::SeqCst),
                        );
                        swallow = suppress;
                        suppressing.send(
                            &sender,
                            CapturedInput::Key {
                                event: KeyEvent {
                                    keycode,
                                    down,
                                    flags,
                                },
                                remote,
                            },
                        );
                    }
                    _ => {
                        let dx =
                            event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X) as f64;
                        let dy =
                            event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y) as f64;
                        if let Some(mouse_event) = map_event(event_type, event, dx, dy) {
                            let (suppress, remote) = routing
                                .borrow_mut()
                                .mouse(&mouse_event, suppressing.mouse.load(Ordering::SeqCst));
                            swallow = suppress;
                            let loc = event.location();
                            suppressing.send(
                                &sender,
                                CapturedInput::Mouse {
                                    event: mouse_event,
                                    abs_x: loc.x,
                                    abs_y: loc.y,
                                    remote,
                                },
                            );
                        }
                    }
                }
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
        self.suppressing.ready.store(true, Ordering::SeqCst);

        log::info!("macOS event tap started, entering run loop");
        // Poll the runloop in short slices so we can notice `shutdown` and
        // break out cleanly. `run_current()` would block forever and leave
        // no way for the UI to stop the backend.
        //
        // 16 ms poll instead of 100 ms: when macOS auto-disables the tap
        // (TapDisabledByTimeout / TapDisabledByUserInput) we only see it
        // at the next poll cycle. With a 100 ms slice, up to 100 ms of
        // events (clicks included!) can flow through unsuppressed — this
        // is one root cause of the "click fires on server too" bug. A
        // 16 ms slice caps the leak window at ~one display frame.
        loop {
            if shutdown.load(Ordering::SeqCst) {
                log::info!("macOS event tap: shutdown requested, exiting run loop");
                break;
            }
            // Re-enable the tap if macOS disabled it during the last
            // callback cycle. `tap.enable()` is idempotent so the check
            // doesn't need to be perfectly precise — worst case we call
            // it an extra time.
            if tap_disabled.swap(false, Ordering::SeqCst) {
                log::info!("Re-enabling macOS event tap after disable");
                tap.enable();
            }
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

    fn control_handle(&self) -> Arc<CaptureControl> {
        self.suppressing.clone()
    }
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
                n => MouseButton::Other(n as u8),
            };
            Some(MouseEvent::ButtonDown(button))
        }
        CGEventType::OtherMouseUp => {
            let btn_num = event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
            let button = match btn_num {
                2 => MouseButton::Middle,
                n => MouseButton::Other(n as u8),
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

/// Device-dependent modifier masks distinguish left/right keys, including
/// releasing one Shift while the other remains held. Caps Lock is a toggle.
fn modifier_down(code: u32, flags: u64) -> bool {
    let mask = match code {
        0x38 => 0x0002,
        0x3c => 0x0004, // left/right shift
        0x3b => 0x0001,
        0x3e => 0x2000, // left/right control
        0x3a => 0x0020,
        0x3d => 0x0040, // left/right option
        0x37 => 0x0008,
        0x36 => 0x0010, // left/right command
        0x39 => 0x10000,
        0x3f => 0x800000,
        _ => 0,
    };
    flags & mask != 0
}

#[cfg(test)]
mod tests {
    use super::modifier_down;
    #[test]
    fn modifier_sides_release_independently() {
        for (left, right, left_flag, right_flag, common) in [
            (0x38, 0x3c, 0x2, 0x4, 1 << 17),
            (0x3b, 0x3e, 0x1, 0x2000, 1 << 18),
            (0x3a, 0x3d, 0x20, 0x40, 1 << 19),
            (0x37, 0x36, 0x8, 0x10, 1 << 20),
        ] {
            assert!(modifier_down(left, left_flag | right_flag | common));
            assert!(!modifier_down(left, right_flag | common));
            assert!(modifier_down(right, right_flag | common));
            assert!(!modifier_down(right, 0));
        }
    }
}
