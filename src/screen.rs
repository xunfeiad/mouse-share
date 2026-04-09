use crate::protocol::ScreenInfo;
use anyhow::Result;

pub fn get_screen_info() -> Result<ScreenInfo> {
    #[cfg(target_os = "macos")]
    {
        use core_graphics::display::CGDisplay;
        let display = CGDisplay::main();
        Ok(ScreenInfo {
            width: display.pixels_wide() as u32,
            height: display.pixels_high() as u32,
        })
    }
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
        unsafe {
            Ok(ScreenInfo {
                width: GetSystemMetrics(SM_CXSCREEN) as u32,
                height: GetSystemMetrics(SM_CYSCREEN) as u32,
            })
        }
    }
}
