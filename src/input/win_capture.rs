use crate::protocol::{KeyEvent, MouseButton, MouseEvent};
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::capture::{CaptureQueue, CapturedInput, InputCapture, InputRouting, Suppression};

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetSystemMetrics, PeekMessageW, SetCursorPos, SetWindowsHookExW,
    UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, PM_REMOVE, SM_CXSCREEN, SM_CYSCREEN,
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
    suppressing: Arc<Suppression>,
}

impl WinCapture {
    pub fn new() -> Self {
        Self {
            suppressing: Arc::new(Suppression::default()),
        }
    }
}

// Thread-local storage for the hook callback context
thread_local! {
    static HOOK_SENDER: std::cell::RefCell<Option<CaptureQueue>> = std::cell::RefCell::new(None);
    static HOOK_SUPPRESS: std::cell::RefCell<Option<Arc<Suppression>>> = std::cell::RefCell::new(None);
    // None until we see the first mouse event, then tracks the last observed
    // cursor position for delta computation. Lazy-init avoids a huge spurious
    // dx/dy spike on the first event.
    static LAST_POS: std::cell::Cell<Option<(i32, i32)>> = std::cell::Cell::new(None);
    // Cached primary screen dimensions, used to recenter the cursor while
    // suppressing (so info.pt never clamps at an edge and kills the delta).
    static SCREEN_SIZE: std::cell::Cell<(i32, i32)> = std::cell::Cell::new((0, 0));
    static ROUTING: std::cell::RefCell<InputRouting> = Default::default();
}

impl InputCapture for WinCapture {
    fn run(&mut self, sender: CaptureQueue, shutdown: Arc<AtomicBool>) -> Result<()> {
        // Store sender and suppress flag in thread-local storage
        HOOK_SENDER.with(|s| *s.borrow_mut() = Some(sender));
        HOOK_SUPPRESS.with(|s| *s.borrow_mut() = Some(self.suppressing.clone()));

        // Cache primary screen dimensions for cursor recentering.
        let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        SCREEN_SIZE.with(|s| s.set((sw, sh)));

        let mouse_hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), None, 0)? };
        let kbd_hook =
            match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(kbd_hook_proc), None, 0) } {
                Ok(hook) => hook,
                Err(e) => {
                    let _ = unsafe { UnhookWindowsHookEx(mouse_hook) };
                    return Err(e.into());
                }
            };

        self.suppressing.ready.store(true, Ordering::Release);
        log::info!("Windows mouse + keyboard hooks installed, entering message loop");

        // Non-blocking message pump: drain pending messages, then sleep
        // briefly so we can notice `shutdown` and exit cleanly. Using
        // GetMessageW blocks forever and leaves no way for the UI to stop
        // the backend.
        let mut msg = MSG::default();
        loop {
            if shutdown.load(Ordering::Acquire) || self.suppressing.failed.load(Ordering::Acquire) {
                log::info!("Windows hook thread: shutdown requested, exiting pump");
                break;
            }
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    // Hooks fire as a side effect of message delivery — no
                    // explicit dispatch is needed for WH_*_LL.
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let mouse_result = unsafe { UnhookWindowsHookEx(mouse_hook) };
        let keyboard_result = unsafe { UnhookWindowsHookEx(kbd_hook) };
        // Clean up thread-locals so a later session on the same thread
        // doesn't see stale refs.
        HOOK_SENDER.with(|s| *s.borrow_mut() = None);
        HOOK_SUPPRESS.with(|s| *s.borrow_mut() = None);
        LAST_POS.with(|p| p.set(None));
        mouse_result?;
        keyboard_result?;
        Ok(())
    }

    fn suppress_handle(&self) -> Arc<Suppression> {
        self.suppressing.clone()
    }
}

unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
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
    let mouse_event = match w {
        x if x == WM_MOUSEMOVE => Some(MouseEvent::Move { dx, dy }),
        x if x == WM_LBUTTONDOWN => Some(MouseEvent::ButtonDown(MouseButton::Left)),
        x if x == WM_LBUTTONUP => Some(MouseEvent::ButtonUp(MouseButton::Left)),
        x if x == WM_RBUTTONDOWN => Some(MouseEvent::ButtonDown(MouseButton::Right)),
        x if x == WM_RBUTTONUP => Some(MouseEvent::ButtonUp(MouseButton::Right)),
        x if x == WM_MBUTTONDOWN => Some(MouseEvent::ButtonDown(MouseButton::Middle)),
        x if x == WM_MBUTTONUP => Some(MouseEvent::ButtonUp(MouseButton::Middle)),
        x if x == WM_XBUTTONDOWN => {
            let xbutton = ((info.mouseData >> 16) & 0xFFFF) as u8;
            Some(MouseEvent::ButtonDown(MouseButton::Other(xbutton)))
        }
        x if x == WM_XBUTTONUP => {
            let xbutton = ((info.mouseData >> 16) & 0xFFFF) as u8;
            Some(MouseEvent::ButtonUp(MouseButton::Other(xbutton)))
        }
        x if x == WM_MOUSEWHEEL => {
            let delta = (info.mouseData as i32 >> 16) as f64 / 120.0;
            Some(MouseEvent::Scroll { dx: 0.0, dy: delta })
        }
        x if x == WM_MOUSEHWHEEL => {
            let delta = (info.mouseData as i32 >> 16) as f64 / 120.0;
            Some(MouseEvent::Scroll { dx: delta, dy: 0.0 })
        }
        _ => None,
    };

    let should_suppress = mouse_event.is_some_and(|event| {
        HOOK_SUPPRESS.with(|state| {
            let state = state.borrow();
            let Some(state) = state.as_ref() else {
                return false;
            };
            let (swallow, send) = ROUTING.with(|routing| routing.borrow_mut().mouse(&event, state));
            if send {
                HOOK_SENDER.with(|sender| {
                    if let Some(sender) = sender.borrow().as_ref() {
                        sender.send(CapturedInput::Mouse {
                            event,
                            abs_x: info.pt.x as f64,
                            abs_y: info.pt.y as f64,
                        });
                    }
                });
            }
            swallow
        })
    });

    if should_suppress {
        // Recenter the real cursor so subsequent events carry a usable delta
        // even when the user pushes toward a screen edge. Without this, the
        // OS clamps info.pt at the edge and dx/dy collapses to zero on that
        // axis — leaving the remote cursor stuck.
        let (sw, sh) = SCREEN_SIZE.with(|s| s.get());
        if w == WM_MOUSEMOVE && sw > 0 && sh > 0 {
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

unsafe extern "system" fn kbd_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
    if (info.flags.0 & 0x10) != 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let w = wparam.0 as u32;
    let down = w == WM_KEYDOWN || w == WM_SYSKEYDOWN;
    let up = w == WM_KEYUP || w == WM_SYSKEYUP;
    let should_suppress = (down || up)
        && HOOK_SUPPRESS.with(|state| {
            let state = state.borrow();
            let Some(state) = state.as_ref() else {
                return false;
            };
            let (swallow, send) =
                ROUTING.with(|routing| routing.borrow_mut().key(info.vkCode, down, state));
            if send {
                let key_event = KeyEvent {
                    keycode: info.vkCode,
                    down,
                    flags: u64::from((info.flags.0 & LLKHF_EXTENDED) != 0),
                };
                HOOK_SENDER.with(|sender| {
                    if let Some(sender) = sender.borrow().as_ref() {
                        sender.send(CapturedInput::Key(key_event));
                    }
                });
            }
            swallow
        });

    if should_suppress {
        LRESULT(1) // Swallow the event from local apps
    } else {
        CallNextHookEx(None, code, wparam, lparam)
    }
}
