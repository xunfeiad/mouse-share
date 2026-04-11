//! Shared state between the networking/input backend and the UI.
//!
//! The backend threads (server, client, clipboard, input capture) update the
//! atomics and mutexes here; the UI reads them in its repaint loop. A single
//! `shutdown` AtomicBool is checked in every event-loop iteration to let the
//! UI gracefully stop a running session without killing the process.

use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Mutex;

/// Observable state for a running Server or Client session. Shared across
/// backend threads via `Arc<SharedState>`. All fields are safe to read/write
/// from any thread.
pub struct SharedState {
    /// UI sets this to `true` to request the backend stop. Backend loops
    /// must check it on every iteration.
    pub shutdown: AtomicBool,

    /// `true` while a peer is connected and the session is live.
    pub connected: AtomicBool,

    /// Remote peer address (server address on client, client address on
    /// server). `None` until the first handshake completes.
    pub peer_addr: Mutex<Option<String>>,

    /// `true` while the cursor has been handed off to the other side
    /// (i.e. local input is being suppressed and forwarded).
    pub mouse_on_peer: AtomicBool,

    /// Unix millis of the last event — used for watchdog / freshness checks.
    pub last_event_ms: AtomicU64,

    /// Unix millis of the last heartbeat received from the peer — UI
    /// renders "connection lost" if this falls too far behind.
    pub last_heartbeat_ms: AtomicU64,

    /// Unix millis of when the session started, for the "uptime" display.
    pub started_ms: AtomicU64,

    /// Last non-fatal error, if any. `None` means healthy. Typical values:
    /// "port in use", "server unreachable", etc. The UI displays a red
    /// banner while this is Some.
    pub last_error: Mutex<Option<String>>,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            shutdown: AtomicBool::new(false),
            connected: AtomicBool::new(false),
            peer_addr: Mutex::new(None),
            mouse_on_peer: AtomicBool::new(false),
            last_event_ms: AtomicU64::new(0),
            last_heartbeat_ms: AtomicU64::new(0),
            started_ms: AtomicU64::new(0),
            last_error: Mutex::new(None),
        }
    }

    pub fn set_error(&self, err: impl Into<String>) {
        *self.last_error.lock().unwrap() = Some(err.into());
    }

    pub fn clear_error(&self) {
        *self.last_error.lock().unwrap() = None;
    }

    pub fn set_peer(&self, addr: impl Into<String>) {
        *self.peer_addr.lock().unwrap() = Some(addr.into());
    }
}

impl Default for SharedState {
    fn default() -> Self {
        Self::new()
    }
}

/// Current time in milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
