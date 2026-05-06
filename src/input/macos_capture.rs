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

        let tap = CGEventTap::new(
            CGEventTapLocation::HID,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            events_of_interest,
            move |_proxy: CGEventTapProxy, event_type: CGEventType, event: &CGEvent| {
                match event_type {
                    // macOS sends these special event types through the tap
                    // callback when it has disabled the tap. Signal the
                    // outer loop to re-enable, then pass through.
                    CGEventType::TapDisabledByTimeout
                    | CGEventType::TapDisabledByUserInput => {
                        log::warn!(
                            "macOS event tap disabled ({:?}), scheduling re-enable",
                            event_type
                        );
                        tap_disabled_cb.store(true, Ordering::SeqCst);
                        return Some(event.clone());
                    }
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

                        if let Some(mouse_event) = map_event(event_type, event, dx, dy) {
                            let loc = event.location();
                            let _ = sender.try_send(CapturedInput::Mouse {
                                event: mouse_event,
                                abs_x: loc.x,
                                abs_y: loc.y,
                            });
                        }
                    }
                }

                // Upgraded to SeqCst to remove any doubt about the read
                // seeing the latest store from the server event loop.
                // This costs a memory barrier per event (~ns) — negligible
                // at human mouse rates.
                let suppress = suppressing.load(Ordering::SeqCst);
                // Diagnostic: when a button / scroll event flows through the
                // tap, log which side of the suppress fence it took. This
                // nails down whether the "click fires on server too" bug is
                // (a) tap returning None but macOS ignoring it, or (b) the
                // suppress flag not being set at that moment. Moves are not
                // logged — they're too frequent.
                let is_discrete = matches!(
                    event_type,
                    CGEventType::LeftMouseDown
                        | CGEventType::LeftMouseUp
                        | CGEventType::RightMouseDown
                        | CGEventType::RightMouseUp
                        | CGEventType::OtherMouseDown
                        | CGEventType::OtherMouseUp
                        | CGEventType::ScrollWheel
                );
                if is_discrete {
                    if suppress {
                        log::info!("tap suppressing {:?}", event_type);
                    } else {
                        log::info!("tap passing through {:?}", event_type);
                    }
                }

                if suppress {
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
            let btn_num =
                event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
            let button = match btn_num {
                2 => MouseButton::Middle,
                n => MouseButton::Other(n as u8),
            };
            Some(MouseEvent::ButtonDown(button))
        }
        CGEventType::OtherMouseUp => {
            let btn_num =
                event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
            let button = match btn_num {
                2 => MouseButton::Middle,
                n => MouseButton::Other(n as u8),
            };
            Some(MouseEvent::ButtonUp(button))
        }

        CGEventType::ScrollWheel => {
            let scroll_dy = event
                .get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1)
                as f64;
            let scroll_dx = event
                .get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2)
                as f64;
            Some(MouseEvent::Scroll {
                dx: scroll_dx,
                dy: scroll_dy,
            })
        }

        _ => None,
    }
}
