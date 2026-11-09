//! Windows input capture: low-level hooks, edge switching, forwarding to the remote.
//!
//! While the cursor is "remote" every mouse/keyboard event is swallowed locally and
//! sent to the receiver. The local cursor is parked at the screen centre and movement
//! is measured as the offset from that point. Hook callbacks use `try_lock` so they can
//! never deadlock or stall input; they only touch memory and a channel.
use crate::inject_win;
use kvm_protocol::{Animation, AnimationStyle, Config, Edge, InputEvent, Message, MouseButton};
use std::collections::HashMap;
use std::io;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

struct State {
    cfg: Config,
    remote: bool,
    connected: bool,
    vx: i32,
    vy: i32,
    cx: i32,
    cy: i32,
    /// virtual desktop bounds: left, top, right, bottom
    bounds: (i32, i32, i32, i32),
    held: Vec<(u16, bool)>,
    buttons_down: u32,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static TX: OnceLock<Sender<Message>> = OnceLock::new();

fn send(m: Message) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(m);
    }
}

fn input(ev: InputEvent) {
    send(Message::Input(ev));
}

pub struct Controller;

impl Controller {
    pub fn set_connected(&self, c: bool) {
        if let Ok(mut g) = STATE.lock() {
            if let Some(st) = g.as_mut() {
                st.connected = c;
            }
        }
    }

    /// Give control back to this PC (used on disconnect).
    pub fn force_local(&self) {
        if let Ok(mut g) = STATE.lock() {
            if let Some(st) = g.as_mut() {
                if st.remote {
                    leave_remote(st);
                }
            }
        }
    }

    pub fn update_config(&self, cfg: Config) {
        if let Ok(mut g) = STATE.lock() {
            if let Some(st) = g.as_mut() {
                st.cfg = cfg;
            }
        }
    }
}

fn screen_state(cfg: Config) -> State {
    unsafe {
        let l = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let t = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let r = l + GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let b = t + GetSystemMetrics(SM_CYVIRTUALSCREEN);
        State {
            cfg,
            remote: false,
            connected: false,
            vx: 0,
            vy: 0,
            cx: GetSystemMetrics(SM_CXSCREEN) / 2,
            cy: GetSystemMetrics(SM_CYSCREEN) / 2,
            bounds: (l, t, r, b),
            held: Vec::new(),
            buttons_down: 0,
        }
    }
}

pub fn start(cfg: Config, tx: Sender<Message>) -> io::Result<Controller> {
    let _ = TX.set(tx);
    *STATE.lock().unwrap() = Some(screen_state(cfg));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::spawn(move || unsafe {
        let hooks = (|| -> windows::core::Result<()> {
            let hmod = HINSTANCE(GetModuleHandleW(None)?.0);
            SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hmod, 0)?;
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), hmod, 0)?;
            Ok(())
        })();
        let _ = ready_tx.send(hooks.map_err(|e| e.to_string()));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Controller),
        Ok(Err(e)) => Err(io::Error::new(io::ErrorKind::Other, e)),
        Err(e) => Err(io::Error::new(io::ErrorKind::Other, e.to_string())),
    }
}

// ---- switching logic -------------------------------------------------------

fn at_edge(st: &State, p: POINT) -> bool {
    let (l, t, r, b) = st.bounds;
    match st.cfg.remote_edge {
        Edge::Right => p.x >= r - 1,
        Edge::Left => p.x <= l,
        Edge::Top => p.y <= t,
        Edge::Bottom => p.y >= b - 1,
    }
}

fn enter_remote(st: &mut State, p: POINT) {
    let (l, t, r, b) = st.bounds;
    let (rw, rh) = (st.cfg.remote_width as i32, st.cfg.remote_height as i32);
    let fy = ((p.y - t) as f32 / (b - t).max(1) as f32).clamp(0.0, 1.0);
    let fx = ((p.x - l) as f32 / (r - l).max(1) as f32).clamp(0.0, 1.0);
    let edge = st.cfg.remote_edge;
    // (position along the edge, start of virtual cursor, "snap to corner" move, follow-up move)
    let (frac, vx, vy, snap, follow) = match edge {
        Edge::Right => (fy, 0, (fy * rh as f32) as i32, (-20000, -20000), (0, (fy * rh as f32) as i32)),
        Edge::Left => (fy, rw - 1, (fy * rh as f32) as i32, (20000, -20000), (0, (fy * rh as f32) as i32)),
        Edge::Top => (fx, (fx * rw as f32) as i32, rh - 1, (-20000, 20000), ((fx * rw as f32) as i32, 0)),
        Edge::Bottom => (fx, (fx * rw as f32) as i32, 0, (-20000, -20000), ((fx * rw as f32) as i32, 0)),
    };
    // Don't leave modifier/other keys stuck down locally.
    let held: Vec<_> = st.held.drain(..).collect();
    if !held.is_empty() {
        std::thread::spawn(move || {
            for (scan, ext) in held {
                inject_win::key(scan, ext, false);
            }
        });
    }
    st.remote = true;
    st.vx = vx;
    st.vy = vy;
    unsafe {
        let _ = SetCursorPos(st.cx, st.cy);
    }
    send(Message::Enter { edge, pos: frac });
    input(InputEvent::MouseMoveRel { dx: snap.0, dy: snap.1 });
    input(InputEvent::MouseMoveRel { dx: follow.0, dy: follow.1 });
}

fn leave_remote(st: &mut State) {
    let (l, t, r, b) = st.bounds;
    let (rw, rh) = (st.cfg.remote_width.max(1) as f32, st.cfg.remote_height.max(1) as f32);
    let fy = (st.vy as f32 / rh).clamp(0.0, 1.0);
    let fx = (st.vx as f32 / rw).clamp(0.0, 1.0);
    let edge = st.cfg.remote_edge;
    let (x, y) = match edge {
        Edge::Right => (r - 2, t + (fy * (b - t) as f32) as i32),
        Edge::Left => (l + 1, t + (fy * (b - t) as f32) as i32),
        Edge::Top => (l + (fx * (r - l) as f32) as i32, t + 1),
        Edge::Bottom => (l + (fx * (r - l) as f32) as i32, b - 2),
    };
    send(Message::Leave);
    st.remote = false;
    unsafe {
        let _ = SetCursorPos(x, y);
    }
    animate(x, y, edge, st.cfg.animation);
}

fn move_remote(st: &mut State, dx: i32, dy: i32) {
    if dx == 0 && dy == 0 {
        return;
    }
    input(InputEvent::MouseMoveRel { dx, dy });
    st.vx += dx;
    st.vy += dy;
    let (rw, rh) = (st.cfg.remote_width as i32, st.cfg.remote_height as i32);
    let back = match st.cfg.remote_edge {
        Edge::Right => st.vx < 0,
        Edge::Left => st.vx >= rw,
        Edge::Top => st.vy >= rh,
        Edge::Bottom => st.vy < 0,
    };
    if back {
        leave_remote(st);
    } else {
        st.vx = st.vx.clamp(0, rw - 1);
        st.vy = st.vy.clamp(0, rh - 1);
    }
}

// ---- bounce animation -------------------------------------------------------

fn ease(style: AnimationStyle, t: f32) -> f32 {
    match style {
        AnimationStyle::None => 1.0,
        AnimationStyle::Slide => 1.0 - (1.0 - t).powi(3),
        AnimationStyle::Elastic => {
            if t <= 0.0 {
                0.0
            } else if t >= 1.0 {
                1.0
            } else {
                2f32.powf(-10.0 * t) * ((t * 10.0 - 0.75) * (2.0 * std::f32::consts::PI / 3.0)).sin() + 1.0
            }
        }
        AnimationStyle::Bounce => {
            let (n1, d1) = (7.5625, 2.75);
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

/// The cursor "drops" away from the edge it re-entered on, with the chosen easing.
fn animate(x: i32, y: i32, edge: Edge, a: Animation) {
    if a.style == AnimationStyle::None || a.duration_ms == 0 {
        return;
    }
    std::thread::spawn(move || {
        let amp = 40.0f32;
        let start = Instant::now();
        loop {
            let t = (start.elapsed().as_millis() as f32 / a.duration_ms as f32).min(1.0);
            let d = (amp * ease(a.style, t)) as i32;
            let (px, py) = match edge {
                Edge::Right => (x - d, y),
                Edge::Left => (x + d, y),
                Edge::Top => (x, y + d),
                Edge::Bottom => (x, y - d),
            };
            unsafe {
                let _ = SetCursorPos(px, py);
            }
            if t >= 1.0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(8));
        }
    });
}

// ---- hooks ------------------------------------------------------------------

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        // Skip events injected by software (including our own SetCursorPos/SendInput).
        if info.flags & 1 == 0 && handle_mouse(wparam.0 as u32, info) {
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

fn handle_mouse(msg: u32, info: &MSLLHOOKSTRUCT) -> bool {
    let mut guard = match STATE.try_lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    let st = match guard.as_mut() {
        Some(s) => s,
        None => return false,
    };
    let button = match msg {
        WM_LBUTTONDOWN => Some((MouseButton::Left, true)),
        WM_LBUTTONUP => Some((MouseButton::Left, false)),
        WM_RBUTTONDOWN => Some((MouseButton::Right, true)),
        WM_RBUTTONUP => Some((MouseButton::Right, false)),
        WM_MBUTTONDOWN => Some((MouseButton::Middle, true)),
        WM_MBUTTONUP => Some((MouseButton::Middle, false)),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let b = if (info.mouseData >> 16) == 1 { MouseButton::Back } else { MouseButton::Forward };
            Some((b, msg == WM_XBUTTONDOWN))
        }
        _ => None,
    };
    if let Some((button, pressed)) = button {
        if st.remote {
            input(InputEvent::MouseButton { button, pressed });
            return true;
        }
        if pressed {
            st.buttons_down += 1;
        } else {
            st.buttons_down = st.buttons_down.saturating_sub(1);
        }
        return false;
    }
    match msg {
        WM_MOUSEMOVE => {
            if st.remote {
                let (dx, dy) = (info.pt.x - st.cx, info.pt.y - st.cy);
                move_remote(st, dx, dy);
                true
            } else if st.connected && st.buttons_down == 0 && at_edge(st, info.pt) {
                enter_remote(st, info.pt);
                true
            } else {
                false
            }
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            if st.remote {
                let notches = (info.mouseData >> 16) as i16 as i32 / 120;
                if msg == WM_MOUSEWHEEL {
                    input(InputEvent::Scroll { dx: 0, dy: notches });
                } else {
                    input(InputEvent::Scroll { dx: notches, dy: 0 });
                }
                true
            } else {
                false
            }
        }
        _ => false,
    }
}

fn scan_to_hid(scan: u16, ext: bool) -> Option<u16> {
    static MAP: OnceLock<HashMap<(u16, bool), u16>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut m = HashMap::new();
        for hid in 0u16..=0xE7 {
            if let Some(k) = inject_win::hid_to_scan(hid) {
                m.entry(k).or_insert(hid);
            }
        }
        m
    })
    .get(&(scan, ext))
    .copied()
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        if info.flags.0 & 0x10 == 0 && handle_key(wparam.0 as u32, info) {
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

fn handle_key(msg: u32, info: &KBDLLHOOKSTRUCT) -> bool {
    let mut guard = match STATE.try_lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    let st = match guard.as_mut() {
        Some(s) => s,
        None => return false,
    };
    let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    let ext = info.flags.0 & 0x01 != 0;
    let scan = info.scanCode as u16;
    if st.remote {
        if let Some(hid) = scan_to_hid(scan, ext) {
            if hid == st.cfg.escape_hid {
                if down {
                    leave_remote(st);
                }
                return true;
            }
            input(InputEvent::Key { code: hid, pressed: down });
        }
        return true;
    }
    if down {
        if !st.held.contains(&(scan, ext)) {
            st.held.push((scan, ext));
        }
    } else {
        st.held.retain(|k| *k != (scan, ext));
    }
    false
}
