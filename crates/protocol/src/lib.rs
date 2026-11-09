//! Wire messages and shared config types for the software KVM.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Edge { Left, Right, Top, Bottom }

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MouseButton { Left, Right, Middle, Back, Forward }

/// Input events sent from the machine holding the physical devices.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InputEvent {
    MouseMoveRel { dx: i32, dy: i32 },
    MouseMoveAbs { x: i32, y: i32 },
    MouseButton { button: MouseButton, pressed: bool },
    Scroll { dx: i32, dy: i32 },
    /// `code` is a platform-neutral USB HID usage code.
    Key { code: u16, pressed: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    Hello { name: String, width: u32, height: u32, version: u16 },
    /// Cursor crossed an edge; the receiver takes over at `pos` (0.0..1.0 along the edge).
    Enter { edge: Edge, pos: f32 },
    Leave,
    Input(InputEvent),
    Clipboard(String),
    Ping,
    Pong,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AnimationStyle { None, Bounce, Elastic, Slide }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Animation {
    pub style: AnimationStyle,
    pub duration_ms: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Screen {
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// Neighbour by edge, e.g. `right = "windows-pc"`.
    pub left: Option<String>,
    pub right: Option<String>,
    pub top: Option<String>,
    pub bottom: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub port: u16,
    pub edge_dwell_ms: u32,
    pub lock_hotkey: String,
    pub share_clipboard: bool,
    pub animation: Animation,
    pub screens: Vec<Screen>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 24800,
            edge_dwell_ms: 0,
            lock_hotkey: "ScrollLock".into(),
            share_clipboard: true,
            animation: Animation { style: AnimationStyle::Bounce, duration_ms: 250 },
            screens: vec![],
        }
    }
}
pub mod codec;
