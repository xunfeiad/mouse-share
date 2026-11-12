use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Other(u8),
}

/// A mouse event. Each variant carries exactly the data it needs:
/// - `Move` / `Scroll`: directional deltas.
/// - `ButtonDown` / `ButtonUp`: which button changed state.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum MouseEvent {
    Move { dx: f64, dy: f64 },
    ButtonDown(MouseButton),
    ButtonUp(MouseButton),
    Scroll { dx: f64, dy: f64 },
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

pub fn serialize(msg: &Message) -> anyhow::Result<Vec<u8>> {
    Ok(bincode::serialize(msg)?)
}

/// Serialize `msg` into `buf`, reusing the buffer's existing capacity.
/// Used on the server hot path to avoid allocating a fresh `Vec<u8>` per
/// forwarded mouse event. `buf` is cleared first, then written into.
pub fn serialize_into(buf: &mut Vec<u8>, msg: &Message) -> anyhow::Result<()> {
    buf.clear();
    bincode::serialize_into(&mut *buf, msg)?;
    Ok(())
}

pub fn deserialize(data: &[u8]) -> anyhow::Result<Message> {
    Ok(bincode::deserialize(data)?)
}
