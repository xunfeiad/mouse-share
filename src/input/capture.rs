use crate::protocol::MouseEvent;
use anyhow::Result;
use crossbeam_channel::Sender;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Trait for capturing global mouse events on the server side.
pub trait InputCapture: Send {
    /// Start the capture loop. Sends captured events through `sender`.
    /// This blocks the calling thread (runs an event loop).
    fn run(&mut self, sender: Sender<MouseEvent>) -> Result<()>;

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

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("failed to create CGEventSource"))?;
    let event = CGEvent::new(source)
        .map_err(|_| anyhow::anyhow!("failed to create CGEvent"))?;
    let pos = event.location();
    Ok((pos.x, pos.y))
}

#[cfg(target_os = "windows")]
fn platform_get_cursor_position() -> Result<(f64, f64)> {
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    use windows::Win32::Foundation::POINT;

    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point)? };
    Ok((point.x as f64, point.y as f64))
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
