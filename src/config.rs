use crate::protocol::ScreenInfo;

/// Which edge of the server screen triggers transition to the client
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl std::str::FromStr for Edge {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "left" => Ok(Edge::Left),
            "right" => Ok(Edge::Right),
            "top" => Ok(Edge::Top),
            "bottom" => Ok(Edge::Bottom),
            _ => Err(format!("invalid edge: {s}, expected left/right/top/bottom")),
        }
    }
}

pub struct ScreenConfig {
    pub server_screen: ScreenInfo,
    pub client_screen: Option<ScreenInfo>,
    pub edge: Edge,
}

impl ScreenConfig {
    pub fn new(server_screen: ScreenInfo, edge: Edge) -> Self {
        Self {
            server_screen,
            client_screen: None,
            edge,
        }
    }

    /// Check if the cursor at (x, y) is at the transition edge.
    /// Coordinates are absolute on the server screen.
    pub fn at_edge(&self, x: f64, y: f64) -> bool {
        let w = self.server_screen.width as f64;
        let h = self.server_screen.height as f64;
        match self.edge {
            Edge::Right => x >= w - 1.0,
            Edge::Left => x <= 0.0,
            Edge::Bottom => y >= h - 1.0,
            Edge::Top => y <= 0.0,
        }
    }

    /// Map the server edge position to a client entry position
    pub fn entry_position(&self, x: f64, y: f64) -> (f64, f64) {
        let client = self.client_screen.as_ref().unwrap_or(&self.server_screen);
        let cw = client.width as f64;
        let ch = client.height as f64;
        let sw = self.server_screen.width as f64;
        let sh = self.server_screen.height as f64;

        match self.edge {
            Edge::Right => (0.0, y / sh * ch),
            Edge::Left => (cw - 1.0, y / sh * ch),
            Edge::Bottom => (x / sw * cw, 0.0),
            Edge::Top => (x / sw * cw, ch - 1.0),
        }
    }
}
