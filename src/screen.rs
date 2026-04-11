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

/// Returns the refresh rate of the primary display in Hz. The cursor
/// warp rate on the client side is capped to this value — warping faster
/// than the display refreshes just backlogs the window server pipeline
/// and manifests as lag; warping slower than refresh drops frames of
/// motion. Matching the rate to the actual display makes the client
/// feel smooth on any hardware (60 Hz office monitor, 120 Hz ProMotion,
/// 144/240 Hz gaming display) without a hard-coded tuning constant.
///
/// Falls back to 120 Hz when the OS can't report a rate — safe because
/// 120 Hz is handled correctly by every modern display either directly
/// (ProMotion / gaming) or by dropping every other frame (60 Hz).
pub fn get_display_refresh_hz() -> f64 {
    #[cfg(target_os = "macos")]
    {
        use core_graphics::display::CGDisplay;
        if let Some(mode) = CGDisplay::main().display_mode() {
            let hz = mode.refresh_rate();
            if hz > 1.0 {
                return hz;
            }
            // Built-in Apple displays (MacBook / Studio Display) often
            // report 0.0 because they're variable-refresh. Assume 120
            // for modern ProMotion hardware — still safe on 60 Hz since
            // we rate-limit TO this value, not AT this value.
            return 120.0;
        }
        120.0
    }
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Graphics::Gdi::{
            GetDC, GetDeviceCaps, ReleaseDC, VREFRESH, HDC,
        };
        unsafe {
            let hdc: HDC = GetDC(None);
            if hdc.is_invalid() {
                return 60.0;
            }
            let hz = GetDeviceCaps(hdc, VREFRESH);
            ReleaseDC(None, hdc);
            if hz > 1 { hz as f64 } else { 60.0 }
        }
    }
}
