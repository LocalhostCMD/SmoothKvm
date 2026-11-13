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
    /// Emergency stop: release every key and mouse button you are holding down.
    ReleaseAll,
    // ---- 0.8: everything below is new; only sent to peers that announced version >= 8 ----
    /// Sent by a controller right after connecting: the port it listens on itself, so the screen can
    /// open connections back to it (to send files or windows the other way).
    Announce { port: u16, version: u16 },
    /// First message of a file-transfer session: `count` files, `total` bytes in all.
    SendFiles { count: u32, total: u64 },
    /// One file follows as exactly `size` bytes of raw frames. `path` is relative and '/'-separated.
    FileStart { path: String, size: u64 },
    /// The sender has nothing more to send.
    FilesEnd,
    /// The receiver's verdict for the whole transfer.
    FilesResult { ok: bool, detail: String },
}

/// Protocol version spoken by this build (in `Announce`).
pub const PROTO_VERSION: u16 = 8;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AnimationStyle {
    None,
    Bounce,
    Elastic,
    Slide,
    /// Goes slightly past the target and settles back.
    Overshoot,
    /// Damped spring.
    Spring,
    /// Your own cubic-bezier curve (see `Animation::curve`).
    Custom,
}

/// What is drawn on screen when you move to another screen. Purely visual: it never touches the mouse.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Visual {
    None,
    /// Colourful rings spreading out from where you crossed.
    Ripple,
    /// A glowing wave along the screen edge.
    Glow,
    /// A burst of confetti-like sparks.
    Sparks,
    /// A streaking comet with a rainbow tail along the edge.
    Comet,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Palette {
    Rainbow,
    Ocean,
    Sunset,
    Neon,
    Mono,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct Animation {
    /// The on-screen effect.
    pub visual: Visual,
    pub palette: Palette,
    /// Motion (easing) of the effect: how it speeds up, bounces or springs.
    pub style: AnimationStyle,
    pub duration_ms: u32,
    /// Size of the effect on screen, in pixels.
    pub size_px: u32,
    /// Play when you move onto another screen.
    pub on_enter: bool,
    /// Play when you come back to this PC.
    pub on_leave: bool,
    /// Control points `[x1, y1, x2, y2]` of the custom motion curve (same meaning as CSS cubic-bezier).
    pub curve: [f32; 4],
}

impl Default for Animation {
    fn default() -> Self {
        Self {
            visual: Visual::Ripple,
            palette: Palette::Rainbow,
            style: AnimationStyle::Overshoot,
            duration_ms: 700,
            size_px: 220,
            on_enter: true,
            on_leave: true,
            curve: [0.34, 1.56, 0.64, 1.0],
        }
    }
}

fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32, t: f32) -> f32 {
    let x = |s: f32| 3.0 * (1.0 - s).powi(2) * s * x1 + 3.0 * (1.0 - s) * s * s * x2 + s.powi(3);
    let y = |s: f32| 3.0 * (1.0 - s).powi(2) * s * y1 + 3.0 * (1.0 - s) * s * s * y2 + s.powi(3);
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..24 {
        let mid = (lo + hi) / 2.0;
        if x(mid) < t {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    y((lo + hi) / 2.0)
}

/// Progress 0..1 -> eased progress (may overshoot 1 for some styles). Always 0 at 0 and 1 at 1.
/// The panel preview (JavaScript) uses the same formulas.
pub fn ease(a: &Animation, t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t <= 0.0 {
        return 0.0;
    }
    if t >= 1.0 {
        return 1.0;
    }
    match a.style {
        AnimationStyle::None => 1.0,
        AnimationStyle::Slide => 1.0 - (1.0 - t).powi(3),
        AnimationStyle::Overshoot => {
            let (c1, c3) = (1.70158f32, 2.70158f32);
            1.0 + c3 * (t - 1.0).powi(3) + c1 * (t - 1.0).powi(2)
        }
        AnimationStyle::Spring => 1.0 - (-7.0 * t).exp() * (12.0 * t).cos(),
        AnimationStyle::Custom => cubic_bezier(a.curve[0], a.curve[1], a.curve[2], a.curve[3], t),
        AnimationStyle::Elastic => 2f32.powf(-10.0 * t) * ((t * 10.0 - 0.75) * (2.0 * std::f32::consts::PI / 3.0)).sin() + 1.0,
        AnimationStyle::Bounce => {
            let (n1, d1) = (7.5625f32, 2.75f32);
            if t < 1.0 / d1 {
                n1 * t * t
            } else if t < 2.0 / d1 {
                let t = t - 1.5 / d1;
                n1 * t * t + 0.75
            } else if t < 2.5 / d1 {
                let t = t - 2.25 / d1;
                n1 * t * t + 0.9375
            } else {
                let t = t - 2.625 / d1;
                n1 * t * t + 0.984375
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_style_starts_at_zero_and_ends_at_one() {
        for style in [
            AnimationStyle::None,
            AnimationStyle::Bounce,
            AnimationStyle::Elastic,
            AnimationStyle::Slide,
            AnimationStyle::Overshoot,
            AnimationStyle::Spring,
            AnimationStyle::Custom,
        ] {
            let a = Animation { style, ..Animation::default() };
            assert_eq!(ease(&a, 0.0), 0.0, "{style:?} start");
            assert_eq!(ease(&a, 1.0), 1.0, "{style:?} end");
            // the curve should be finite everywhere in between
            for i in 1..100 {
                assert!(ease(&a, i as f32 / 100.0).is_finite(), "{style:?} at {i}");
            }
        }
    }

    #[test]
    fn linear_bezier_is_identity() {
        let a = Animation { style: AnimationStyle::Custom, curve: [0.0, 0.0, 1.0, 1.0], ..Animation::default() };
        for i in 1..10 {
            let t = i as f32 / 10.0;
            assert!((ease(&a, t) - t).abs() < 0.01, "t={t}");
        }
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
    /// Let paired computers control THIS machine too (it listens on `port`). Off by default.
    pub receive: bool,
    /// Share copied TEXT with paired computers while connected (off by default; never logged).
    pub clipboard: bool,
    pub animation: Animation,
    /// Accept files that paired computers send (saved to Downloads/SmoothKvm, never opened or run).
    pub file_drop: bool,
    /// Windows: which side of this screen the "drop here to send" strip appears on.
    pub drop_edge: Edge,
    /// Look for a new SmoothKvm release about once a day (it is only installed when you click Update).
    pub update_check: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 24800,
            remotes: vec![],
            escape_hid: 0x47,
            game_guard: true,
            receive: false,
            clipboard: false,
            animation: Animation::default(),
            file_drop: true,
            drop_edge: Edge::Right,
            update_check: true,
        }
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
        if !(40..=600).contains(&a.size_px) {
            return Err("effect size must be 40-600 px".into());
        }
        let [x1, y1, x2, y2] = a.curve;
        if !(0.0..=1.0).contains(&x1) || !(0.0..=1.0).contains(&x2) || !(-2.0..=3.0).contains(&y1) || !(-2.0..=3.0).contains(&y2) {
            return Err("custom curve: x values must be 0-1 and y values between -2 and 3".into());
        }
        if self.escape_hid == 0 || self.escape_hid > 0xE7 {
            return Err("invalid escape key".into());
        }
        Ok(())
    }
}
