use crate::protocol::MouseButton;
use anyhow::Result;

/// Trait for simulating mouse input on the client side.
pub trait InputSimulator: Send {
    fn move_to(&mut self, x: f64, y: f64) -> Result<()>;
    fn move_relative(&mut self, dx: f64, dy: f64) -> Result<()>;
    fn button_down(&mut self, button: MouseButton) -> Result<()>;
    fn button_up(&mut self, button: MouseButton) -> Result<()>;
    fn scroll(&mut self, dx: f64, dy: f64) -> Result<()>;
}

pub fn create_simulator() -> Box<dyn InputSimulator> {
    #[cfg(target_os = "macos")]
    {
        Box::new(super::macos_simulate::MacOsSimulator::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(super::win_simulate::WinSimulator::new())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        compile_error!("Unsupported platform. Only macOS and Windows are supported.");
    }
}
