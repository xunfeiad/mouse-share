use crate::protocol::{KeyEvent, MouseEvent};
use anyhow::Result;
use crossbeam_channel::Sender;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Unified captured input — mouse or keyboard. Both share the same channel
/// so the server event loop can apply the "only forward while mouse is on
/// client" rule uniformly to both streams.
#[derive(Debug, Clone)]
pub enum CapturedInput {
    Mouse(MouseEvent),
    Key(KeyEvent),
}

/// Trait for capturing global input events on the server side.
pub trait InputCapture: Send {
    /// Start the capture loop. Sends captured events through `sender`.
    /// This blocks the calling thread (runs an event loop). The loop exits
    /// cleanly when `shutdown` flips to `true`.
    fn run(&mut self, sender: Sender<CapturedInput>, shutdown: Arc<AtomicBool>) -> Result<()>;

    /// Get a handle to toggle event suppression.
    /// When suppressed, events are consumed (not delivered to the local OS).
    fn suppress_handle(&self) -> Arc<AtomicBool>;
}

/// Get the current absolute cursor position (x, y).
pub fn get_cursor_position() -> Result<(f64, f64)> {
    platform_get_cursor_position()
}

#[cfg(target_os = "macos")]
fn platform_get_cursor_position() -> Result<(f64, f64)> {
    use core_graphics::event::CGEvent;
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use std::cell::RefCell;

    // Cache the CGEventSource per thread. Creating a fresh source costs
    // hundreds of microseconds; re-using one (clone = CFRetain) is an atomic
    // increment. This function is called per mouse event during edge
    // detection while the mouse is on the server, so the savings add up.
    thread_local! {
        static SOURCE: RefCell<Option<CGEventSource>> = const { RefCell::new(None) };
    }

    SOURCE.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(
                CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                    .map_err(|_| anyhow::anyhow!("failed to create CGEventSource"))?,
            );
        }
        let source = slot.as_ref().unwrap().clone();
        let event = CGEvent::new(source)
            .map_err(|_| anyhow::anyhow!("failed to create CGEvent"))?;
        let pos = event.location();
        Ok((pos.x, pos.y))
    })
}

#[cfg(target_os = "windows")]
fn platform_get_cursor_position() -> Result<(f64, f64)> {
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    use windows::Win32::Foundation::POINT;

    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point)? };
    Ok((point.x as f64, point.y as f64))
}

/// Hide the local system cursor. Used by the server when forwarding input to
/// a remote client, so the user sees only the remote cursor.
/// Caller is responsible for balancing hide/show calls (they use a refcount
/// on macOS — two hides require two shows).
pub fn hide_local_cursor() {
    #[cfg(target_os = "macos")]
    {
        use core_graphics::display::CGDisplay;
        if let Err(e) = CGDisplay::main().hide_cursor() {
            log::warn!("hide_cursor failed: {:?}", e);
        }
    }
    #[cfg(target_os = "windows")]
    {
        // Note: Win32 ShowCursor is per-thread and does not reliably hide the
        // system cursor from a console app. A proper Windows implementation
        // would use SetSystemCursor with a transparent cursor. For now this
        // is a no-op on Windows — the local cursor will remain visible.
    }
}

/// Show the local system cursor. Balances a previous `hide_local_cursor`.
pub fn show_local_cursor() {
    #[cfg(target_os = "macos")]
    {
        use core_graphics::display::CGDisplay;
        if let Err(e) = CGDisplay::main().show_cursor() {
            log::warn!("show_cursor failed: {:?}", e);
        }
    }
}

/// Promote this process to a macOS foreground application. Required for
/// `CGDisplayHideCursor` to actually take effect: plain CLI binaries are
/// "background" processes in macOS's eyes, and cursor-hiding calls from
/// background processes silently no-op.
///
/// Tradeoff: the process will appear in the Dock and Cmd+Tab switcher.
/// Hiding it would require bundling as a proper `.app` with
/// `LSUIElement=YES` in Info.plist, which is a bigger packaging change.
///
/// Safe to call multiple times — TransformProcessType is idempotent
/// after the first promotion.
pub fn promote_to_foreground_app() {
    #[cfg(target_os = "macos")]
    {
        #[repr(C)]
        struct ProcessSerialNumber {
            high_long_of_psn: u32,
            low_long_of_psn: u32,
        }

        // Constants from Carbon's Processes.h
        const K_CURRENT_PROCESS: u32 = 2;
        const K_PROCESS_TRANSFORM_TO_FOREGROUND_APPLICATION: u32 = 1;

        #[link(name = "ApplicationServices", kind = "framework")]
        extern "C" {
            fn TransformProcessType(
                psn: *const ProcessSerialNumber,
                transform_state: u32,
            ) -> i32;
        }

        let psn = ProcessSerialNumber {
            high_long_of_psn: 0,
            low_long_of_psn: K_CURRENT_PROCESS,
        };
        let status = unsafe {
            TransformProcessType(&psn, K_PROCESS_TRANSFORM_TO_FOREGROUND_APPLICATION)
        };
        if status == 0 {
            log::info!("Promoted to foreground application (for cursor hiding)");
        } else {
            log::warn!(
                "TransformProcessType failed status={} — cursor hiding may be ineffective",
                status
            );
        }
    }
}

pub fn create_capture() -> Box<dyn InputCapture> {
    #[cfg(target_os = "macos")]
    {
        Box::new(super::macos_capture::MacOsCapture::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(super::win_capture::WinCapture::new())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        compile_error!("Unsupported platform. Only macOS and Windows are supported.");
    }
}
