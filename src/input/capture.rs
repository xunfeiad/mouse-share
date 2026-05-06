use crate::protocol::{KeyEvent, MouseEvent};
use anyhow::Result;
use crossbeam_channel::Sender;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// Saved cursor position captured by `hide_local_cursor` so
/// `show_local_cursor` can restore the cursor to where it physically was
/// before the hide. We warp the cursor to a fixed point on the main
/// display during the hide (to force every compositor to redraw and
/// actually honor the refcounted hide — see the multi-display note on
/// `hide_local_cursor`), which would otherwise leave the cursor at that
/// fixed point after unhide. Module-level state is fine: hide_local_cursor
/// / show_local_cursor are always called in balanced pairs from a single
/// owner (either the server or the client event loop).
#[cfg(target_os = "macos")]
static CURSOR_RESTORE_POS: Mutex<Option<(f64, f64)>> = Mutex::new(None);

#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
static CURSOR_RESTORE_POS: Mutex<Option<(f64, f64)>> = Mutex::new(None);

/// Unified captured input — mouse or keyboard. Both share the same channel
/// so the server event loop can apply the "only forward while mouse is on
/// client" rule uniformly to both streams.
///
/// The mouse variant carries the absolute cursor position observed by the
/// capture layer at event time. The server's edge-detection branch uses
/// this directly instead of calling `get_cursor_position()` per event,
/// which on macOS is an IPC to the window server — at 1 kHz event rates
/// that was a measurable waste.
#[derive(Debug, Clone)]
pub enum CapturedInput {
    Mouse {
        event: MouseEvent,
        abs_x: f64,
        abs_y: f64,
    },
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

/// Hide the local system cursor. Used by the server when forwarding input
/// to a remote client, so the user sees only the remote cursor.
///
/// History — why this function no longer freezes the cursor:
///
/// An earlier revision also called
/// `CGAssociateMouseAndMouseCursorPosition(false)` here to decouple the
/// cursor from HID events entirely — the Synergy / Barrier pattern. That
/// does stop cursor drift regardless of tap state, but it creates a
/// catastrophic failure mode: if anything goes wrong while the
/// association is off (forwarding state machine stalls, tap gets
/// disabled, the process crashes), the cursor stays **frozen system-wide**
/// and the user can only recover by force-quitting the process with
/// Cmd+Option+Esc, or in the worst case logging out. During a stall the
/// UI itself becomes unreachable (user can't click the Stop button with
/// a frozen cursor), turning any minor bug into a full deadlock.
///
/// We now rely solely on the HID-level filtering event tap in
/// `macos_capture.rs` to swallow mouse events while forwarding. The
/// tradeoff: if the tap ever goes passive (permission issue, auto-disable
/// window), the local cursor will briefly track the physical mouse with
/// the visual hidden — visually ugly but the user can still move and
/// click to self-rescue. This is strictly safer than deadlocking the
/// whole session.
///
/// Caller is responsible for balancing hide/show calls.
///
/// Multi-display note: on real multi-monitor setups, `CGDisplayHideCursor`
/// is refcounted *globally* but its visual effect is per-compositor —
/// each display runs an independent rendering pipeline that only redraws
/// when something invalidates its cached frame. Our CGEventTap
/// suppression swallows the HID events that would normally be that
/// invalidation, so a display the cursor is *not* currently on keeps
/// showing its last-drawn frame forever — and a later compositor wake
/// (a click, a window repaint) will re-draw the cursor *at its cached
/// position*, defeating the hide.
///
/// Fix, in order:
///
/// 1. **Record** the current cursor position so `show_local_cursor` can
///    restore it.
/// 2. **Warp** the cursor to a fixed point on the main display. This
///    does two things in one shot:
///    * the compositor the cursor *left* redraws without a cursor
///      (stale frame cleared), and
///    * the main compositor redraws with the cursor at the new position
///      — which then processes the hide below.
/// 3. **Hide** on every active display. Iterating isn't strictly needed
///    for correctness after the warp (the global refcount is what the
///    compositor reads), but calling per-display is a belt-and-braces
///    trigger for each compositor's redraw and balances symmetrically
///    with `show_local_cursor`.
pub fn hide_local_cursor() {
    #[cfg(target_os = "macos")]
    {
        use core_graphics::display::CGDisplay;
        use core_graphics::geometry::CGPoint;

        // (1) Remember where the cursor physically was so we can put it
        // back on show. Failure is non-fatal — we just won't restore.
        if let Ok(pos) = platform_get_cursor_position() {
            if let Ok(mut guard) = CURSOR_RESTORE_POS.lock() {
                *guard = Some(pos);
            }
        }

        // (2) Warp to (1,1) on main. If the cursor was on a secondary
        // display this forces the secondary compositor to redraw (the
        // cursor left it) — which is exactly what clears the stale
        // cursor frame on that display. If the cursor was already on
        // main, this is a short hop that wakes the main compositor
        // before the hide lands.
        if let Err(e) = CGDisplay::warp_mouse_cursor_position(CGPoint::new(1.0, 1.0)) {
            log::warn!("pre-hide warp failed: {:?}", e);
        }

        // (3) Hide on every active display. Each call increments the
        // global refcount and also nudges the per-display compositor.
        let displays = CGDisplay::active_displays().unwrap_or_default();
        if displays.is_empty() {
            // Fallback: can't enumerate → at least hide on the main
            // display so the primary screen is still covered.
            if let Err(e) = CGDisplay::main().hide_cursor() {
                log::warn!("hide_cursor failed: {:?}", e);
            }
            return;
        }
        for id in displays {
            if let Err(e) = CGDisplay::new(id).hide_cursor() {
                log::warn!("hide_cursor(display={}) failed: {:?}", id, e);
            }
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
///
/// Must be called exactly the same number of times per display as
/// `hide_local_cursor` — we iterate every active display symmetrically so
/// the per-display hide refcounts stay balanced. Display hotplug between
/// hide and show is best-effort; `restore_cursor_state_on_startup` exists
/// as the crash-recovery path.
///
/// Also defensively calls `CGAssociateMouseAndMouseCursorPosition(true)`.
/// Normal operation never disassociates any more (see `hide_local_cursor`),
/// but earlier builds did — so if the user upgrades after a crash that
/// left the system-wide association disabled, calling this on the first
/// show_cursor after startup will unstick it instead of requiring a
/// logout.
pub fn show_local_cursor() {
    show_local_cursor_inner(true);
}

/// Show the cursor without restoring to the previously saved position.
/// Used by the client Enter path where we're about to warp to the entry
/// point — restoring to the stale position from the last hide would
/// override the warp.
pub fn show_local_cursor_no_restore() {
    show_local_cursor_inner(false);
}

fn show_local_cursor_inner(restore_position: bool) {
    #[cfg(target_os = "macos")]
    {
        use core_graphics::display::CGDisplay;
        use core_graphics::geometry::CGPoint;

        let displays = CGDisplay::active_displays().unwrap_or_default();
        if displays.is_empty() {
            if let Err(e) = CGDisplay::main().show_cursor() {
                log::warn!("show_cursor failed: {:?}", e);
            }
        } else {
            for id in displays {
                if let Err(e) = CGDisplay::new(id).show_cursor() {
                    log::warn!("show_cursor(display={}) failed: {:?}", id, e);
                }
            }
        }

        // Restore the cursor to where it physically was before the
        // matching `hide_local_cursor` call — otherwise the user sees
        // the cursor jump to (1,1) when control returns to the server,
        // which is jarring. Skipped when `restore_position` is false
        // (client Enter path — the caller will warp to the entry point).
        let saved = CURSOR_RESTORE_POS.lock().ok().and_then(|mut g| g.take());
        if restore_position {
            if let Some((x, y)) = saved {
                if let Err(e) = CGDisplay::warp_mouse_cursor_position(CGPoint::new(x, y)) {
                    log::warn!("post-show restore warp failed: {:?}", e);
                }
            }
        }

        // Defensive: restore association in case a previous run left it
        // disabled. Idempotent when already enabled.
        if let Err(e) = CGDisplay::associate_mouse_and_mouse_cursor_position(true) {
            log::warn!("CGAssociateMouseAndMouseCursorPosition(true) failed: {:?}", e);
        }
    }
}

/// Unstick any stale cursor-association or cursor-hide state left over
/// from a previous run that crashed or was force-quit. Safe to call
/// repeatedly. Called at process startup before the UI spins up.
pub fn restore_cursor_state_on_startup() {
    #[cfg(target_os = "macos")]
    {
        use core_graphics::display::CGDisplay;
        // An old build of mouse-share could have exited while the global
        // cursor-mouse association was disabled, leaving the user with a
        // frozen cursor. Force it back to enabled here.
        let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(true);
        // Balance at most one stale hide per active display. Previous
        // builds only hid the main display, newer builds hide every
        // display — a crash under either will be covered by iterating
        // here. Calls past refcount=0 are benign no-ops.
        let displays = CGDisplay::active_displays().unwrap_or_default();
        if displays.is_empty() {
            let _ = CGDisplay::main().show_cursor();
        } else {
            for id in displays {
                let _ = CGDisplay::new(id).show_cursor();
            }
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
