//! Linux: carrying a drag across the screen edge. While you drag something (mouse button held) against the
//! side of the screen where another computer is, an invisible catcher appears along that edge. A file drag that
//! reaches it is carried straight over (the pointer goes on to the other computer with the files in hand);
//! if that is not possible, letting go on the edge still sends the files over. Nothing is ever shown.
//!
//! It is a plain X11 window speaking the XDND drag-and-drop protocol. KDE Plasma on Wayland passes drags from
//! Wayland apps (Dolphin, the desktop, ...) to X11 windows, and libX11 is part of every desktop install, so it is
//! loaded at runtime (dlopen) and SmoothKvm needs no GUI toolkit. If there is no X server it simply stays off.
#![allow(non_snake_case, non_upper_case_globals, unused_imports)]
use crate::store::Identity;
use crate::x11::{self, Atom, Display, Window, XEvent, XSetWindowAttributes, X};
use crate::{shared, xfer};
use kvm_protocol::Edge;
use std::ffi::{c_char, c_int, c_long, c_uint, c_ulong, c_void, CString};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// XClientMessageEvent: type(0) serial(1) send_event(2) display(3) window(4) message_type(5) format(6) data.l[0..5] (7..12)
const CM_WINDOW: usize = 4;
const CM_TYPE: usize = 5;
const CM_DATA: usize = 7;
// XSelectionEvent: ... requestor(4) selection(5) target(6) property(7) time(8)
const SEL_PROPERTY: usize = 7;
use x11::{ClientMessage, CWBackPixel, CWEventMask, CWOverrideRedirect, Expose, ExposureMask, PropModeReplace, SelectionNotify, XA_ATOM};

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

/// Start the strip (does nothing on systems without X11/XWayland).
pub fn start(ident: Arc<Identity>) {
    std::thread::spawn(move || {
        // wait for a desktop session (the service starts at boot, before anyone logs in)
        let (x, dpy) = loop {
            if let Some(x) = X::load() {
                if let Some(dpy) = x11::open(&x) {
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
    /// When the current drag came over the strip (it is carried across after resting there briefly).
    hover_since: Option<Instant>,
    /// 0 = not carried, 1 = asked for the file list to carry it, 2 = carried over (the drop here just ends it)
    carry: u8,
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

/// The catcher hugs the very edge (the pointer sits on the last pixels while you push against it).
const THICK: i32 = 6;

impl Strip {
    fn atom(x: &X, dpy: *mut Display, name: &str) -> Atom {
        x.atom(dpy, name)
    }

    fn geometry(&self, edge: Edge, frac: f32) -> (i32, i32, u32, u32) {
        let (sw, sh) = unsafe {
            let s = (self.x.XDefaultScreen)(self.dpy);
            ((self.x.XDisplayWidth)(self.dpy, s), (self.x.XDisplayHeight)(self.dpy, s))
        };
        // the whole edge (the corners never switch, but the catcher is harmless there)
        let along = |len: i32| -> (i32, i32) {
            let _ = frac;
            (0, len)
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

    /// Invisible: nothing is drawn (kept so the event handling stays the same).
    fn draw(&self) {}

    fn show(&mut self, edge: Edge, frac: f32, name: String, idx: usize) {
        let (x, y, w, h) = self.geometry(edge, frac);
        self.target = Some((name, idx, edge, frac));
        self.hovering = false;
        self.hover_since = None;
        self.carry = 0;
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
        self.hover_since = None;
        self.carry = 0;
        self.hide_at = None;
    }

    fn client_message(&self, to: Window, kind: Atom, data: [c_long; 5]) {
        let mut ev = XEvent::zeroed();
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
        attr.background_pixel = 0;
        attr.border_pixel = 0;
        // fully see-through (32-bit visual, all pixels transparent) so it is never visible
        let mut vi: x11::XVisualInfo = std::mem::zeroed();
        let w = if (x.XMatchVisualInfo)(dpy, (x.XDefaultScreen)(dpy), 32, x11::TrueColor, &mut vi) != 0 {
            attr.colormap = (x.XCreateColormap)(dpy, root, vi.visual, 0);
            (x.XCreateWindow)(dpy, root, 0, 0, THICK as c_uint, 200, 0, 32, 1, vi.visual, CWOverrideRedirect | CWEventMask | CWBackPixel | x11::CWBorderPixel | x11::CWColormap, &mut attr)
        } else {
            // no compositor: a 1-pixel line is the least visible it can be
            (x.XCreateWindow)(dpy, root, 0, 0, 1, 200, 0, 0, 1 /* InputOutput */, std::ptr::null_mut(), CWOverrideRedirect | CWEventMask | CWBackPixel, &mut attr)
        };
        let version: [u32; 1] = [5];
        (x.XChangeProperty)(dpy, w, atoms.aware, XA_ATOM, 32, PropModeReplace, version.as_ptr() as *const u8, 1);
        let title = b"SmoothKvm drop";
        let wm_name = Strip::atom(&x, dpy, "WM_NAME");
        (x.XChangeProperty)(dpy, w, wm_name, 31 /* XA_STRING */, 8, PropModeReplace, title.as_ptr(), title.len() as c_int);
        (x.XFlush)(dpy);
        w
    };
    let mut s = Strip { x, dpy, win, atoms, shown: false, target: None, hovering: false, hover_since: None, carry: 0, source: 0, hide_at: None };
    shared::info("Drag-and-drop to other computers is ready: drag files across the screen edge where another computer is");
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
        if s.shown && s.hovering && s.carry == 0 {
            // like a second monitor: the drag goes straight across, no pause at the edge
            if s.hover_since.is_some() {
                s.carry = 1;
                unsafe {
                    (s.x.XConvertSelection)(dpy, s.atoms.selection, s.atoms.uri_list, s.atoms.prop, s.win, 0);
                    (s.x.XFlush)(dpy);
                }
            }
        }
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        unsafe { libc::poll(&mut pfd, 1, 40) };
        while unsafe { (s.x.XPending)(dpy) } > 0 {
            let mut ev = XEvent::zeroed();
            unsafe { (s.x.XNextEvent)(dpy, &mut ev) };
            match ev.kind() {
                Expose => s.draw(),
                ClientMessage => {
                    let kind = ev.long(CM_TYPE) as Atom;
                    let d = |i: usize| ev.long(CM_DATA + i);
                    if kind == s.atoms.enter {
                        s.source = d(0) as Window;
                        s.hovering = true;
                        s.hover_since = Some(Instant::now());
                        s.draw();
                    } else if kind == s.atoms.position {
                        s.source = d(0) as Window;
                        // accept, and keep sending positions; action: copy
                        s.client_message(s.source, s.atoms.status, [s.win as c_long, 1 | 2, 0, 0, s.atoms.action_copy as c_long]);
                    } else if kind == s.atoms.leave {
                        s.hovering = false;
                        s.hover_since = None;
                        s.draw();
                    } else if kind == s.atoms.drop {
                        s.source = d(0) as Window;
                        if s.carry == 2 {
                            // the drag went on to the other computer; this is only the drag ending here
                            s.client_message(s.source, s.atoms.finished, [s.win as c_long, 1, s.atoms.action_copy as c_long, 0, 0]);
                            s.hide();
                            continue;
                        }
                        let time = d(2) as c_ulong;
                        unsafe {
                            (s.x.XConvertSelection)(dpy, s.atoms.selection, s.atoms.uri_list, s.atoms.prop, s.win, time);
                            (s.x.XFlush)(dpy);
                        }
                    }
                }
                SelectionNotify if s.carry == 1 => {
                    // the file list of a drag resting on the strip: carry it over to the other computer
                    let got = ev.long(SEL_PROPERTY) as Atom != 0;
                    let paths = if got { s.read_drop() } else { vec![] };
                    s.carry = 3; // tried (a plain drop on the strip still works if this does not)
                    if let (false, Some((name, idx, _edge, frac))) = (paths.is_empty(), s.target.clone()) {
                        let mut b = [0u8; 4];
                        let _ = getrandom::getrandom(&mut b);
                        let drag = u32::from_le_bytes(b);
                        let names: Vec<String> = paths.iter().filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).collect();
                        if crate::capture_linux::cross_with_drag(idx, frac, names, drag) {
                            match xfer::send_async_as(Some(name.clone()), paths, ident.clone(), Some(drag)) {
                                Ok(_) => s.carry = 2,
                                Err(e) => shared::notify(&format!("Could not carry the files over: {e}")),
                            }
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
        tx.send(Cmd::Show { edge: Edge::Right, frac: 0.5, name: "Nowhere".into(), idx: 99 }).unwrap();
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
            let mut ev = XEvent::zeroed();
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
        unsafe { (x.XSetSelectionOwner)(dpy, a("XdndSelection"), src, 0) };
        send(a("XdndEnter"), [src as c_long, 5 << 24, a("text/uri-list") as c_long, 0, 0]);
        send(a("XdndPosition"), [src as c_long, 0, 0, 0, a("XdndActionCopy") as c_long]);
        send(a("XdndDrop"), [src as c_long, 0, 0, 0, 0]);
        // answer the strip's request for the data, then wait for XdndFinished
        let uri = format!("file://{}\r\n", file.display().to_string().replace(' ', "%20"));
        let (mut got_status, mut finished) = (false, None);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && finished.is_none() {
            while unsafe { (x.XPending)(dpy) } > 0 {
                let mut ev = XEvent::zeroed();
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
                        let mut n = XEvent::zeroed();
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
