//! Windows input capture: low-level hooks, edge switching between several remote screens.
//!
//! While the cursor is on a remote screen every mouse/keyboard event is swallowed locally
//! and sent to that receiver. The local cursor is parked at the screen centre and movement
//! is measured as the offset from that point. Hook callbacks use `try_lock` so they can
//! never deadlock or stall input; they only touch memory and channels.
use crate::inject_win;
use kvm_protocol::{Animation, AnimationStyle, Config, Edge, InputEvent, Message, MouseButton};
use std::collections::HashMap;
use std::io;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::SHQueryUserNotificationState;
use windows::Win32::UI::WindowsAndMessaging::*;

struct State {
    cfg: Config,
    generation: u64,
    senders: Vec<Sender<Message>>,
    connected: Vec<bool>,
    /// Index of the remote currently holding the keyboard/mouse.
    active: Option<usize>,
    vx: i32,
    vy: i32,
    cx: i32,
    cy: i32,
    /// virtual desktop bounds: left, top, right, bottom
    bounds: (i32, i32, i32, i32),
    held: Vec<(u16, bool)>,
    buttons_down: u32,
    guard_cache: (Option<Instant>, bool),
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

impl State {
    fn send(&self, m: Message) {
        if let Some(tx) = self.active.and_then(|i| self.senders.get(i)) {
            let _ = tx.send(m);
        }
    }
    fn input(&self, ev: InputEvent) {
        self.send(Message::Input(ev));
    }
}

#[derive(Clone)]
pub struct Controller;

impl Controller {
    /// Install a new set of remotes (called whenever the config changes).
    pub fn set_remotes(&self, generation: u64, cfg: Config, senders: Vec<Sender<Message>>) {
        if let Ok(mut g) = STATE.lock() {
            if let Some(st) = g.as_mut() {
                if st.active.is_some() {
                    leave_remote(st);
                }
                st.generation = generation;
                st.connected = vec![false; senders.len()];
                st.senders = senders;
                st.cfg = cfg;
            }
        }
    }

    pub fn set_connected(&self, generation: u64, idx: usize, connected: bool) {
        if let Ok(mut g) = STATE.lock() {
            if let Some(st) = g.as_mut() {
                if st.generation != generation || idx >= st.connected.len() {
                    return;
                }
                st.connected[idx] = connected;
                if !connected && st.active == Some(idx) {
                    leave_remote(st);
                }
            }
        }
    }
}

/// True while a full-screen app or game is running (Windows' "do not disturb" detection).
pub fn game_active_now() -> bool {
    unsafe { SHQueryUserNotificationState().map(|s| matches!(s.0, 2 | 3 | 4)).unwrap_or(false) }
}

fn screen_state(cfg: Config) -> State {
    unsafe {
        let l = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let t = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let r = l + GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let b = t + GetSystemMetrics(SM_CYVIRTUALSCREEN);
        State {
            cfg,
            generation: 0,
            senders: Vec::new(),
            connected: Vec::new(),
            active: None,
            vx: 0,
            vy: 0,
            cx: GetSystemMetrics(SM_CXSCREEN) / 2,
            cy: GetSystemMetrics(SM_CYSCREEN) / 2,
            bounds: (l, t, r, b),
            held: Vec::new(),
            buttons_down: 0,
            guard_cache: (None, false),
        }
    }
}

pub fn start(cfg: Config) -> io::Result<Controller> {
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

fn edge_hit(b: (i32, i32, i32, i32), edge: Edge, p: POINT) -> bool {
    let (l, t, r, bot) = b;
    match edge {
        Edge::Right => p.x >= r - 1,
        Edge::Left => p.x <= l,
        Edge::Top => p.y <= t,
        Edge::Bottom => p.y >= bot - 1,
    }
}

/// Which connected remote (if any) sits on the edge the cursor just touched.
fn find_target(st: &State, p: POINT) -> Option<usize> {
    st.cfg
        .remotes
        .iter()
        .enumerate()
        .find(|(i, r)| r.enabled && st.connected.get(*i) == Some(&true) && edge_hit(st.bounds, r.edge, p))
        .map(|(i, _)| i)
}

fn game_blocks(st: &mut State) -> bool {
    if !st.cfg.game_guard {
        return false;
    }
    if let (Some(t), v) = st.guard_cache {
        if t.elapsed() < Duration::from_millis(400) {
            return v;
        }
    }
    let v = game_active_now();
    st.guard_cache = (Some(Instant::now()), v);
    v
}

/// Direction "into" the remote screen from the edge the cursor entered through.
fn inward(edge: Edge) -> (i32, i32) {
    match edge {
        Edge::Right => (1, 0),
        Edge::Left => (-1, 0),
        Edge::Top => (0, -1),
        Edge::Bottom => (0, 1),
    }
}

fn enter_remote(st: &mut State, idx: usize, p: POINT) {
    let r = st.cfg.remotes[idx].clone();
    let (l, t, rr, b) = st.bounds;
    let (rw, rh) = (r.width as i32, r.height as i32);
    let fy = ((p.y - t) as f32 / (b - t).max(1) as f32).clamp(0.0, 1.0);
    let fx = ((p.x - l) as f32 / (rr - l).max(1) as f32).clamp(0.0, 1.0);
    // (position along the edge, start of virtual cursor, "snap to corner" move, follow-up move)
    let (frac, vx, vy, snap, follow) = match r.edge {
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
    let anim = st.cfg.animation;
    let animate_in = anim.on_enter && anim.style != AnimationStyle::None && anim.duration_ms > 0;
    let dist = if animate_in { anim.distance_px as i32 } else { 0 };
    let dir = inward(r.edge);
    st.active = Some(idx);
    st.vx = (vx + dir.0 * dist).clamp(0, rw - 1);
    st.vy = (vy + dir.1 * dist).clamp(0, rh - 1);
    unsafe {
        let _ = SetCursorPos(st.cx, st.cy);
    }
    st.send(Message::Enter { edge: r.edge, pos: frac });
    st.input(InputEvent::MouseMoveRel { dx: snap.0, dy: snap.1 });
    st.input(InputEvent::MouseMoveRel { dx: follow.0, dy: follow.1 });
    if animate_in {
        animate_remote(st.senders[idx].clone(), anim, dir);
    }
}

fn leave_remote(st: &mut State) {
    let Some(idx) = st.active else { return };
    let Some(r) = st.cfg.remotes.get(idx).cloned() else {
        st.active = None;
        return;
    };
    let (l, t, rr, b) = st.bounds;
    let (rw, rh) = (r.width.max(1) as f32, r.height.max(1) as f32);
    let fy = (st.vy as f32 / rh).clamp(0.0, 1.0);
    let fx = (st.vx as f32 / rw).clamp(0.0, 1.0);
    let (x, y) = match r.edge {
        Edge::Right => (rr - 2, t + (fy * (b - t) as f32) as i32),
        Edge::Left => (l + 1, t + (fy * (b - t) as f32) as i32),
        Edge::Top => (l + (fx * (rr - l) as f32) as i32, t + 1),
        Edge::Bottom => (l + (fx * (rr - l) as f32) as i32, b - 2),
    };
    st.send(Message::Leave);
    st.active = None;
    unsafe {
        let _ = SetCursorPos(x, y);
    }
    if st.cfg.animation.on_leave {
        animate_local(x, y, r.edge, st.cfg.animation);
    }
}

fn move_remote(st: &mut State, dx: i32, dy: i32) {
    if dx == 0 && dy == 0 {
        return;
    }
    let Some(r) = st.active.and_then(|i| st.cfg.remotes.get(i)).cloned() else { return };
    st.input(InputEvent::MouseMoveRel { dx, dy });
    st.vx += dx;
    st.vy += dy;
    let (rw, rh) = (r.width as i32, r.height as i32);
    let back = match r.edge {
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

// ---- animations ---------------------------------------------------------------

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
fn animate_local(x: i32, y: i32, edge: Edge, a: Animation) {
    if a.style == AnimationStyle::None || a.duration_ms == 0 {
        return;
    }
    std::thread::spawn(move || {
        let start = Instant::now();
        loop {
            let t = (start.elapsed().as_millis() as f32 / a.duration_ms as f32).min(1.0);
            let d = (a.distance_px as f32 * ease(a.style, t)) as i32;
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

/// Same easing on the remote screen: the cursor is nudged in from the edge it arrived on.
fn animate_remote(tx: Sender<Message>, a: Animation, dir: (i32, i32)) {
    std::thread::spawn(move || {
        let start = Instant::now();
        let mut prev = 0i32;
        loop {
            let t = (start.elapsed().as_millis() as f32 / a.duration_ms as f32).min(1.0);
            let d = (a.distance_px as f32 * ease(a.style, t)).round() as i32;
            let step = d - prev;
            prev = d;
            if step != 0 {
                let ev = InputEvent::MouseMoveRel { dx: dir.0 * step, dy: dir.1 * step };
                if tx.send(Message::Input(ev)).is_err() {
                    break;
                }
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
        if st.active.is_some() {
            st.input(InputEvent::MouseButton { button, pressed });
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
            if st.active.is_some() {
                let (dx, dy) = (info.pt.x - st.cx, info.pt.y - st.cy);
                move_remote(st, dx, dy);
                true
            } else if st.buttons_down == 0 {
                match find_target(st, info.pt) {
                    // A full-screen app/game is running: stay on this PC.
                    Some(_) if game_blocks(st) => false,
                    Some(i) => {
                        enter_remote(st, i, info.pt);
                        true
                    }
                    None => false,
                }
            } else {
                false
            }
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            if st.active.is_some() {
                let notches = (info.mouseData >> 16) as i16 as i32 / 120;
                if msg == WM_MOUSEWHEEL {
                    st.input(InputEvent::Scroll { dx: 0, dy: notches });
                } else {
                    st.input(InputEvent::Scroll { dx: notches, dy: 0 });
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
    if st.active.is_some() {
        if let Some(hid) = scan_to_hid(scan, ext) {
            if hid == st.cfg.escape_hid {
                if down {
                    leave_remote(st);
                }
                return true;
            }
            st.input(InputEvent::Key { code: hid, pressed: down });
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
