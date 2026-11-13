//! Linux as the CONTROLLING computer: reads the physical keyboard and mouse from /dev/input (evdev),
//! and while a remote screen is selected it "grabs" them (so this computer stops seeing them) and forwards
//! everything over the encrypted connection.
//!
//! Switching: push the pointer off the side of the screen where another computer is (on KDE a tiny KWin
//! script tells us when the pointer touches an edge, see edge_linux.rs, because Wayland does not let normal
//! programs see the pointer), and push it back past that computer's opposite edge to return. The hotkey
//! (Scroll Lock by default) also works: next connected screen, again to cycle, finally back here.
//! Three quick presses = emergency stop.
//! Needs read access to /dev/input/event* (the package adds the service user to the `input` group).
use crate::shared;
use kvm_protocol::{Config, Edge, InputEvent, Message, MouseButton};
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const REL_HWHEEL: u16 = 6;
const REL_WHEEL: u16 = 8;
const BTN_LEFT: u16 = 0x110;
const KEY_A: u16 = 30;

struct State {
    generation: u64,
    cfg: Config,
    senders: Vec<Sender<Message>>,
    connected: Vec<bool>,
    active: Option<usize>,
    paused: bool,
    /// HID codes of keys currently held on the remote (released again on switching back).
    held: HashSet<u16>,
    buttons: HashSet<u8>,
    presses: Vec<Instant>,
    /// Where the pointer is on the active remote screen (in its pixels), to notice crossing back.
    vx: i32,
    vy: i32,
    /// The pointer has left the screen edge since we last came back (so it doesn't bounce straight over again).
    armed: bool,
    /// Mouse buttons held on THIS computer (tracked all the time, not only while controlling).
    local_buttons: HashSet<u8>,
    last_blocked_note: Option<Instant>,
    /// The drop strip is showing (hidden again when the drag ends).
    strip_shown: bool,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
/// Devices are grabbed exactly while a remote screen is active.
static WANT_GRAB: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
pub struct Controller;

fn btn_msg(b: u8) -> MouseButton {
    match b {
        0 => MouseButton::Left,
        1 => MouseButton::Right,
        2 => MouseButton::Middle,
        3 => MouseButton::Back,
        _ => MouseButton::Forward,
    }
}

fn notify(text: &str) {
    shared::info(text);
    let uid = unsafe { libc::getuid() };
    let run = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{uid}"));
    let _ = std::process::Command::new("notify-send")
        .args(["-a", "SmoothKvm", "-t", "1500", "SmoothKvm", text])
        .env("XDG_RUNTIME_DIR", &run)
        .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={run}/bus"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|mut c| {
            std::thread::spawn(move || {
                let _ = c.wait();
            });
        });
}

impl State {
    fn send(&self, m: Message) {
        if let Some(tx) = self.active.and_then(|i| self.senders.get(i)) {
            let _ = tx.send(m);
        }
    }
}

/// Give control back to this computer: release everything held on the remote first.
fn leave(st: &mut State) {
    if st.active.is_some() {
        st.send(Message::ReleaseAll);
        st.send(Message::Leave);
        st.active = None;
        st.held.clear();
        st.buttons.clear();
        WANT_GRAB.store(false, Ordering::SeqCst);
    }
}

/// Take over screen `i`, with the pointer appearing at `frac` (0..1) along the edge it borders this one.
fn enter(st: &mut State, i: usize, frac: f32) {
    let Some(r) = st.cfg.remotes.get(i).cloned() else { return };
    let (rw, rh) = (r.width.max(1) as i32, r.height.max(1) as i32);
    let frac = frac.clamp(0.0, 1.0);
    // Where it appears on the other screen, and how to get it there with relative moves only:
    // slam into a corner first (the other computer clamps it), then move along the edge.
    let (vx, vy, snap, follow) = match r.edge {
        Edge::Right => (0, (frac * rh as f32) as i32, (-20000, -20000), (0, (frac * rh as f32) as i32)),
        Edge::Left => (rw - 1, (frac * rh as f32) as i32, (20000, -20000), (0, (frac * rh as f32) as i32)),
        Edge::Top => ((frac * rw as f32) as i32, rh - 1, (-20000, 20000), ((frac * rw as f32) as i32, 0)),
        Edge::Bottom => ((frac * rw as f32) as i32, 0, (-20000, -20000), ((frac * rw as f32) as i32, 0)),
    };
    st.active = Some(i);
    st.vx = vx.clamp(0, rw - 1);
    st.vy = vy.clamp(0, rh - 1);
    st.send(Message::Enter { edge: r.edge, pos: frac });
    st.send(Message::Input(InputEvent::MouseMoveRel { dx: snap.0, dy: snap.1 }));
    st.send(Message::Input(InputEvent::MouseMoveRel { dx: follow.0, dy: follow.1 }));
    WANT_GRAB.store(true, Ordering::SeqCst);
    if st.cfg.animation.on_enter {
        crate::fx_x11::play_at_edge(st.cfg.animation, r.edge, frac);
    }
    notify(&format!("Controlling {}", r.name));
}

/// The effect for coming back from screen `active`, on the shared edge where the pointer re-appears.
fn return_effect(st: &State) {
    let Some(r) = st.active.and_then(|i| st.cfg.remotes.get(i)) else { return };
    if !st.cfg.animation.on_leave {
        return;
    }
    let frac = match r.edge {
        Edge::Left | Edge::Right => st.vy as f32 / r.height.max(1) as f32,
        Edge::Top | Edge::Bottom => st.vx as f32 / r.width.max(1) as f32,
    };
    crate::fx_x11::play_at_edge(st.cfg.animation, r.edge, frac);
}

/// The pointer touched `side` of this computer's screen (reported by KWin), `frac` along that edge.
pub fn edge_hit(side: Edge, frac: f32) {
    let Ok(mut g) = STATE.lock() else { return };
    let Some(st) = g.as_mut() else { return };
    if st.paused || st.active.is_some() || !st.armed {
        return;
    }
    let Some(i) = st.cfg.remotes.iter().enumerate().position(|(i, r)| r.enabled && r.edge == side && st.connected.get(i).copied().unwrap_or(false)) else {
        return;
    };
    if !st.local_buttons.is_empty() {
        // Dragging something against the edge: offer the drop strip (files dropped there are sent over, and
        // the pointer then follows). Crossing with the button still down would leave the drag hanging here.
        let name = st.cfg.remotes[i].name.clone();
        if crate::drop_x11::available() {
            crate::drop_x11::show(side, frac, &name, i);
            st.strip_shown = true;
        } else {
            let quiet = st.last_blocked_note.map(|t| t.elapsed() < Duration::from_secs(20)).unwrap_or(false);
            if !quiet {
                st.last_blocked_note = Some(Instant::now());
                notify("Let go of the mouse button to move to the other computer");
            }
        }
        return;
    }
    enter(st, i, frac);
}

/// Files were dropped on the strip for screen `idx`: carry on over there.
pub fn cross_after_drop(idx: usize, _side: Edge, frac: f32) {
    let Ok(mut g) = STATE.lock() else { return };
    let Some(st) = g.as_mut() else { return };
    st.strip_shown = false;
    if st.paused || st.active.is_some() || !st.connected.get(idx).copied().unwrap_or(false) {
        return;
    }
    enter(st, idx, frac);
}

/// Give the keyboard and mouse back to this computer (an app window from the other one just appeared here).
pub fn return_home() {
    if let Ok(mut g) = STATE.lock() {
        if let Some(st) = g.as_mut() {
            if st.active.is_some() {
                leave(st);
                st.armed = false;
            }
        }
    }
}

/// The pointer is no longer on any edge of this screen.
pub fn edge_away() {
    if let Ok(mut g) = STATE.lock() {
        if let Some(st) = g.as_mut() {
            st.armed = true;
        }
    }
}

/// Pointer moved while controlling another screen: follow it there, and come back when it crosses
/// back over the edge that borders this computer.
fn move_remote(st: &mut State, dx: i32, dy: i32) {
    let Some(r) = st.active.and_then(|i| st.cfg.remotes.get(i)).cloned() else { return };
    st.send(Message::Input(InputEvent::MouseMoveRel { dx, dy }));
    let (rw, rh) = (r.width.max(1) as i32, r.height.max(1) as i32);
    st.vx = st.vx.saturating_add(dx);
    st.vy = st.vy.saturating_add(dy);
    let back = match r.edge {
        Edge::Right => st.vx < 0,
        Edge::Left => st.vx >= rw,
        Edge::Top => st.vy >= rh,
        Edge::Bottom => st.vy < 0,
    };
    if back && st.buttons.is_empty() {
        return_effect(st);
        leave(st);
        st.armed = false;
        // Nudge this computer's pointer off the edge so it is clearly back (and doesn't sit on the trigger).
        let (nx, ny) = match r.edge {
            Edge::Right => (-6, 0),
            Edge::Left => (6, 0),
            Edge::Top => (0, 6),
            Edge::Bottom => (0, -6),
        };
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            crate::inject_linux::inject(&InputEvent::MouseMoveRel { dx: nx, dy: ny });
        });
        notify("Back on this computer");
    } else {
        st.vx = st.vx.clamp(0, rw - 1);
        st.vy = st.vy.clamp(0, rh - 1);
    }
}

/// The hotkey: this PC -> first connected screen -> next one ... -> back to this PC.
fn toggle(st: &mut State) {
    if st.paused {
        return;
    }
    let from = st.active.map(|a| a + 1).unwrap_or(0);
    let next = (from..st.senders.len()).find(|&i| st.connected.get(i).copied().unwrap_or(false));
    if st.active.is_some() {
        if next.is_none() {
            return_effect(st); // the hotkey brings you back here
        }
        leave(st);
    }
    match next {
        Some(i) => enter(st, i, 0.5),
        None if from == 0 => notify("No connected screen to switch to"),
        None => notify("Back on this computer"),
    }
}

fn do_emergency(st: &mut State) {
    shared::warn("EMERGENCY STOP: control returned to this computer, all keys and buttons released");
    if st.active.is_some() {
        leave(st);
    }
    st.paused = true;
    for tx in &st.senders {
        let _ = tx.send(Message::ReleaseAll);
    }
}

pub fn is_paused() -> bool {
    STATE.lock().ok().and_then(|g| g.as_ref().map(|s| s.paused)).unwrap_or(false)
}

impl Controller {
    pub fn set_remotes(&self, generation: u64, cfg: Config, senders: Vec<Sender<Message>>) {
        if let Ok(mut g) = STATE.lock() {
            if let Some(st) = g.as_mut() {
                leave(st);
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
                    leave(st);
                    notify("Connection lost: control is back on this computer");
                }
            }
        }
    }

    pub fn emergency_stop(&self) {
        if let Ok(mut g) = STATE.lock() {
            if let Some(st) = g.as_mut() {
                do_emergency(st);
            }
        }
    }

    pub fn resume(&self) {
        if let Ok(mut g) = STATE.lock() {
            if let Some(st) = g.as_mut() {
                st.paused = false;
                shared::info("Resumed after emergency stop");
            }
        }
    }
}

pub fn start(cfg: Config) -> io::Result<Controller> {
    *STATE.lock().unwrap() = Some(State {
        generation: 0,
        cfg,
        senders: vec![],
        connected: vec![],
        active: None,
        paused: false,
        held: HashSet::new(),
        buttons: HashSet::new(),
        presses: vec![],
        vx: 0,
        vy: 0,
        armed: true,
        local_buttons: HashSet::new(),
        last_blocked_note: None,
        strip_shown: false,
    });
    std::thread::spawn(scanner);
    crate::edge_linux::start();
    Ok(Controller)
}

// ---- keyboard code translation ----

pub(crate) fn linux_to_hid(code: u16) -> Option<u16> {
    static MAP: OnceLock<HashMap<u16, u16>> = OnceLock::new();
    MAP.get_or_init(|| (0u16..=0xE7).filter_map(|h| crate::inject_linux::hid_to_linux(h).map(|l| (l, h))).collect())
        .get(&code)
        .copied()
}

// ---- device discovery ----

static OPEN: Mutex<Option<HashSet<String>>> = Mutex::new(None);
static WARNED: AtomicBool = AtomicBool::new(false);

fn ioc_read(nr: u32, len: u32) -> libc::c_ulong {
    ((2u32 << 30) | (len << 16) | (0x45 << 8) | nr) as libc::c_ulong
}

fn bits(fd: i32, ev: u32, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    unsafe { libc::ioctl(fd, ioc_read(0x20 + ev, len as u32) as _, buf.as_mut_ptr()) };
    buf
}

fn has(b: &[u8], bit: u16) -> bool {
    b.get(bit as usize / 8).map(|x| x & (1 << (bit % 8)) != 0).unwrap_or(false)
}

fn dev_name(fd: i32) -> String {
    let mut buf = [0u8; 128];
    unsafe { libc::ioctl(fd, ioc_read(0x06, 128) as _, buf.as_mut_ptr()) };
    let end = buf.iter().position(|&b| b == 0).unwrap_or(0);
    String::from_utf8_lossy(&buf[..end]).to_string()
}

fn scanner() {
    loop {
        if let Ok(rd) = std::fs::read_dir("/dev/input") {
            let mut denied = false;
            let mut opened_any = OPEN.lock().ok().map(|g| g.as_ref().map(|s| !s.is_empty()).unwrap_or(false)).unwrap_or(false);
            for e in rd.flatten() {
                let path = e.path().to_string_lossy().to_string();
                if !path.contains("/event") {
                    continue;
                }
                if OPEN.lock().ok().map(|g| g.as_ref().map(|s| s.contains(&path)).unwrap_or(false)).unwrap_or(true) {
                    continue;
                }
                match OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(&path) {
                    Ok(f) => {
                        let fd = f.as_raw_fd();
                        let name = dev_name(fd);
                        let keys = bits(fd, EV_KEY as u32, 96);
                        let rels = bits(fd, EV_REL as u32, 4);
                        let keyboard = has(&keys, KEY_A);
                        let pointer = has(&rels, REL_X) && has(&rels, REL_Y);
                        // never read our own virtual device (that would loop input back)
                        let lower = name.to_lowercase();
                        if lower.contains("smoothkvm virtual") || lower.contains("softwarekvm") || !(keyboard || pointer) {
                            continue;
                        }
                        if let Ok(mut g) = OPEN.lock() {
                            g.get_or_insert_with(HashSet::new).insert(path.clone());
                        }
                        opened_any = true;
                        let p = path.clone();
                        std::thread::spawn(move || {
                            device_loop(f, &p);
                            if let Ok(mut g) = OPEN.lock() {
                                if let Some(s) = g.as_mut() {
                                    s.remove(&p);
                                }
                            }
                        });
                    }
                    Err(err) if err.kind() == io::ErrorKind::PermissionDenied => denied = true,
                    Err(_) => {}
                }
            }
            if denied && !opened_any && !WARNED.swap(true, Ordering::SeqCst) {
                shared::warn("Cannot read the keyboard/mouse (/dev/input): this user must be in the 'input' group. Controlling other computers from here is off until then.");
            }
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

// ---- per-device reader ----

fn device_loop(mut f: std::fs::File, path: &str) {
    let fd = f.as_raw_fd();
    let mut grabbed = false;
    let (mut dx, mut dy, mut wx, mut wy) = (0i32, 0i32, 0i32, 0i32);
    let size = std::mem::size_of::<libc::input_event>();
    let mut buf = vec![0u8; size * 64];
    let mut fill = 0usize;
    loop {
        // (un)grab to follow whether a remote screen is active
        let want = WANT_GRAB.load(Ordering::SeqCst);
        if want != grabbed {
            if unsafe { libc::ioctl(fd, 0x4004_4590 as _, want as libc::c_int) } == 0 {
                grabbed = want;
            } else if want {
                shared::warn(&format!("Could not take over {path}; keys from it may still reach this computer"));
                grabbed = true; // do not retry every tick
            } else {
                grabbed = false;
            }
        }
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut pfd, 1, 100) };
        if r < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return;
        }
        if r <= 0 {
            continue;
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return; // unplugged
        }
        match f.read(&mut buf[fill..]) {
            Ok(0) => return,
            Ok(n) => fill += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(_) => return,
        }
        let mut off = 0;
        while off + size <= fill {
            let ev: libc::input_event = unsafe { std::ptr::read_unaligned(buf[off..].as_ptr() as *const libc::input_event) };
            off += size;
            handle(ev.type_, ev.code, ev.value, (&mut dx, &mut dy, &mut wx, &mut wy));
        }
        buf.copy_within(off..fill, 0);
        fill -= off;
    }
}

fn handle(kind: u16, code: u16, value: i32, acc: (&mut i32, &mut i32, &mut i32, &mut i32)) {
    let (dx, dy, wx, wy) = acc;
    match kind {
        EV_REL => match code {
            REL_X => *dx += value,
            REL_Y => *dy += value,
            REL_WHEEL => *wy += value,
            REL_HWHEEL => *wx += value,
            _ => {}
        },
        EV_SYN => {
            if *dx != 0 || *dy != 0 {
                if let Ok(mut g) = STATE.lock() {
                    if let Some(st) = g.as_mut() {
                        if st.active.is_some() {
                            move_remote(st, *dx, *dy);
                        }
                    }
                }
            }
            if *wx != 0 || *wy != 0 {
                forward(InputEvent::Scroll { dx: *wx, dy: *wy });
            }
            *dx = 0;
            *dy = 0;
            *wx = 0;
            *wy = 0;
        }
        EV_KEY => {
            if value == 2 {
                return; // autorepeat: the other computer repeats held keys itself
            }
            let pressed = value == 1;
            if (BTN_LEFT..BTN_LEFT + 5).contains(&code) {
                let b = (code - BTN_LEFT) as u8;
                if let Ok(mut g) = STATE.lock() {
                    if let Some(st) = g.as_mut() {
                        if st.active.is_some() {
                            if pressed {
                                st.buttons.insert(b);
                            } else {
                                st.buttons.remove(&b);
                            }
                            st.send(Message::Input(InputEvent::MouseButton { button: btn_msg(b), pressed }));
                        } else if pressed {
                            st.local_buttons.insert(b);
                        }
                        if !pressed {
                            st.local_buttons.remove(&b);
                            if st.local_buttons.is_empty() && st.strip_shown {
                                // the drag ended (dropped on the strip, or elsewhere): the strip goes away
                                st.strip_shown = false;
                                crate::drop_x11::hide();
                            }
                        }
                    }
                }
                return;
            }
            let Some(hid) = linux_to_hid(code) else { return };
            if let Ok(mut g) = STATE.lock() {
                let Some(st) = g.as_mut() else { return };
                if hid == st.cfg.escape_hid {
                    hotkey(st, pressed);
                    return;
                }
                if st.active.is_some() {
                    if pressed {
                        st.held.insert(hid);
                    } else {
                        st.held.remove(&hid);
                    }
                    st.send(Message::Input(InputEvent::Key { code: hid, pressed }));
                }
            }
        }
        _ => {}
    }
}

fn hotkey(st: &mut State, pressed: bool) {
    let now = Instant::now();
    if pressed {
        st.presses.retain(|t| now.duration_since(*t) < Duration::from_millis(1200));
        st.presses.push(now);
        if st.presses.len() >= 3 {
            st.presses.clear();
            do_emergency(st);
        }
    } else if !st.paused {
        // switch on release so the key is not left half-pressed on either computer
        toggle(st);
    }
}

fn forward(ev: InputEvent) {
    if let Ok(mut g) = STATE.lock() {
        if let Some(st) = g.as_mut() {
            if st.active.is_some() {
                st.send(Message::Input(ev));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keycodes_round_trip() {
        // 'A' = HID 0x04 = Linux KEY_A (30); Scroll Lock = HID 0x47 = Linux 70
        assert_eq!(linux_to_hid(30), Some(0x04));
        assert_eq!(linux_to_hid(70), Some(0x47));
        assert_eq!(linux_to_hid(0x2ff), None);
    }

    fn test_state(n: usize) -> (State, Vec<std::sync::mpsc::Receiver<Message>>) {
        let mut cfg = Config::default();
        cfg.remotes = (0..n).map(|i| kvm_protocol::Remote { name: format!("s{i}"), ..Default::default() }).collect();
        let (mut txs, mut rxs) = (vec![], vec![]);
        for _ in 0..n {
            let (t, r) = std::sync::mpsc::channel();
            txs.push(t);
            rxs.push(r);
        }
        let st = State {
            generation: 1,
            cfg,
            senders: txs,
            connected: vec![true; n],
            active: None,
            paused: false,
            held: HashSet::new(),
            buttons: HashSet::new(),
            presses: vec![],
            vx: 0,
            vy: 0,
            armed: true,
            local_buttons: HashSet::new(),
            last_blocked_note: None,
            strip_shown: false,
        };
        (st, rxs)
    }

    #[test]
    fn hotkey_cycles_through_screens_then_back() {
        let (mut st, rx) = test_state(2);
        st.connected[1] = false;
        hotkey(&mut st, true);
        assert_eq!(st.active, None, "switching happens on release, not press");
        hotkey(&mut st, false);
        assert_eq!(st.active, Some(0));
        assert!(matches!(rx[0].try_recv(), Ok(Message::Enter { .. })));
        st.held.insert(4);
        hotkey(&mut st, true);
        hotkey(&mut st, false);
        assert_eq!(st.active, None, "the only connected screen -> back to this computer");
        let rest: Vec<_> = rx[0].try_iter().filter(|m| !matches!(m, Message::Input(InputEvent::MouseMoveRel { .. }))).collect();
        assert!(matches!(rest.as_slice(), [Message::ReleaseAll, Message::Leave]));
        assert!(st.held.is_empty());
    }

    #[test]
    fn three_quick_presses_is_an_emergency_stop() {
        let (mut st, rx) = test_state(1);
        hotkey(&mut st, false);
        assert_eq!(st.active, Some(0));
        for _ in 0..3 {
            hotkey(&mut st, true);
        }
        assert!(st.paused);
        assert_eq!(st.active, None);
        let all: Vec<_> = rx[0].try_iter().collect();
        assert!(all.iter().any(|m| matches!(m, Message::ReleaseAll)));
        // while stopped the hotkey does nothing
        hotkey(&mut st, false);
        assert_eq!(st.active, None);
    }

    #[test]
    fn edge_crossing_and_back() {
        let (mut st, rx) = test_state(1);
        st.cfg.remotes[0].edge = Edge::Right;
        st.cfg.remotes[0].width = 1000;
        st.cfg.remotes[0].height = 500;
        enter(&mut st, 0, 0.4);
        assert_eq!(st.active, Some(0));
        assert_eq!((st.vx, st.vy), (0, 200), "appears at the left edge of the other screen, 40% down");
        let msgs: Vec<_> = rx[0].try_iter().collect();
        assert!(matches!(msgs[0], Message::Enter { edge: Edge::Right, .. }));
        assert!(matches!(msgs[1], Message::Input(InputEvent::MouseMoveRel { dx: -20000, dy: -20000 })));
        assert!(matches!(msgs[2], Message::Input(InputEvent::MouseMoveRel { dx: 0, dy: 200 })));
        // moving around over there, including pushing against its far edge, keeps control there
        move_remote(&mut st, 300, 0);
        move_remote(&mut st, 5000, 0);
        assert_eq!(st.vx, 999);
        // holding a button over there: no return while dragging
        st.buttons.insert(0);
        move_remote(&mut st, -3000, 0);
        assert_eq!(st.active, Some(0));
        assert_eq!(st.vx, 0);
        st.buttons.clear();
        move_remote(&mut st, 5, 0);
        move_remote(&mut st, -5, 0);
        assert_eq!(st.active, Some(0), "at its left edge but not past it yet");
        move_remote(&mut st, -1, 0);
        assert_eq!(st.active, None, "crossed back over the shared edge");
        assert!(!st.armed, "must leave the edge before it can cross again");
    }

    #[test]
    fn edge_hit_only_where_a_connected_screen_is() {
        let (st, _rx) = test_state(1);
        *STATE.lock().unwrap() = Some(st);
        let active = || STATE.lock().unwrap().as_ref().unwrap().active;
        let set = |f: &dyn Fn(&mut State)| f(STATE.lock().unwrap().as_mut().unwrap());
        set(&|s| s.cfg.remotes[0].edge = Edge::Left);
        edge_hit(Edge::Right, 0.5);
        assert_eq!(active(), None, "nothing on the right");
        set(&|s| {
            s.local_buttons.insert(0);
        });
        edge_hit(Edge::Left, 0.5);
        assert_eq!(active(), None, "not while dragging something here");
        set(&|s| s.local_buttons.clear());
        set(&|s| s.connected[0] = false);
        edge_hit(Edge::Left, 0.5);
        assert_eq!(active(), None, "not to a disconnected screen");
        set(&|s| s.connected[0] = true);
        edge_hit(Edge::Left, 0.5);
        assert_eq!(active(), Some(0));
        set(&|s| {
            leave(s);
            s.armed = false;
        });
        edge_hit(Edge::Left, 0.5);
        assert_eq!(active(), None, "just came back: wait until the pointer leaves the edge");
        edge_away();
        edge_hit(Edge::Left, 0.5);
        assert_eq!(active(), Some(0));
        *STATE.lock().unwrap() = None;
        WANT_GRAB.store(false, Ordering::SeqCst);
    }

    #[test]
    fn evdev_ioctl_numbers() {
        // EVIOCGNAME(128) on Linux = 0x80804506; EVIOCGBIT(EV_KEY,96) = 0x80604521
        assert_eq!(ioc_read(0x06, 128), 0x8080_4506);
        assert_eq!(ioc_read(0x21, 96), 0x8060_4521);
    }
}
