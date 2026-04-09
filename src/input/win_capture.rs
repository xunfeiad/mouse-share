use crate::protocol::{KeyEvent, MouseButton, MouseEvent, MouseEventType};
use anyhow::Result;
use crossbeam_channel::Sender;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::capture::{CapturedInput, InputCapture};

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, GetSystemMetrics, SetCursorPos, SetWindowsHookExW,
    UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, SM_CXSCREEN, SM_CYSCREEN,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

/// `MSLLHOOKSTRUCT.flags` bit set when the event was synthesized via SendInput
/// or SetCursorPos. We must skip these so our own cursor-recenter calls don't
/// feed back into the delta computation.
const LLMHF_INJECTED: u32 = 0x00000001;

/// `KBDLLHOOKSTRUCT.flags` bit indicating the scan code was preceded by a
/// prefix byte having the value 0xE0 — i.e. this is an extended key like
/// arrows, right-Ctrl/Alt, Insert/Delete/Home/End/PgUp/PgDn, numpad divide.
const LLKHF_EXTENDED: u32 = 0x00000001;

pub struct WinCapture {
    suppressing: Arc<AtomicBool>,
}

impl WinCapture {
    pub fn new() -> Self {
        Self {
            suppressing: Arc::new(AtomicBool::new(false)),
        }
    }
}

// Thread-local storage for the hook callback context
thread_local! {
    static HOOK_SENDER: std::cell::RefCell<Option<Sender<CapturedInput>>> = std::cell::RefCell::new(None);
    static HOOK_SUPPRESS: std::cell::RefCell<Option<Arc<AtomicBool>>> = std::cell::RefCell::new(None);
    // None until we see the first mouse event, then tracks the last observed
    // cursor position for delta computation. Lazy-init avoids a huge spurious
    // dx/dy spike on the first event.
    static LAST_POS: std::cell::Cell<Option<(i32, i32)>> = std::cell::Cell::new(None);
    // Cached primary screen dimensions, used to recenter the cursor while
    // suppressing (so info.pt never clamps at an edge and kills the delta).
    static SCREEN_SIZE: std::cell::Cell<(i32, i32)> = std::cell::Cell::new((0, 0));
}

impl InputCapture for WinCapture {
    fn run(&mut self, sender: Sender<CapturedInput>) -> Result<()> {
        // Store sender and suppress flag in thread-local storage
        HOOK_SENDER.with(|s| *s.borrow_mut() = Some(sender));
        HOOK_SUPPRESS.with(|s| *s.borrow_mut() = Some(self.suppressing.clone()));

        // Cache primary screen dimensions for cursor recentering.
        let (sw, sh) = unsafe {
            (
                GetSystemMetrics(SM_CXSCREEN),
                GetSystemMetrics(SM_CYSCREEN),
            )
        };
        SCREEN_SIZE.with(|s| s.set((sw, sh)));

        let mouse_hook = unsafe {
            SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), None, 0)?
        };
        let kbd_hook = unsafe {
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(kbd_hook_proc), None, 0)?
        };

        log::info!("Windows mouse + keyboard hooks installed, entering message loop");

        // Windows requires a message pump on the hook thread
        let mut msg = MSG::default();
        unsafe {
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                // No dispatch needed, we only care about the hooks
            }
        }

        unsafe { UnhookWindowsHookEx(mouse_hook)? };
        unsafe { UnhookWindowsHookEx(kbd_hook)? };
        Ok(())
    }

    fn suppress_handle(&self) -> Arc<AtomicBool> {
        self.suppressing.clone()
    }
}

unsafe extern "system" fn mouse_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);

    // Skip events we injected ourselves (SetCursorPos recenter calls). If we
    // didn't skip them, the recenter event would overwrite LAST_POS and zero
    // out the delta computation on the very next real event.
    if (info.flags & LLMHF_INJECTED) != 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    // Lazy-init LAST_POS on first event — avoids a massive spurious delta
    // from the default (0, 0).
    let (last_x, last_y) = LAST_POS.with(|p| match p.get() {
        Some(prev) => prev,
        None => {
            p.set(Some((info.pt.x, info.pt.y)));
            (info.pt.x, info.pt.y)
        }
    });
    let dx = (info.pt.x - last_x) as f64;
    let dy = (info.pt.y - last_y) as f64;
    LAST_POS.with(|p| p.set(Some((info.pt.x, info.pt.y))));

    let w = wparam.0 as u32;
    let event_type = match w {
        x if x == WM_MOUSEMOVE => Some(MouseEventType::Move),
        x if x == WM_LBUTTONDOWN => Some(MouseEventType::ButtonDown(MouseButton::Left)),
        x if x == WM_LBUTTONUP => Some(MouseEventType::ButtonUp(MouseButton::Left)),
        x if x == WM_RBUTTONDOWN => Some(MouseEventType::ButtonDown(MouseButton::Right)),
        x if x == WM_RBUTTONUP => Some(MouseEventType::ButtonUp(MouseButton::Right)),
        x if x == WM_MBUTTONDOWN => Some(MouseEventType::ButtonDown(MouseButton::Middle)),
        x if x == WM_MBUTTONUP => Some(MouseEventType::ButtonUp(MouseButton::Middle)),
        x if x == WM_XBUTTONDOWN => {
            let xbutton = ((info.mouseData >> 16) & 0xFFFF) as u8;
            Some(MouseEventType::ButtonDown(MouseButton::Other(xbutton)))
        }
        x if x == WM_XBUTTONUP => {
            let xbutton = ((info.mouseData >> 16) & 0xFFFF) as u8;
            Some(MouseEventType::ButtonUp(MouseButton::Other(xbutton)))
        }
        x if x == WM_MOUSEWHEEL => {
            let delta = (info.mouseData as i32 >> 16) as f64 / 120.0;
            Some(MouseEventType::Scroll { dx: 0.0, dy: delta })
        }
        x if x == WM_MOUSEHWHEEL => {
            let delta = (info.mouseData as i32 >> 16) as f64 / 120.0;
            Some(MouseEventType::Scroll { dx: delta, dy: 0.0 })
        }
        _ => None,
    };

    if let Some(evt) = event_type {
        let mouse_event = MouseEvent::now(dx, dy, evt);
        HOOK_SENDER.with(|s| {
            if let Some(sender) = s.borrow().as_ref() {
                let _ = sender.try_send(CapturedInput::Mouse(mouse_event));
            }
        });
    }

    let should_suppress = HOOK_SUPPRESS.with(|s| {
        s.borrow()
            .as_ref()
            .map(|b| b.load(Ordering::Relaxed))
            .unwrap_or(false)
    });

    if should_suppress {
        // Recenter the real cursor so subsequent events carry a usable delta
        // even when the user pushes toward a screen edge. Without this, the
        // OS clamps info.pt at the edge and dx/dy collapses to zero on that
        // axis — leaving the remote cursor stuck.
        let (sw, sh) = SCREEN_SIZE.with(|s| s.get());
        if sw > 0 && sh > 0 {
            let cx = sw / 2;
            let cy = sh / 2;
            let _ = SetCursorPos(cx, cy);
            LAST_POS.with(|p| p.set(Some((cx, cy))));
        }
        LRESULT(1) // Suppress the event
    } else {
        CallNextHookEx(None, code, wparam, lparam)
    }
}

unsafe extern "system" fn kbd_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
    let w = wparam.0 as u32;
    let down = w == WM_KEYDOWN || w == WM_SYSKEYDOWN;
    let up = w == WM_KEYUP || w == WM_SYSKEYUP;

    if down || up {
        // Encode the extended-key bit in the low bit of `flags`. The Windows
        // simulator on the other side uses this to set KEYEVENTF_EXTENDEDKEY
        // so arrow keys, right-Ctrl/Alt, Insert, Delete, etc. simulate
        // correctly. Note: `info.flags` is a KBDLLHOOKSTRUCT_FLAGS newtype,
        // so we access the inner u32 via `.0`.
        let extended = (info.flags.0 & LLKHF_EXTENDED) != 0;
        let out_flags: u64 = if extended { 1 } else { 0 };
        log::info!(
            "key captured: vk={} down={} extended={}",
            info.vkCode, down, extended
        );
        let key_event = KeyEvent {
            keycode: info.vkCode,
            down,
            flags: out_flags,
        };
        HOOK_SENDER.with(|s| {
            if let Some(sender) = s.borrow().as_ref() {
                let _ = sender.try_send(CapturedInput::Key(key_event));
            }
        });
    }

    let should_suppress = HOOK_SUPPRESS.with(|s| {
        s.borrow()
            .as_ref()
            .map(|b| b.load(Ordering::Relaxed))
            .unwrap_or(false)
    });

    if should_suppress {
        LRESULT(1) // Swallow the event from local apps
    } else {
        CallNextHookEx(None, code, wparam, lparam)
    }
}
