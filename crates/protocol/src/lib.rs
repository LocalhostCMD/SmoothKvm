//! Wire messages and shared config types for the software KVM.
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
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AnimationStyle {
    None,
    Bounce,
    Elastic,
    Slide,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Animation {
    pub style: AnimationStyle,
    pub duration_ms: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Port the receiver listens on.
    pub port: u16,
    /// Sender: address of the receiver, e.g. "192.168.1.50:24800".
    pub peer_addr: String,
    /// Which side of this PC's screen the remote screen sits on.
    pub remote_edge: Edge,
    pub remote_width: u32,
    pub remote_height: u32,
    /// HID usage code of the key that returns control to this PC (default Scroll Lock).
    pub escape_hid: u16,
    pub share_clipboard: bool,
    pub animation: Animation,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 24800,
            peer_addr: String::new(),
            remote_edge: Edge::Right,
            remote_width: 1920,
            remote_height: 1080,
            escape_hid: 0x47,
            share_clipboard: true,
            animation: Animation { style: AnimationStyle::Bounce, duration_ms: 250 },
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if self.port < 1024 {
            return Err("port must be 1024-65535".into());
        }
        if !(100..=20000).contains(&self.remote_width) || !(100..=20000).contains(&self.remote_height) {
            return Err("remote screen size out of range".into());
        }
        if self.animation.duration_ms > 2000 {
            return Err("animation duration must be 0-2000 ms".into());
        }
        if self.peer_addr.len() > 255 {
            return Err("peer address too long".into());
        }
        if self.escape_hid == 0 || self.escape_hid > 0xE7 {
            return Err("invalid escape key".into());
        }
        Ok(())
    }
}
