use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Clone, Debug, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Other(u8),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum MouseEventType {
    Move,
    ButtonDown(MouseButton),
    ButtonUp(MouseButton),
    Scroll { dx: f64, dy: f64 },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MouseEvent {
    /// Relative movement delta
    pub dx: f64,
    pub dy: f64,
    pub event_type: MouseEventType,
    /// Monotonic timestamp in microseconds
    pub timestamp_us: u64,
}

/// A keyboard key press / release. Keycodes are platform-native:
/// CGKeyCode (u16) on macOS, VK_* on Windows. Cross-OS forwarding would
/// require a keymap translation layer — currently only same-OS usage is
/// guaranteed to work.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct KeyEvent {
    pub keycode: u32,
    pub down: bool,
    /// macOS: CGEventFlags bitfield (modifier state). Windows: unused (0).
    pub flags: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ScreenInfo {
    pub width: u32,
    pub height: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Message {
    /// Client -> Server: register with screen info
    Hello(ScreenInfo),
    /// Server -> Client: acknowledge with server screen info
    HelloAck(ScreenInfo),
    /// Server -> Client: mouse entered client screen at position
    Enter { x: f64, y: f64 },
    /// Server -> Client: mouse left client screen
    Leave,
    /// Server -> Client: mouse event
    Input(MouseEvent),
    /// Server -> Client: keyboard event (forwarded only while mouse is on client)
    KeyInput(KeyEvent),
    /// Bidirectional keepalive
    Heartbeat,
}

impl MouseEvent {
    pub fn now(dx: f64, dy: f64, event_type: MouseEventType) -> Self {
        let timestamp_us = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        Self {
            dx,
            dy,
            event_type,
            timestamp_us,
        }
    }
}

pub fn serialize(msg: &Message) -> anyhow::Result<Vec<u8>> {
    Ok(bincode::serialize(msg)?)
}

pub fn deserialize(data: &[u8]) -> anyhow::Result<Message> {
    Ok(bincode::deserialize(data)?)
}
