//! Linux: the "drop here to send" strip. While you drag something (mouse button held) against the side of the
//! screen where another computer is, a strip appears there; drop files on it and they are sent to that
//! computer, and the pointer then carries on over there.
//!
//! It is a plain X11 window speaking the XDND drag-and-drop protocol. KDE Plasma on Wayland passes drags from
//! Wayland apps (Dolphin, the desktop, ...) to X11 windows, and libX11 is part of every desktop install, so it is
//! loaded at runtime (dlopen) and SmoothKvm needs no GUI toolkit. If there is no X server it simply stays off.
#![allow(non_snake_case, non_upper_case_globals)]
use crate::store::Identity;
use crate::{shared, xfer};
use kvm_protocol::Edge;
use std::ffi::{c_char, c_int, c_long, c_uint, c_ulong, c_void, CString};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Display = c_void;
type Window = c_ulong;
type Atom = c_ulong;

#[repr(C)]
struct XSetWindowAttributes {
    background_pixmap: c_ulong,
    background_pixel: c_ulong,
    border_pixmap: c_ulong,
    border_pixel: c_ulong,
    bit_gravity: c_int,
    win_gravity: c_int,
    backing_store: c_int,
    backing_planes: c_ulong,
    backing_pixel: c_ulong,
    save_under: c_int,
    event_mask: c_long,
    do_not_propagate_mask: c_long,
    override_redirect: c_int,
    colormap: c_ulong,
    cursor: c_ulong,
}

/// XEvent is a union of 24 longs; we read the fields we need at their x86-64 offsets.
#[repr(C)]
#[derive(Clone, Copy)]
struct XEvent {
    pad: [c_long; 24],
}

impl XEvent {
    fn kind(&self) -> c_int {
        self.pad[0] as c_int
    }
    fn long(&self, i: usize) -> c_long {
        self.pad[i]
    }
}

// XClientMessageEvent: type(0) serial(1) send_event(2) display(3) window(4) message_type(5) format(6) data.l[0..5] (7..12)
const CM_WINDOW: usize = 4;
const CM_TYPE: usize = 5;
const CM_DATA: usize = 7;
// XSelectionEvent: ... requestor(4) selection(5) target(6) property(7) time(8)
const SEL_PROPERTY: usize = 7;

const Expose: c_int = 12;
const ClientMessage: c_int = 33;
const SelectionNotify: c_int = 31;
const ExposureMask: c_long = 1 << 15;
const CWOverrideRedirect: c_ulong = 1 << 9;
const CWEventMask: c_ulong = 1 << 11;
const CWBackPixel: c_ulong = 1 << 1;
const PropModeReplace: c_int = 0;
const XA_ATOM: Atom = 4;

struct X {
    _lib: *mut c_void,
    XOpenDisplay: unsafe extern "C" fn(*const c_char) -> *mut Display,
    XDefaultRootWindow: unsafe extern "C" fn(*mut Display) -> Window,
    XDefaultScreen: unsafe extern "C" fn(*mut Display) -> c_int,
    XDisplayWidth: unsafe extern "C" fn(*mut Display, c_int) -> c_int,
    XDisplayHeight: unsafe extern "C" fn(*mut Display, c_int) -> c_int,
    XCreateWindow: unsafe extern "C" fn(*mut Display, Window, c_int, c_int, c_uint, c_uint, c_uint, c_int, c_uint, *mut c_void, c_ulong, *mut XSetWindowAttributes) -> Window,
    XInternAtom: unsafe extern "C" fn(*mut Display, *const c_char, c_int) -> Atom,
    XChangeProperty: unsafe extern "C" fn(*mut Display, Window, Atom, Atom, c_int, c_int, *const u8, c_int) -> c_int,
    XMapRaised: unsafe extern "C" fn(*mut Display, Window) -> c_int,
    XUnmapWindow: unsafe extern "C" fn(*mut Display, Window) -> c_int,
    XMoveResizeWindow: unsafe extern "C" fn(*mut Display, Window, c_int, c_int, c_uint, c_uint) -> c_int,
    XPending: unsafe extern "C" fn(*mut Display) -> c_int,
    XNextEvent: unsafe extern "C" fn(*mut Display, *mut XEvent) -> c_int,
    XSendEvent: unsafe extern "C" fn(*mut Display, Window, c_int, c_long, *mut XEvent) -> c_int,
    XConvertSelection: unsafe extern "C" fn(*mut Display, Atom, Atom, Atom, Window, c_ulong) -> c_int,
    XGetWindowProperty: unsafe extern "C" fn(*mut Display, Window, Atom, c_long, c_long, c_int, Atom, *mut Atom, *mut c_int, *mut c_ulong, *mut c_ulong, *mut *mut u8) -> c_int,
    XFree: unsafe extern "C" fn(*mut c_void) -> c_int,
    XFlush: unsafe extern "C" fn(*mut Display) -> c_int,
    XConnectionNumber: unsafe extern "C" fn(*mut Display) -> c_int,
    XDefaultGC: unsafe extern "C" fn(*mut Display, c_int) -> *mut c_void,
    XSetForeground: unsafe extern "C" fn(*mut Display, *mut c_void, c_ulong) -> c_int,
    XDrawString: unsafe extern "C" fn(*mut Display, Window, *mut c_void, c_int, c_int, *const c_char, c_int) -> c_int,
    XSetWindowBackground: unsafe extern "C" fn(*mut Display, Window, c_ulong) -> c_int,
    XClearWindow: unsafe extern "C" fn(*mut Display, Window) -> c_int,
}

unsafe impl Send for X {}

macro_rules! sym {
    ($lib:expr, $name:ident) => {{
        let n = CString::new(stringify!($name)).unwrap();
        let p = libc::dlsym($lib, n.as_ptr());
        if p.is_null() {
            return None;
        }
        std::mem::transmute::<*mut c_void, _>(p)
    }};
}

impl X {
    fn load() -> Option<X> {
        unsafe {
            let name = CString::new("libX11.so.6").unwrap();
            let lib = libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if lib.is_null() {
                return None;
            }
            Some(X {
                _lib: lib,
                XOpenDisplay: sym!(lib, XOpenDisplay),
                XDefaultRootWindow: sym!(lib, XDefaultRootWindow),
                XDefaultScreen: sym!(lib, XDefaultScreen),
                XDisplayWidth: sym!(lib, XDisplayWidth),
                XDisplayHeight: sym!(lib, XDisplayHeight),
                XCreateWindow: sym!(lib, XCreateWindow),
                XInternAtom: sym!(lib, XInternAtom),
                XChangeProperty: sym!(lib, XChangeProperty),
                XMapRaised: sym!(lib, XMapRaised),
                XUnmapWindow: sym!(lib, XUnmapWindow),
                XMoveResizeWindow: sym!(lib, XMoveResizeWindow),
                XPending: sym!(lib, XPending),
                XNextEvent: sym!(lib, XNextEvent),
                XSendEvent: sym!(lib, XSendEvent),
                XConvertSelection: sym!(lib, XConvertSelection),
                XGetWindowProperty: sym!(lib, XGetWindowProperty),
                XFree: sym!(lib, XFree),
                XFlush: sym!(lib, XFlush),
                XConnectionNumber: sym!(lib, XConnectionNumber),
                XDefaultGC: sym!(lib, XDefaultGC),
                XSetForeground: sym!(lib, XSetForeground),
                XDrawString: sym!(lib, XDrawString),
                XSetWindowBackground: sym!(lib, XSetWindowBackground),
                XClearWindow: sym!(lib, XClearWindow),
            })
        }
    }
}

enum Cmd {
    Show { edge: Edge, frac: f32, name: String, idx: usize },
    Hide,
}

static TX: Mutex<Option<Sender<Cmd>>> = Mutex::new(None);

/// Show the strip on `edge` (centred near `frac`), for sending to screen `name` (index `idx`).
pub fn show(edge: Edge, frac: f32, name: &str, idx: usize) {
    if let Ok(g) = TX.lock() {
        if let Some(tx) = g.as_ref() {
            let _ = tx.send(Cmd::Show { edge, frac, name: name.to_string(), idx });
        }
    }
}

pub fn hide() {
    if let Ok(g) = TX.lock() {
        if let Some(tx) = g.as_ref() {
            let _ = tx.send(Cmd::Hide);
        }
    }
}

pub fn available() -> bool {
    TX.lock().map(|g| g.is_some()).unwrap_or(false)
}

/// Point libX11 at the desktop's X server (XWayland). A system service does not inherit the desktop session's
/// environment, so ask the user's systemd for it, or fall back to the usual places.
fn find_display() -> Option<(String, Option<String>)> {
    if let Ok(d) = std::env::var("DISPLAY") {
        if !d.is_empty() {
            return Some((d, std::env::var("XAUTHORITY").ok()));
        }
    }
    let uid = unsafe { libc::getuid() };
    let run = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{uid}"));
    let out = std::process::Command::new("systemctl")
        .args(["--user", "show-environment"])
        .env("XDG_RUNTIME_DIR", &run)
        .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={run}/bus"))
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let env = String::from_utf8_lossy(&out.stdout).to_string();
    let get = |k: &str| env.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).map(|v| v.trim_matches('\'').to_string());
    let display = get("DISPLAY")?;
    let auth = get("XAUTHORITY").or_else(|| {
        // Plasma keeps XWayland's cookie in /run/user/<uid>/xauth_XXXXXX
        std::fs::read_dir(&run).ok()?.flatten().map(|e| e.path()).find(|p| p.file_name().map(|n| n.to_string_lossy().starts_with("xauth_")).unwrap_or(false)).map(|p| p.to_string_lossy().to_string())
    });
    Some((display, auth))
}

/// Start the strip (does nothing on systems without X11/XWayland).
pub fn start(ident: Arc<Identity>) {
    std::thread::spawn(move || {
        // wait for a desktop session (the service starts at boot, before anyone logs in)
        let (x, dpy) = loop {
            if let (Some(x), Some((display, auth))) = (X::load(), find_display()) {
                if let Some(a) = &auth {
                    if std::env::var_os("XAUTHORITY").is_none() {
                        std::env::set_var("XAUTHORITY", a);
                    }
                }
                let name = CString::new(display).unwrap_or_default();
                let dpy = unsafe { (x.XOpenDisplay)(name.as_ptr()) };
                if !dpy.is_null() {
                    break (x, dpy);
                }
            }
            std::thread::sleep(Duration::from_secs(15));
        };
        let (tx, rx) = channel();
        if let Ok(mut g) = TX.lock() {
            *g = Some(tx);
        }
        run(x, dpy, rx, ident);
        if let Ok(mut g) = TX.lock() {
            *g = None;
        }
    });
}

struct Strip {
    x: X,
    dpy: *mut Display,
    win: Window,
    atoms: Atoms,
    shown: bool,
    target: Option<(String, usize, Edge, f32)>,
    hovering: bool,
    source: Window,
    hide_at: Option<Instant>,
}

#[derive(Clone, Copy)]
struct Atoms {
    aware: Atom,
    enter: Atom,
    position: Atom,
    status: Atom,
    leave: Atom,
    drop: Atom,
    finished: Atom,
    selection: Atom,
    action_copy: Atom,
    uri_list: Atom,
    prop: Atom,
}

const BLUE: c_ulong = 0x3b6df0;
const BLUE_HOT: c_ulong = 0x1f9d55;
const THICK: i32 = 90;

impl Strip {
    fn atom(x: &X, dpy: *mut Display, name: &str) -> Atom {
        let n = CString::new(name).unwrap();
        unsafe { (x.XInternAtom)(dpy, n.as_ptr(), 0) }
    }

    fn geometry(&self, edge: Edge, frac: f32) -> (i32, i32, u32, u32) {
        let (sw, sh) = unsafe {
            let s = (self.x.XDefaultScreen)(self.dpy);
            ((self.x.XDisplayWidth)(self.dpy, s), (self.x.XDisplayHeight)(self.dpy, s))
        };
        // centred where the pointer is, 40% of the edge long (but always covering the pointer)
        let along = |len: i32| -> (i32, i32) {
            let l = (len as f32 * 0.4) as i32;
            let c = (frac.clamp(0.0, 1.0) * len as f32) as i32;
            ((c - l / 2).clamp(0, (len - l).max(0)), l)
        };
        match edge {
            Edge::Left => {
                let (y, l) = along(sh);
                (0, y, THICK as u32, l as u32)
            }
            Edge::Right => {
                let (y, l) = along(sh);
                (sw - THICK, y, THICK as u32, l as u32)
            }
            Edge::Top => {
                let (x, l) = along(sw);
                (x, 0, l as u32, THICK as u32)
            }
            Edge::Bottom => {
                let (x, l) = along(sw);
                (x, sh - THICK, l as u32, THICK as u32)
            }
        }
    }

    fn draw(&self) {
        let Some((name, _, edge, _)) = &self.target else { return };
        unsafe {
            let gc = (self.x.XDefaultGC)(self.dpy, (self.x.XDefaultScreen)(self.dpy));
            (self.x.XSetWindowBackground)(self.dpy, self.win, if self.hovering { BLUE_HOT } else { BLUE });
            (self.x.XClearWindow)(self.dpy, self.win);
            (self.x.XSetForeground)(self.dpy, gc, 0xffffff);
            let vertical = matches!(edge, Edge::Left | Edge::Right);
            let lines = if vertical { vec!["Drop", "to send", "to", name.as_str()] } else { vec!["Drop to send to", name.as_str()] };
            for (i, l) in lines.iter().enumerate() {
                let s: String = l.chars().take(if vertical { 12 } else { 40 }).collect();
                let c = CString::new(s.clone()).unwrap_or_default();
                (self.x.XDrawString)(self.dpy, self.win, gc, 8, 22 + i as c_int * 16, c.as_ptr(), s.len() as c_int);
            }
            (self.x.XFlush)(self.dpy);
        }
    }

    fn show(&mut self, edge: Edge, frac: f32, name: String, idx: usize) {
        let (x, y, w, h) = self.geometry(edge, frac);
        self.target = Some((name, idx, edge, frac));
        self.hovering = false;
        self.hide_at = None;
        unsafe {
            (self.x.XMoveResizeWindow)(self.dpy, self.win, x, y, w.max(1), h.max(1));
            (self.x.XMapRaised)(self.dpy, self.win);
        }
        self.shown = true;
        self.draw();
    }

    fn hide(&mut self) {
        if self.shown {
            unsafe {
                (self.x.XUnmapWindow)(self.dpy, self.win);
                (self.x.XFlush)(self.dpy);
            }
        }
        self.shown = false;
        self.hovering = false;
        self.hide_at = None;
    }

    fn client_message(&self, to: Window, kind: Atom, data: [c_long; 5]) {
        let mut ev = XEvent { pad: [0; 24] };
        ev.pad[0] = ClientMessage as c_long;
        ev.pad[CM_WINDOW] = to as c_long;
        ev.pad[CM_TYPE] = kind as c_long;
        ev.pad[6] = 32; // format
        ev.pad[CM_DATA..CM_DATA + 5].copy_from_slice(&data);
        unsafe {
            (self.x.XSendEvent)(self.dpy, to, 0, 0, &mut ev);
            (self.x.XFlush)(self.dpy);
        }
    }

    fn read_drop(&self) -> Vec<PathBuf> {
        let (mut typ, mut fmt, mut n, mut after, mut data) = (0 as Atom, 0 as c_int, 0 as c_ulong, 0 as c_ulong, std::ptr::null_mut::<u8>());
        let ok = unsafe { (self.x.XGetWindowProperty)(self.dpy, self.win, self.atoms.prop, 0, 1 << 20, 1, 0, &mut typ, &mut fmt, &mut n, &mut after, &mut data) };
        if ok != 0 || data.is_null() {
            return vec![];
        }
        let bytes = unsafe { std::slice::from_raw_parts(data, n as usize) }.to_vec();
        unsafe { (self.x.XFree)(data as *mut c_void) };
        parse_uri_list(&String::from_utf8_lossy(&bytes))
    }
}

/// "file:///home/me/My%20File.step\r\n..." -> paths (anything that is not a local file is skipped).
pub fn parse_uri_list(s: &str) -> Vec<PathBuf> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.strip_prefix("file://"))
        .map(|rest| rest.strip_prefix("localhost").unwrap_or(rest))
        .filter(|p| p.starts_with('/'))
        .map(|p| {
            let b = p.as_bytes();
            let mut out = Vec::with_capacity(b.len());
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'%' && i + 2 < b.len() {
                    if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                }
                out.push(b[i]);
                i += 1;
            }
            use std::os::unix::ffi::OsStringExt;
            PathBuf::from(std::ffi::OsString::from_vec(out))
        })
        .collect()
}

fn run(x: X, dpy: *mut Display, rx: Receiver<Cmd>, ident: Arc<Identity>) {
    let atoms = Atoms {
        aware: Strip::atom(&x, dpy, "XdndAware"),
        enter: Strip::atom(&x, dpy, "XdndEnter"),
        position: Strip::atom(&x, dpy, "XdndPosition"),
        status: Strip::atom(&x, dpy, "XdndStatus"),
        leave: Strip::atom(&x, dpy, "XdndLeave"),
        drop: Strip::atom(&x, dpy, "XdndDrop"),
        finished: Strip::atom(&x, dpy, "XdndFinished"),
        selection: Strip::atom(&x, dpy, "XdndSelection"),
        action_copy: Strip::atom(&x, dpy, "XdndActionCopy"),
        uri_list: Strip::atom(&x, dpy, "text/uri-list"),
        prop: Strip::atom(&x, dpy, "SMOOTHKVM_DROP"),
    };
    let win = unsafe {
        let root = (x.XDefaultRootWindow)(dpy);
        let mut attr: XSetWindowAttributes = std::mem::zeroed();
        attr.override_redirect = 1; // stays where we put it, above other windows, no title bar
        attr.event_mask = ExposureMask;
        attr.background_pixel = BLUE;
        let w = (x.XCreateWindow)(dpy, root, 0, 0, THICK as c_uint, 200, 0, 0, 1 /* InputOutput */, std::ptr::null_mut(), CWOverrideRedirect | CWEventMask | CWBackPixel, &mut attr);
        let version: [u32; 1] = [5];
        (x.XChangeProperty)(dpy, w, atoms.aware, XA_ATOM, 32, PropModeReplace, version.as_ptr() as *const u8, 1);
        let title = b"SmoothKvm drop";
        let wm_name = Strip::atom(&x, dpy, "WM_NAME");
        (x.XChangeProperty)(dpy, w, wm_name, 31 /* XA_STRING */, 8, PropModeReplace, title.as_ptr(), title.len() as c_int);
        (x.XFlush)(dpy);
        w
    };
    let mut s = Strip { x, dpy, win, atoms, shown: false, target: None, hovering: false, source: 0, hide_at: None };
    shared::info("Drag-and-drop to other computers is ready: drag files against the screen edge where another computer is");
    let fd = unsafe { (s.x.XConnectionNumber)(dpy) };
    loop {
        // commands from the edge/mouse logic
        loop {
            match rx.try_recv() {
                Ok(Cmd::Show { edge, frac, name, idx }) => s.show(edge, frac, name, idx),
                Ok(Cmd::Hide) => {
                    // a drop in progress finishes first
                    if !s.hovering {
                        s.hide();
                    } else {
                        s.hide_at = Some(Instant::now() + Duration::from_secs(3));
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(_) => return,
            }
        }
        if let Some(t) = s.hide_at {
            if Instant::now() >= t {
                s.hide();
            }
        }
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        unsafe { libc::poll(&mut pfd, 1, 40) };
        while unsafe { (s.x.XPending)(dpy) } > 0 {
            let mut ev = XEvent { pad: [0; 24] };
            unsafe { (s.x.XNextEvent)(dpy, &mut ev) };
            match ev.kind() {
                Expose => s.draw(),
                ClientMessage => {
                    let kind = ev.long(CM_TYPE) as Atom;
                    let d = |i: usize| ev.long(CM_DATA + i);
                    if kind == s.atoms.enter {
                        s.source = d(0) as Window;
                        s.hovering = true;
                        s.draw();
                    } else if kind == s.atoms.position {
                        s.source = d(0) as Window;
                        // accept, and keep sending positions; action: copy
                        s.client_message(s.source, s.atoms.status, [s.win as c_long, 1 | 2, 0, 0, s.atoms.action_copy as c_long]);
                    } else if kind == s.atoms.leave {
                        s.hovering = false;
                        s.draw();
                    } else if kind == s.atoms.drop {
                        s.source = d(0) as Window;
                        let time = d(2) as c_ulong;
                        unsafe {
                            (s.x.XConvertSelection)(dpy, s.atoms.selection, s.atoms.uri_list, s.atoms.prop, s.win, time);
                            (s.x.XFlush)(dpy);
                        }
                    }
                }
                SelectionNotify => {
                    let got = ev.long(SEL_PROPERTY) as Atom != 0;
                    let paths = if got { s.read_drop() } else { vec![] };
                    s.client_message(s.source, s.atoms.finished, [s.win as c_long, if paths.is_empty() { 0 } else { 1 }, s.atoms.action_copy as c_long, 0, 0]);
                    let target = s.target.clone();
                    s.hide();
                    if let Some((name, idx, edge, frac)) = target {
                        if paths.is_empty() {
                            shared::warn("Only files and folders can be dropped there");
                        } else {
                            match xfer::send_async(Some(name.clone()), paths, ident.clone()) {
                                Ok(t) => shared::notify(&format!("Sending to {}", t.name)),
                                Err(e) => shared::notify(&format!("Could not send: {e}")),
                            }
                            // and carry on over there, like the file did
                            #[cfg(target_os = "linux")]
                            crate::capture_linux::cross_after_drop(idx, edge, frac);
                        }
                        let _ = (idx, edge, frac);
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_lists() {
        let p = parse_uri_list("file:///home/me/My%20Part.step\r\n# comment\r\nfile://localhost/tmp/a.txt\r\nhttps://example.com/x\r\n");
        assert_eq!(p, vec![PathBuf::from("/home/me/My Part.step"), PathBuf::from("/tmp/a.txt")]);
    }

    /// Plays the part of a drag source (like Dolphin) against the real strip window on a real X server:
    /// enter, position, drop, then hands over a uri-list. Skipped when there is no X display.
    #[test]
    fn real_x11_drop() {
        if std::env::var_os("DISPLAY").is_none() {
            return;
        }
        let Some(x) = X::load() else { return };
        let dpy = unsafe { (x.XOpenDisplay)(std::ptr::null()) };
        if dpy.is_null() {
            return;
        }
        // the strip, in its own thread with its own connection
        let (tx, rx) = channel();
        let dir = std::env::temp_dir().join(format!("skvm-drop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("drag me.txt");
        std::fs::write(&file, "hi").unwrap();
        std::thread::spawn(move || {
            let x2 = X::load().unwrap();
            let d2 = unsafe { (x2.XOpenDisplay)(std::ptr::null()) };
            let ident = Arc::new(Identity { private: vec![1; 32], public: vec![2; 32] });
            run(x2, d2, rx, ident);
        });
        tx.send(Cmd::Show { edge: Edge::Right, frac: 0.5, name: "Nowhere".into(), idx: 0 }).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        // find the strip window by name, act as the drag source
        let a = |n: &str| Strip::atom(&x, dpy, n);
        let out = std::process::Command::new("xwininfo").args(["-name", "SmoothKvm drop"]).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let id = text.split_whitespace().find(|w| w.starts_with("0x")).expect("strip window is mapped");
        let strip = c_ulong::from_str_radix(id.trim_start_matches("0x"), 16).unwrap();
        let src = unsafe {
            let root = (x.XDefaultRootWindow)(dpy);
            let mut attr: XSetWindowAttributes = std::mem::zeroed();
            (x.XCreateWindow)(dpy, root, 0, 0, 10, 10, 0, 0, 1, std::ptr::null_mut(), 0, &mut attr)
        };
        let send = |kind: Atom, data: [c_long; 5]| {
            let mut ev = XEvent { pad: [0; 24] };
            ev.pad[0] = ClientMessage as c_long;
            ev.pad[CM_WINDOW] = strip as c_long;
            ev.pad[CM_TYPE] = kind as c_long;
            ev.pad[6] = 32;
            ev.pad[CM_DATA..CM_DATA + 5].copy_from_slice(&data);
            unsafe {
                (x.XSendEvent)(dpy, strip, 0, 0, &mut ev);
                (x.XFlush)(dpy);
            }
        };
        // become the owner of the drag selection, offering text/uri-list
        type SetOwner = unsafe extern "C" fn(*mut Display, Atom, Window, c_ulong) -> c_int;
        let set_owner: SetOwner = unsafe { std::mem::transmute(libc::dlsym(x._lib, b"XSetSelectionOwner\0".as_ptr() as *const c_char)) };
        unsafe { set_owner(dpy, a("XdndSelection"), src, 0) };
        send(a("XdndEnter"), [src as c_long, 5 << 24, a("text/uri-list") as c_long, 0, 0]);
        send(a("XdndPosition"), [src as c_long, 0, 0, 0, a("XdndActionCopy") as c_long]);
        send(a("XdndDrop"), [src as c_long, 0, 0, 0, 0]);
        // answer the strip's request for the data, then wait for XdndFinished
        let uri = format!("file://{}\r\n", file.display().to_string().replace(' ', "%20"));
        let (mut got_status, mut finished) = (false, None);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && finished.is_none() {
            while unsafe { (x.XPending)(dpy) } > 0 {
                let mut ev = XEvent { pad: [0; 24] };
                unsafe { (x.XNextEvent)(dpy, &mut ev) };
                if ev.kind() == ClientMessage && ev.long(CM_TYPE) as Atom == a("XdndStatus") {
                    got_status = ev.long(CM_DATA + 1) & 1 == 1;
                }
                if ev.kind() == ClientMessage && ev.long(CM_TYPE) as Atom == a("XdndFinished") {
                    finished = Some(ev.long(CM_DATA + 1));
                }
                if ev.kind() == 30 {
                    // XSelectionRequestEvent: owner(4) requestor(5) selection(6) target(7) property(8) time(9)
                    let (req, sel, target, prop, time) = (ev.long(5) as Window, ev.long(6), ev.long(7), ev.long(8) as Atom, ev.long(9));
                    unsafe {
                        (x.XChangeProperty)(dpy, req, prop, target as Atom, 8, PropModeReplace, uri.as_ptr(), uri.len() as c_int);
                        let mut n = XEvent { pad: [0; 24] };
                        n.pad[0] = SelectionNotify as c_long;
                        n.pad[4] = req as c_long;
                        n.pad[5] = sel;
                        n.pad[6] = target;
                        n.pad[7] = prop as c_long;
                        n.pad[8] = time;
                        (x.XSendEvent)(dpy, req, 0, 0, &mut n);
                        (x.XFlush)(dpy);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(got_status, "the strip accepted the drag");
        assert_eq!(finished, Some(1), "the strip read the file list and finished the drop successfully");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
