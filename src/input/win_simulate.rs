use crate::protocol::MouseButton;
use anyhow::Result;

use super::simulate::InputSimulator;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

pub struct WinSimulator {
    current_x: f64,
    current_y: f64,
    screen_w: f64,
    screen_h: f64,
}

impl WinSimulator {
    pub fn new() -> Self {
        let (w, h) = unsafe {
            (
                GetSystemMetrics(SM_CXSCREEN) as f64,
                GetSystemMetrics(SM_CYSCREEN) as f64,
            )
        };
        Self {
            current_x: 0.0,
            current_y: 0.0,
            screen_w: w,
            screen_h: h,
        }
    }

    fn send_mouse_input(&self, flags: MOUSE_EVENT_FLAGS, data: i32) -> Result<()> {
        // Windows absolute coordinates use 0-65535 normalized range
        let abs_x = (self.current_x / self.screen_w * 65535.0) as i32;
        let abs_y = (self.current_y / self.screen_h * 65535.0) as i32;

        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: abs_x,
                    dy: abs_y,
                    mouseData: data as u32,
                    dwFlags: flags | MOUSEEVENTF_ABSOLUTE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };

        unsafe {
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }
        Ok(())
    }
}

impl InputSimulator for WinSimulator {
    fn move_to(&mut self, x: f64, y: f64) -> Result<()> {
        self.current_x = x;
        self.current_y = y;
        self.send_mouse_input(MOUSEEVENTF_MOVE, 0)
    }

    fn move_relative(&mut self, dx: f64, dy: f64) -> Result<()> {
        self.current_x = (self.current_x + dx).clamp(0.0, self.screen_w - 1.0);
        self.current_y = (self.current_y + dy).clamp(0.0, self.screen_h - 1.0);
        self.send_mouse_input(MOUSEEVENTF_MOVE, 0)
    }

    fn button_down(&mut self, button: MouseButton) -> Result<()> {
        let (flags, data) = match button {
            MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, 0),
            MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, 0),
            MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, 0),
            MouseButton::Other(n) => (MOUSEEVENTF_XDOWN, n as i32),
        };
        self.send_mouse_input(flags | MOUSEEVENTF_MOVE, data)
    }

    fn button_up(&mut self, button: MouseButton) -> Result<()> {
        let (flags, data) = match button {
            MouseButton::Left => (MOUSEEVENTF_LEFTUP, 0),
            MouseButton::Right => (MOUSEEVENTF_RIGHTUP, 0),
            MouseButton::Middle => (MOUSEEVENTF_MIDDLEUP, 0),
            MouseButton::Other(n) => (MOUSEEVENTF_XUP, n as i32),
        };
        self.send_mouse_input(flags | MOUSEEVENTF_MOVE, data)
    }

    fn scroll(&mut self, dx: f64, dy: f64) -> Result<()> {
        if dy != 0.0 {
            let wheel_delta = (dy * 120.0) as i32;
            self.send_mouse_input(MOUSEEVENTF_WHEEL, wheel_delta)?;
        }
        if dx != 0.0 {
            let wheel_delta = (dx * 120.0) as i32;
            self.send_mouse_input(MOUSEEVENTF_HWHEEL, wheel_delta)?;
        }
        Ok(())
    }

    fn key_event(&mut self, keycode: u32, down: bool, flags: u64) -> Result<()> {
        // `flags & 1` is the extended-key bit carried over from the Windows
        // source capture (LLKHF_EXTENDED). For sources that don't populate
        // it (e.g. a future non-Windows server), fall back to a VK heuristic
        // so arrows/navigation keys still work.
        let extended_from_flags = (flags & 0x1) != 0;
        let extended =
            extended_from_flags || is_extended_key(keycode as u16);

        let mut kflags: u32 = 0;
        if !down {
            kflags |= KEYEVENTF_KEYUP.0;
        }
        if extended {
            kflags |= KEYEVENTF_EXTENDEDKEY.0;
        }

        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(keycode as u16),
                    wScan: 0,
                    dwFlags: KEYBD_EVENT_FLAGS(kflags),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe {
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }
        Ok(())
    }
}

/// VK codes that require `KEYEVENTF_EXTENDEDKEY` when synthesized via
/// `SendInput`. Used as a fallback when the source didn't populate the
/// extended bit in the protocol flags.
fn is_extended_key(vk: u16) -> bool {
    matches!(
        vk,
        0xA3 | 0xA5 | // VK_RCONTROL, VK_RMENU
        0x2D | 0x2E | // VK_INSERT, VK_DELETE
        0x24 | 0x23 | // VK_HOME, VK_END
        0x21 | 0x22 | // VK_PRIOR, VK_NEXT
        0x25 | 0x26 | 0x27 | 0x28 | // VK_LEFT/UP/RIGHT/DOWN
        0x90 | // VK_NUMLOCK
        0x6F | // VK_DIVIDE (numpad /)
        0x2C   // VK_SNAPSHOT (PrtSc)
    )
}
