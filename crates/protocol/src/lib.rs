//! Wire messages and shared config types for SmoothKvm.
use serde::{Deserialize, Serialize};

pub mod secure;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// Input events sent from the machine holding the physical devices.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InputEvent {
    MouseMoveRel { dx: i32, dy: i32 },
    MouseMoveAbs { x: i32, y: i32 },
    MouseButton { button: MouseButton, pressed: bool },
    /// Positive dy = scroll up / away from the user.
    Scroll { dx: i32, dy: i32 },
    /// `code` is a USB HID usage code.
    Key { code: u16, pressed: bool },
}

/// Control of the receiver's on-demand virtual display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DisplayAction {
    Start { width: u32, height: u32 },
    Stop,
    Status,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    Hello { name: String, width: u32, height: u32, version: u16 },
    /// Cursor crossed an edge; `pos` is 0.0..1.0 along that edge.
    Enter { edge: Edge, pos: f32 },
    Leave,
    Input(InputEvent),
    Clipboard(String),
    Ping,
    Pong,
    /// First message of a session that tunnels the receiver's virtual display (VNC).
    OpenDisplay,
    /// First message of a short control session.
    DisplayCmd(DisplayAction),
    DisplayState { running: bool, detail: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AnimationStyle {
    None,
    Bounce,
    Elastic,
    Slide,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct Animation {
    pub style: AnimationStyle,
    pub duration_ms: u32,
    /// How far the cursor travels in the animation, in pixels.
    pub distance_px: u32,
    /// Animate when the cursor arrives on the remote screen.
    pub on_enter: bool,
    /// Animate when the cursor comes back to this PC.
    pub on_leave: bool,
}

impl Default for Animation {
    fn default() -> Self {
        Self { style: AnimationStyle::Bounce, duration_ms: 300, distance_px: 48, on_enter: true, on_leave: true }
    }
}

/// A computer (or virtual display) whose screen sits next to this PC.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Remote {
    pub name: String,
    /// "host:port", e.g. "100.64.0.5:24800".
    pub addr: String,
    /// Which side of this PC's screen it sits on.
    pub edge: Edge,
    pub width: u32,
    pub height: u32,
    pub enabled: bool,
}

impl Default for Remote {
    fn default() -> Self {
        Self { name: "Screen".into(), addr: String::new(), edge: Edge::Right, width: 1920, height: 1080, enabled: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Port a receiver listens on.
    pub port: u16,
    pub remotes: Vec<Remote>,
    /// HID usage code of the key that returns control to this PC (default Scroll Lock).
    pub escape_hid: u16,
    /// Never switch screens while a full-screen app or game is running.
    pub game_guard: bool,
    pub animation: Animation,
}

impl Default for Config {
    fn default() -> Self {
        Self { port: 24800, remotes: vec![], escape_hid: 0x47, game_guard: true, animation: Animation::default() }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if self.port < 1024 {
            return Err("port must be 1024-65535".into());
        }
        if self.remotes.len() > 4 {
            return Err("at most 4 screens (one per side)".into());
        }
        let mut edges = Vec::new();
        let mut names = Vec::new();
        for r in &self.remotes {
            let n = r.name.trim();
            if n.is_empty() || n.len() > 32 || n.chars().any(|c| c.is_control()) {
                return Err("each screen needs a name (1-32 characters)".into());
            }
            if names.contains(&n) {
                return Err(format!("duplicate screen name: {n}"));
            }
            names.push(n);
            if r.addr.len() > 255 || r.addr.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(format!("{n}: invalid address"));
            }
            if !(100..=20000).contains(&r.width) || !(100..=20000).contains(&r.height) {
                return Err(format!("{n}: screen size out of range"));
            }
            if r.enabled {
                if edges.contains(&r.edge) {
                    return Err("two enabled screens are on the same side".into());
                }
                edges.push(r.edge);
            }
        }
        let a = &self.animation;
        if a.duration_ms > 2000 {
            return Err("animation duration must be 0-2000 ms".into());
        }
        if !(8..=300).contains(&a.distance_px) {
            return Err("animation distance must be 8-300 px".into());
        }
        if self.escape_hid == 0 || self.escape_hid > 0xE7 {
            return Err("invalid escape key".into());
        }
        Ok(())
    }
}
