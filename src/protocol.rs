use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, Copy, PartialEq, Eq, PartialOrd, Ord)]
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

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct ScreenInfo {
    pub width: u32,
    pub height: u32,
}

impl ScreenInfo {
    pub fn valid(self) -> bool {
        (1..=65_536).contains(&self.width) && (1..=65_536).contains(&self.height)
    }

    pub fn clamp(self, (x, y): Point) -> Point {
        (
            x.clamp(0.0, f64::from(self.width - 1)),
            y.clamp(0.0, f64::from(self.height - 1)),
        )
    }
}

use crate::net::devices::{DeviceId, Features, Token};
pub type Point = (f64, f64);

#[derive(Serialize, Deserialize)]
pub enum Login {
    Pair {
        id: DeviceId,
        name: String,
        invite: Token,
    },
    Connect {
        id: DeviceId,
        token: Token,
        screen: ScreenInfo,
        mac: bool,
        #[serde(default)]
        limits: Features,
    },
}

#[derive(Serialize, Deserialize)]
pub enum LoginReply {
    Paired(Token),
    Saved,
    Ready(ReadyConfig),
    Rejected(String),
}

#[derive(Serialize, Deserialize)]
pub struct ReadyConfig {
    #[serde(flatten)]
    pub features: Features,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge: Option<crate::config::Edge>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyboard_supported: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct Position {
    pub epoch: u64,
    pub seq: u64,
    pub control: u64,
    pub point: Point,
}

#[derive(Serialize, Deserialize)]
pub enum Control {
    Ping(u64),
    Pong(u64),
    Enter(Position),
    EnterAck(u64),
    Leave(u64),
    Key(Position, KeyEvent),
    Mouse(Position, MouseEvent),
    Snapshot(Position, Vec<KeyEvent>, Vec<MouseButton>),
    Configure(Features),
    Configured(Features),
}
