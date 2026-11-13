use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, Copy, PartialEq, Eq, Hash)]
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

// The legacy Message is retained only to identify old peers. It is never used
// to establish a v2 session or to inject input.
pub const VERSION: u16 = 2;
pub const MAGIC: [u8; 4] = *b"MSH2";
pub type SessionId = [u8; 16];

pub fn new_id() -> SessionId {
    let mut id = [0; 16];
    getrandom::fill(&mut id).expect("OS random source unavailable");
    id
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    MacOS,
    Windows,
}
pub fn platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::MacOS
    } else {
        Platform::Windows
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Features {
    pub keyboard: bool,
    pub clipboard: bool,
}
impl Default for Features {
    fn default() -> Self {
        Self {
            keyboard: true,
            clipboard: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub struct Position {
    pub x: f64,
    pub y: f64,
    pub motion: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Control {
    Hello {
        version: u16,
        device: SessionId,
        platform: Platform,
        screen: ScreenInfo,
        udp_port: u16,
        features: Features,
    },
    Welcome {
        version: u16,
        session: SessionId,
        platform: Platform,
        screen: ScreenInfo,
        edge: crate::config::Edge,
        features: Features,
        clipboard_epoch: u64,
    },
    Reject(String),
    Ready,
    Heartbeat,
    Enter {
        generation: u64,
        position: Position,
    },
    Leave {
        generation: u64,
    },
    Input {
        generation: u64,
        position: Position,
        event: MouseEvent,
    },
    Key {
        generation: u64,
        position: Position,
        event: KeyEvent,
    },
    Settings {
        features: Features,
        clipboard_epoch: u64,
    },
    Preferences(Features),
    SettingsAck {
        features: Features,
        clipboard_epoch: u64,
    },
    ClipboardFault(String),
    Applied(u64),
    Goodbye,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ControlFrame {
    pub magic: [u8; 4],
    pub session: SessionId,
    pub sequence: u64,
    pub message: Control,
}
impl ControlFrame {
    pub fn new(session: SessionId, sequence: u64, message: Control) -> Self {
        Self {
            magic: MAGIC,
            session,
            sequence,
            message,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct Motion {
    pub magic: [u8; 4],
    pub session: SessionId,
    pub generation: u64,
    pub after_control: u64,
    pub position: Position,
}

pub fn valid_screen(screen: &ScreenInfo) -> bool {
    screen.width > 0 && screen.height > 0 && screen.width <= 65536 && screen.height <= 65536
}
