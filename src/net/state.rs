//! Shared state between the networking/input backend and the UI.
//!
//! The UI and tray read the same session status. Watch signals wake pending
//! network operations when the user stops a session or disables retries.

use super::devices::Features;
use crate::config::Edge;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Idle,
    Listening,
    Connecting,
    Connected,
    Reconnecting,
    Stopping,
    Failed,
}

/// Observable state for a running Server or Client session. Shared across
/// backend threads via `Arc<SharedState>`. All fields are safe to read/write
/// from any thread.
pub struct SharedState {
    status: Mutex<SessionStatus>,
    stop: watch::Sender<bool>,
    auto_reconnect: watch::Sender<bool>,
    pub invitation: Mutex<Option<zeroize::Zeroizing<String>>>,

    /// Remote peer address (server address on client, client address on
    /// server). `None` until the first handshake completes.
    pub peer_addr: Mutex<Option<String>>,

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
            status: Mutex::new(SessionStatus::default()),
            stop: watch::channel(false).0,
            auto_reconnect: watch::channel(true).0,
            invitation: Mutex::new(None),
            peer_addr: Mutex::new(None),
            last_event_ms: AtomicU64::new(0),
            last_heartbeat_ms: AtomicU64::new(0),
            started_ms: AtomicU64::new(0),
            last_error: Mutex::new(None),
        }
    }

    pub fn status(&self) -> SessionStatus {
        *self.status.lock().unwrap()
    }

    pub(crate) fn phase(&self, phase: Phase) {
        let mut status = self.status.lock().unwrap();
        if status.phase != Phase::Stopping || matches!(phase, Phase::Idle | Phase::Failed) {
            status.phase = phase;
        }
    }

    pub fn dismiss_error(&self) {
        self.clear_error();
        if self.status().phase == Phase::Failed {
            self.phase(Phase::Idle);
        }
    }

    pub fn fail(&self, error: impl Into<String>) {
        self.set_error(error);
        self.forwarding(false);
        self.phase(Phase::Failed);
    }

    pub(crate) fn forwarding(&self, value: bool) {
        self.status.lock().unwrap().forwarding = value;
    }

    pub(crate) fn set_features(&self, features: Features) {
        self.status.lock().unwrap().features = features;
    }

    pub(crate) fn set_peer_details(&self, keyboard_supported: bool, edge: Option<Edge>) {
        let mut status = self.status.lock().unwrap();
        status.keyboard_supported = keyboard_supported;
        status.edge = edge;
    }

    pub fn request_stop(&self) {
        self.stop.send_replace(true);
        let mut status = self.status.lock().unwrap();
        if !matches!(status.phase, Phase::Idle | Phase::Failed) {
            status.phase = Phase::Stopping;
        }
    }

    pub(crate) fn stop_signal(&self) -> watch::Receiver<bool> {
        self.stop.subscribe()
    }

    pub fn set_auto_reconnect(&self, enabled: bool) {
        self.auto_reconnect.send_replace(enabled);
    }

    pub(crate) fn reconnect_signal(&self) -> watch::Receiver<bool> {
        self.auto_reconnect.subscribe()
    }

    pub(crate) fn mark_ready(&self) {
        self.started_ms.store(now_ms(), Ordering::Release);
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

#[derive(Clone, Copy)]
pub struct SessionStatus {
    pub phase: Phase,
    pub features: Features,
    pub forwarding: bool,
    pub keyboard_supported: bool,
    pub edge: Option<Edge>,
}

impl Default for SessionStatus {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            features: Features::default(),
            forwarding: false,
            keyboard_supported: true,
            edge: None,
        }
    }
}
