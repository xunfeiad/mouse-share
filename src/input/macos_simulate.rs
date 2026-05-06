use crate::protocol::MouseButton;
use anyhow::Result;
use core_graphics::display::CGDisplay;
use core_graphics::event::{CGEvent, CGEventFlags, CGEventType, CGMouseButton, EventField};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;

use super::simulate::InputSimulator;

/// Warp the visible cursor to a given point. `CGEvent::post(MouseMoved)`
/// alone only notifies apps of a move — it does NOT reliably move the
/// visible cursor on the client Mac when there is no local HID input, so we
/// call `CGWarpMouseCursorPosition` for the actual visual move.
///
/// NOTE: this function does NOT call `show_cursor`. Cursor visibility is
/// managed explicitly at the Enter/Leave boundary by the caller, using
/// `capture::hide_local_cursor` / `show_local_cursor`. Calling show_cursor on
/// every move_relative would push the refcount arbitrarily negative and make
/// later hide calls ineffective.
///
/// NOTE: this function does NOT call `associate_mouse_and_mouse_cursor_position`.
/// That call is a mode toggle that costs a syscall per invocation; we run it
/// once at construction time instead. Calling it per move starves the event
/// tap at high event rates (gaming mice at 500–1000 Hz) and is the single
/// biggest contributor to client-side stutter.
fn warp_cursor(point: CGPoint) {
    if let Err(e) = CGDisplay::warp_mouse_cursor_position(point) {
        log::error!(
            "CGWarpMouseCursorPosition failed: {:?} at ({:.0},{:.0}) \
             — check Accessibility permission for this binary",
            e, point.x, point.y
        );
    }
}

pub struct MacOsSimulator {
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
    pub fn new() -> Self {
        // Source creation is essentially infallible on a working macOS
        // session. If it fails, there is no recovery path — the client is
        // unusable without input simulation.
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .expect("failed to create CGEventSource (HIDSystemState)");
        // Couple the hardware cursor to mouse events once. This is a mode
        // toggle, not a per-event operation — the old code called it on
        // every warp, which was a major source of stutter at high event rates.
        if let Err(e) = CGDisplay::associate_mouse_and_mouse_cursor_position(true) {
            log::warn!("CGAssociateMouseAndMouseCursorPosition failed: {:?}", e);
        }
        Self {
            current_x: 0.0,
            current_y: 0.0,
            source,
        }
    }

    fn post_mouse_event(
        &self,
        event_type: CGEventType,
        point: CGPoint,
        button: CGMouseButton,
    ) -> Result<()> {
        let event = CGEvent::new_mouse_event(self.source.clone(), event_type, point, button)
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
        // Warp is the authoritative visible-move operation. We deliberately
        // DO NOT post a synthetic MouseMoved event afterward:
        //
        //   * `warp_cursor` alone is what the cursor is actually at — the
        //     window server's own cursor tracking (used by menu hover,
        //     NSTrackingArea, hit testing, etc.) picks this up natively.
        //   * Posting `MouseMoved` adds a second synchronous IPC to the
        //     window server per call (~hundreds of µs). At the 500–1000 Hz
        //     drain rate of the client event loop, that's the dominant
        //     source of visible client-side lag — the loop thread spends
        //     most of its wall clock time blocked in that IPC, causing
        //     incoming UDP packets to back up in the kernel buffer.
        //
        // Tools like Synergy / Barrier / input-leap use the same
        // "warp, don't post" pattern for exactly this reason. Apps that
        // specifically need MouseMoved via CGEventTap (rare — input
        // recorders, some accessibility tools) will not see our moves,
        // which is an acceptable tradeoff for smooth cursor tracking.
        warp_cursor(self.current_point());
        Ok(())
    }

    fn move_relative(&mut self, dx: f64, dy: f64) -> Result<()> {
        self.current_x += dx;
        self.current_y += dy;
        // Clamp to reasonable screen bounds (max 16K resolution)
        self.current_x = self.current_x.clamp(0.0, 16384.0);
        self.current_y = self.current_y.clamp(0.0, 16384.0);
        // See `move_to` for why we only warp and don't post — this is
        // the hottest path on the client and the extra IPC was the main
        // remaining source of client-side lag.
        warp_cursor(self.current_point());
        Ok(())
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
        // Create a generic event and set scroll fields manually
        let event = CGEvent::new(self.source.clone())
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
        let event = CGEvent::new_keyboard_event(self.source.clone(), keycode as u16, down)
            .map_err(|_| anyhow::anyhow!("failed to create keyboard event"))?;
        // Preserve modifier state from the server so shift+letter, cmd+c,
        // ctrl+space etc. produce the right character / shortcut locally.
        event.set_flags(CGEventFlags::from_bits_truncate(flags));
        event.post(core_graphics::event::CGEventTapLocation::HID);
        Ok(())
    }
}
