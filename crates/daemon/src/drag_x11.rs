//! Linux: carry on a file drag that came over from the other computer, so you can drop it into any folder here.
//!
//! You drag files on the other computer to the edge that faces this one. That computer sends `DragStart` (what is
//! being dragged), hands the mouse back, and starts copying the files into a private staging folder here. This
//! starts a real drag of those files right where the pointer comes back: an invisible window under the pointer
//! receives a button press (your finger is still on the button) and becomes an ordinary X11 drag source (XDND).
//! Plasma passes X11 drags on to Wayland apps, so Dolphin, the desktop or any app that accepts files takes the
//! drop. If you let go before the copy has finished, the drop waits for it; if you let go where nothing takes
//! files, they are kept in your received-files folder.
#![allow(non_upper_case_globals)]
use crate::x11::{self, Atom, Display, Window, XEvent, XSetWindowAttributes, X};
use crate::{shared, xfer};
use kvm_protocol::Edge;
use std::collections::HashMap;
use std::ffi::{c_int, c_long, c_uint, c_ulong, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Ready state of each drag: None while its files are still arriving, Some(ok) once the transfer is over.
static DRAGS: Mutex<Option<HashMap<u32, Option<bool>>>> = Mutex::new(None);
/// You let go of the mouse button (seen on the physical mouse, see capture_linux).
static LET_GO: AtomicBool = AtomicBool::new(false);

fn root() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".cache")).join("smoothkvm-drag")
}

/// Where the files of drag `id` are put while they are being dragged.
pub fn staging(id: u32) -> PathBuf {
    root().join(format!("{id:08x}"))
}

fn state(id: u32) -> Option<Option<bool>> {
    DRAGS.lock().ok().and_then(|g| g.as_ref().and_then(|m| m.get(&id).copied()))
}

/// The files of drag `id` have all arrived (or the transfer failed).
pub fn ready(id: u32, ok: bool) {
    if let Ok(mut g) = DRAGS.lock() {
        if let Some(v) = g.get_or_insert_with(HashMap::new).get_mut(&id) {
            *v = Some(ok);
        }
    }
}

/// The physical button went up while a carried drag was in progress.
pub fn let_go() {
    LET_GO.store(true, Ordering::SeqCst);
}

/// What actually arrived (top-level items of the staging folder).
fn arrived(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| !p.to_string_lossy().ends_with(".part")).collect()).unwrap_or_default()
}

/// "file:///home/me/My%20Part.step\r\n..." for the given paths.
pub fn uri_list(paths: &[PathBuf]) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::new();
    for p in paths {
        out.push_str("file://");
        for &b in p.as_os_str().as_bytes() {
            if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out.push_str("\r\n");
    }
    out
}

/// The other computer is carrying a drag of `names` over, arriving on `edge` of this screen at `frac` along it.
/// `go` hands the mouse back to this computer and presses the button on our window; false = cancelled.
pub fn begin(names: Vec<String>, id: u32, edge: Edge, frac: f32, go: Box<dyn FnOnce() -> bool + Send>) {
    if let Ok(rd) = std::fs::read_dir(root()) {
        for e in rd.flatten() {
            let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map(|d| d > Duration::from_secs(24 * 3600)).unwrap_or(false);
            if old {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    if let Ok(mut g) = DRAGS.lock() {
        g.get_or_insert_with(HashMap::new).insert(id, None);
    }
    LET_GO.store(false, Ordering::SeqCst);
    std::thread::spawn(move || {
        let dir = staging(id);
        let _ = std::fs::create_dir_all(&dir);
        let names: Vec<String> = names.iter().filter_map(|n| xfer::safe_rel(n)).filter_map(|p| p.into_iter().next()).collect();
        let dropped = match (X::load(), names.is_empty()) {
            (Some(x), false) => match x11::open(&x) {
                Some(dpy) => run(x, dpy, id, &dir, &names, edge, frac, go),
                None => false,
            },
            _ => false,
        };
        if !dropped {
            keep_in_inbox(id, &dir);
        }
        if let Ok(mut g) = DRAGS.lock() {
            if let Some(m) = g.as_mut() {
                m.remove(&id);
            }
        }
    });
}

/// Not dropped anywhere: once the files are in, move them to the received-files folder.
fn keep_in_inbox(id: u32, dir: &Path) {
    let deadline = Instant::now() + Duration::from_secs(600);
    while state(id) == Some(None) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    if state(id) != Some(Some(true)) {
        return;
    }
    let inbox = xfer::inbox();
    let _ = std::fs::create_dir_all(&inbox);
    let mut kept = 0;
    for f in arrived(dir) {
        let name = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if std::fs::rename(&f, inbox.join(xfer::unique_name(&inbox, &name))).is_ok() {
            kept += 1;
        }
    }
    let _ = std::fs::remove_dir_all(dir);
    shared::notify(&format!("Not dropped anywhere: {kept} item(s) kept in {}", inbox.display()));
}

#[derive(Clone, Copy)]
struct Atoms {
    aware: Atom,
    proxy: Atom,
    enter: Atom,
    position: Atom,
    status: Atom,
    leave: Atom,
    drop: Atom,
    finished: Atom,
    selection: Atom,
    action_copy: Atom,
    uri_list: Atom,
    targets: Atom,
}

impl Atoms {
    fn new(x: &X, dpy: *mut Display) -> Self {
        let a = |n: &str| x.atom(dpy, n);
        Atoms {
            aware: a("XdndAware"),
            proxy: a("XdndProxy"),
            enter: a("XdndEnter"),
            position: a("XdndPosition"),
            status: a("XdndStatus"),
            leave: a("XdndLeave"),
            drop: a("XdndDrop"),
            finished: a("XdndFinished"),
            selection: a("XdndSelection"),
            action_copy: a("XdndActionCopy"),
            uri_list: a("text/uri-list"),
            targets: a("TARGETS"),
        }
    }
}

// XEvent layouts (x86-64): button/motion: time = long 7, x_root/y_root = int at byte 72/76.
// SelectionRequest: requestor 5, selection 6, target 7, property 8, time 9. ClientMessage: type 5, data 7..12.
const CM_WINDOW: usize = 4;
const CM_TYPE: usize = 5;
const CM_DATA: usize = 7;

/// The XDND source side: who we are over, what they said, and serving the file list.
struct Source<'a> {
    x: &'a X,
    dpy: *mut Display,
    win: Window,
    a: Atoms,
    /// Top-level XDND-aware window under the pointer (and where to send its messages: itself or its proxy).
    target: Option<(Window, Window)>,
    accepted: bool,
    /// A position was sent and its status not yet received (XDND sends one at a time).
    waiting: bool,
    pending: Option<(i32, i32)>,
    time: c_ulong,
    files: Vec<PathBuf>,
    finished: Option<bool>,
}

impl<'a> Source<'a> {
    fn send(&self, to: (Window, Window), kind: Atom, data: [c_long; 5]) {
        let mut ev = XEvent::zeroed();
        ev.pad[0] = x11::ClientMessage as c_long;
        ev.pad[CM_WINDOW] = to.0 as c_long;
        ev.pad[CM_TYPE] = kind as c_long;
        ev.pad[6] = 32;
        ev.pad[CM_DATA..CM_DATA + 5].copy_from_slice(&data);
        unsafe {
            (self.x.XSendEvent)(self.dpy, to.1, 0, 0, &mut ev);
            (self.x.XFlush)(self.dpy);
        }
    }

    fn prop_long(&self, w: Window, prop: Atom) -> Option<c_ulong> {
        let (mut typ, mut fmt, mut n, mut after, mut data) = (0 as Atom, 0 as c_int, 0 as c_ulong, 0 as c_ulong, std::ptr::null_mut::<u8>());
        let ok = unsafe { (self.x.XGetWindowProperty)(self.dpy, w, prop, 0, 1, 0, 0 /* AnyPropertyType */, &mut typ, &mut fmt, &mut n, &mut after, &mut data) };
        if ok != 0 || data.is_null() {
            return None;
        }
        let v = if n >= 1 && fmt == 32 { Some(unsafe { *(data as *const c_ulong) }) } else { None };
        unsafe { (self.x.XFree)(data as *mut c_void) };
        v
    }

    /// The XDND-aware window at root position (x, y), searching from the top-most window down.
    fn window_at(&self, x: i32, y: i32) -> Option<(Window, Window)> {
        let root = unsafe { (self.x.XDefaultRootWindow)(self.dpy) };
        let mut w = root;
        for _ in 0..16 {
            let (mut dx, mut dy, mut child) = (0, 0, 0 as Window);
            let ok = unsafe { (self.x.XTranslateCoordinates)(self.dpy, root, w, x, y, &mut dx, &mut dy, &mut child) };
            if ok == 0 || child == 0 || child == self.win {
                return None;
            }
            w = child;
            if self.prop_long(w, self.a.aware).is_some() {
                // a proxy (if it says so) receives the messages, which still name the real window
                let to = self.prop_long(w, self.a.proxy).filter(|p| self.prop_long(*p, self.a.proxy) == Some(*p)).unwrap_or(w);
                return Some((w, to));
            }
        }
        None
    }

    fn position(&mut self, x: i32, y: i32) {
        let t = self.window_at(x, y);
        if t != self.target {
            if let Some(old) = self.target {
                self.send(old, self.a.leave, [self.win as c_long, 0, 0, 0, 0]);
            }
            self.target = t;
            self.accepted = false;
            self.waiting = false;
            if let Some(new) = t {
                // version 5, one type: the file list
                self.send(new, self.a.enter, [self.win as c_long, 5 << 24, self.a.uri_list as c_long, 0, 0]);
            }
        }
        let Some(t) = self.target else { return };
        if self.waiting {
            self.pending = Some((x, y));
            return;
        }
        self.waiting = true;
        self.pending = None;
        let xy = ((x.clamp(0, 0xffff) as c_long) << 16) | y.clamp(0, 0xffff) as c_long;
        self.send(t, self.a.position, [self.win as c_long, 0, xy, self.time as c_long, self.a.action_copy as c_long]);
    }

    /// Answer a request for the file list (or for the list of formats offered).
    fn serve(&self, ev: &XEvent) {
        let (req, sel, target, mut prop, time) = (ev.long(5) as Window, ev.long(6), ev.long(7) as Atom, ev.long(8) as Atom, ev.long(9));
        if prop == 0 {
            prop = target; // very old clients
        }
        let ok = unsafe {
            if target == self.a.uri_list {
                let list = uri_list(&self.files);
                (self.x.XChangeProperty)(self.dpy, req, prop, self.a.uri_list, 8, x11::PropModeReplace, list.as_ptr(), list.len() as c_int);
                true
            } else if target == self.a.targets {
                let offered: [c_ulong; 2] = [self.a.targets, self.a.uri_list];
                (self.x.XChangeProperty)(self.dpy, req, prop, x11::XA_ATOM, 32, x11::PropModeReplace, offered.as_ptr() as *const u8, 2);
                true
            } else {
                false
            }
        };
        let mut n = XEvent::zeroed();
        n.pad[0] = x11::SelectionNotify as c_long;
        n.pad[4] = req as c_long;
        n.pad[5] = sel;
        n.pad[6] = target as c_long;
        n.pad[7] = if ok { prop as c_long } else { 0 };
        n.pad[8] = time;
        unsafe {
            (self.x.XSendEvent)(self.dpy, req, 0, 0, &mut n);
            (self.x.XFlush)(self.dpy);
        }
    }

    /// One X event. Returns true for the button press on our window (the drag has started).
    fn handle(&mut self, ev: &XEvent) -> bool {
        match ev.kind() {
            x11::ButtonPress => {
                self.time = ev.long(7) as c_ulong;
                return true;
            }
            x11::MotionNotify => {
                self.time = ev.long(7) as c_ulong;
                self.position(ev.int_at(72), ev.int_at(76));
            }
            x11::ClientMessage => {
                let kind = ev.long(CM_TYPE) as Atom;
                let from = ev.long(CM_DATA) as Window;
                if Some(from) != self.target.map(|t| t.0) {
                    return false;
                }
                if kind == self.a.status {
                    self.accepted = ev.long(CM_DATA + 1) & 1 == 1;
                    self.waiting = false;
                    if let Some((x, y)) = self.pending.take() {
                        self.position(x, y);
                    }
                } else if kind == self.a.finished {
                    // version 5 says whether it worked; older targets only say they are done
                    self.finished = Some(ev.long(CM_DATA + 1) & 1 == 1 || ev.long(CM_DATA + 2) == 0);
                }
            }
            x11::SelectionRequest => self.serve(ev),
            _ => {}
        }
        false
    }

    /// Let go: drop on the target if it takes the files. Returns false if there is nothing to drop on.
    fn drop_now(&mut self) -> bool {
        match (self.target, self.accepted) {
            (Some(t), true) => {
                self.send(t, self.a.drop, [self.win as c_long, 0, self.time as c_long, 0, 0]);
                true
            }
            (Some(t), false) => {
                self.send(t, self.a.leave, [self.win as c_long, 0, 0, 0, 0]);
                false
            }
            _ => false,
        }
    }

    fn pump(&mut self, wait_ms: i32) -> bool {
        let fd = unsafe { (self.x.XConnectionNumber)(self.dpy) };
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        if unsafe { (self.x.XPending)(self.dpy) } == 0 {
            unsafe { libc::poll(&mut pfd, 1, wait_ms) };
        }
        let mut pressed = false;
        while unsafe { (self.x.XPending)(self.dpy) } > 0 {
            let mut ev = XEvent::zeroed();
            unsafe { (self.x.XNextEvent)(self.dpy, &mut ev) };
            pressed |= self.handle(&ev);
        }
        pressed
    }
}

/// Where the pointer comes back onto this screen.
fn arrival(edge: Edge, frac: f32, sw: i32, sh: i32) -> (i32, i32) {
    let f = frac.clamp(0.0, 1.0);
    match edge {
        Edge::Left => (0, (f * sh as f32) as i32),
        Edge::Right => (sw - 1, (f * sh as f32) as i32),
        Edge::Top => ((f * sw as f32) as i32, 0),
        Edge::Bottom => ((f * sw as f32) as i32, sh - 1),
    }
}

/// An invisible, undecorated window under the pointer that will own the drag.
unsafe fn make_window(x: &X, dpy: *mut Display, cx: i32, cy: i32) -> Window {
    const SIZE: i32 = 96;
    let scr = (x.XDefaultScreen)(dpy);
    let (sw, sh) = ((x.XDisplayWidth)(dpy, scr), (x.XDisplayHeight)(dpy, scr));
    let (wx, wy) = ((cx - SIZE / 2).clamp(0, (sw - SIZE).max(0)), (cy - SIZE / 2).clamp(0, (sh - SIZE).max(0)));
    let root = (x.XDefaultRootWindow)(dpy);
    let mut attr: XSetWindowAttributes = std::mem::zeroed();
    attr.event_mask = x11::ButtonPressMask | x11::ButtonReleaseMask | x11::PointerMotionMask | x11::StructureNotifyMask;
    let mut vi: x11::XVisualInfo = std::mem::zeroed();
    let win = if (x.XMatchVisualInfo)(dpy, scr, 32, x11::TrueColor, &mut vi) != 0 {
        attr.colormap = (x.XCreateColormap)(dpy, root, vi.visual, 0);
        (x.XCreateWindow)(dpy, root, wx, wy, SIZE as c_uint, SIZE as c_uint, 0, 32, 1, vi.visual, x11::CWEventMask | x11::CWBackPixel | x11::CWBorderPixel | x11::CWColormap, &mut attr)
    } else {
        (x.XCreateWindow)(dpy, root, wx, wy, SIZE as c_uint, SIZE as c_uint, 0, 0, 1, std::ptr::null_mut(), x11::CWEventMask, &mut attr)
    };
    let set = |name: &str, typ: Atom, vals: &[c_ulong]| {
        let p = x.atom(dpy, name);
        (x.XChangeProperty)(dpy, win, p, typ, 32, x11::PropModeReplace, vals.as_ptr() as *const u8, vals.len() as c_int);
    };
    // no title bar or border, above everything, not in the task bar
    let motif = x.atom(dpy, "_MOTIF_WM_HINTS");
    set("_MOTIF_WM_HINTS", motif, &[2, 0, 0, 0, 0]);
    set("_NET_WM_STATE", x11::XA_ATOM, &[x.atom(dpy, "_NET_WM_STATE_ABOVE"), x.atom(dpy, "_NET_WM_STATE_SKIP_TASKBAR"), x.atom(dpy, "_NET_WM_STATE_SKIP_PAGER")]);
    // put exactly here (USPosition | USSize | PPosition | PSize)
    let mut hints = [0 as c_ulong; 18];
    hints[0] = 1 | 2 | 4 | 8;
    hints[1] = wx as c_ulong;
    hints[2] = wy as c_ulong;
    hints[3] = SIZE as c_ulong;
    hints[4] = SIZE as c_ulong;
    set("WM_NORMAL_HINTS", x.atom(dpy, "WM_SIZE_HINTS"), &hints);
    let title = b"SmoothKvm drag";
    (x.XChangeProperty)(dpy, win, x.atom(dpy, "WM_NAME"), x11::XA_STRING, 8, x11::PropModeReplace, title.as_ptr(), title.len() as c_int);
    (x.XMapRaised)(dpy, win);
    (x.XFlush)(dpy);
    win
}

#[allow(clippy::too_many_arguments)]
fn run(x: X, dpy: *mut Display, id: u32, dir: &Path, names: &[String], edge: Edge, frac: f32, go: Box<dyn FnOnce() -> bool + Send>) -> bool {
    let a = Atoms::new(&x, dpy);
    let (sw, sh) = unsafe {
        let s = (x.XDefaultScreen)(dpy);
        ((x.XDisplayWidth)(dpy, s), (x.XDisplayHeight)(dpy, s))
    };
    let (cx, cy) = arrival(edge, frac, sw, sh);
    let win = unsafe { make_window(&x, dpy, cx, cy) };
    let files: Vec<PathBuf> = names.iter().map(|n| dir.join(n)).collect();
    let mut src = Source { x: &x, dpy, win, a, target: None, accepted: false, waiting: false, pending: None, time: 0, files, finished: None };
    let close = |x: &X| unsafe {
        (x.XDestroyWindow)(dpy, win);
        (x.XCloseDisplay)(dpy);
    };
    // wait until it is on screen, then take the mouse back and press the button on it
    let t = Instant::now();
    loop {
        let mut mapped = false;
        while unsafe { (x.XPending)(dpy) } > 0 {
            let mut ev = XEvent::zeroed();
            unsafe { (x.XNextEvent)(dpy, &mut ev) };
            mapped |= ev.kind() == x11::MapNotify;
        }
        if mapped || t.elapsed() > Duration::from_millis(600) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(60));
    if !go() {
        close(&x);
        return false;
    }
    let t = Instant::now();
    let mut pressed = false;
    while !pressed && t.elapsed() < Duration::from_millis(1500) {
        pressed = src.pump(40);
    }
    if !pressed {
        shared::warn("Could not continue the drag here (the button press did not arrive)");
        crate::capture_linux::end_carried_drag();
        close(&x);
        return false;
    }
    // we hold the drag now: offer the files
    unsafe {
        (x.XSetSelectionOwner)(dpy, a.selection, win, src.time);
        (x.XFlush)(dpy);
    }
    // carry it until you let go (seen on the real mouse), then drop once the files are in
    while !LET_GO.load(Ordering::SeqCst) {
        src.pump(30);
        if state(id).is_none() {
            break;
        }
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    while state(id) == Some(None) && Instant::now() < deadline {
        src.pump(50);
    }
    if state(id) == Some(Some(true)) {
        src.files = arrived(dir);
    }
    let ok = state(id) == Some(Some(true)) && !src.files.is_empty() && src.drop_now();
    crate::capture_linux::end_carried_drag(); // the button goes up here too
    if ok {
        let t = Instant::now();
        while src.finished.is_none() && t.elapsed() < Duration::from_secs(30) {
            src.pump(50);
        }
        shared::info(&format!("Dropped {} item(s) from the other computer", src.files.len()));
    }
    close(&x);
    // a target that never confirmed may still be copying: keep the staged files (cleaned up a day later)
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_lists_round_trip() {
        let p = vec![PathBuf::from("/home/me/.cache/smoothkvm-drag/0000002a/My Part #1.step"), PathBuf::from("/tmp/ä")];
        let s = uri_list(&p);
        assert_eq!(crate::drop_x11::parse_uri_list(&s), p);
        assert!(s.contains("My%20Part%20%231.step"));
    }

    #[test]
    fn arrives_on_the_right_edge() {
        assert_eq!(arrival(Edge::Right, 0.5, 1920, 1080), (1919, 540));
        assert_eq!(arrival(Edge::Left, 0.0, 1920, 1080), (0, 0));
        assert_eq!(arrival(Edge::Bottom, 1.0, 1920, 1080), (1920, 1079));
    }

    /// The drag source against a real X server: a fake drop target (playing Dolphin) under the pointer gets
    /// enter/position, accepts, gets the drop, asks for the file list and receives the staged files' URIs.
    #[test]
    fn real_x11_drag_source() {
        if std::env::var_os("DISPLAY").is_none() {
            return;
        }
        let Some(x) = X::load() else { return };
        let dpy = unsafe { (x.XOpenDisplay)(std::ptr::null()) };
        let tdpy = unsafe { (x.XOpenDisplay)(std::ptr::null()) };
        if dpy.is_null() || tdpy.is_null() {
            return;
        }
        let a = Atoms::new(&x, dpy);
        let ta = Atoms::new(&x, tdpy);
        // the target: an XDND-aware window at a known place
        let target = unsafe {
            let root = (x.XDefaultRootWindow)(tdpy);
            let mut attr: XSetWindowAttributes = std::mem::zeroed();
            attr.override_redirect = 1;
            let w = (x.XCreateWindow)(tdpy, root, 300, 300, 200, 200, 0, 0, 1, std::ptr::null_mut(), x11::CWOverrideRedirect, &mut attr);
            let v: [c_ulong; 1] = [5];
            (x.XChangeProperty)(tdpy, w, ta.aware, x11::XA_ATOM, 32, x11::PropModeReplace, v.as_ptr() as *const u8, 1);
            (x.XMapRaised)(tdpy, w);
            (x.XSync)(tdpy, 0);
            w
        };
        std::thread::sleep(Duration::from_millis(300));
        let src_win = unsafe {
            let root = (x.XDefaultRootWindow)(dpy);
            let mut attr: XSetWindowAttributes = std::mem::zeroed();
            (x.XCreateWindow)(dpy, root, 0, 0, 10, 10, 0, 0, 1, std::ptr::null_mut(), 0, &mut attr)
        };
        let files = vec![PathBuf::from("/tmp/skvm drag/Part 1.step")];
        let mut src = Source { x: &x, dpy, win: src_win, a, target: None, accepted: false, waiting: false, pending: None, time: 0, files: files.clone(), finished: None };
        unsafe {
            (x.XSetSelectionOwner)(dpy, a.selection, src_win, 0);
            (x.XFlush)(dpy);
        }
        src.position(400, 400);
        assert_eq!(src.target.map(|t| t.0), Some(target), "found the window under the pointer");
        // play the target: answer enter/position with "accept", the drop with a request for the data, then finish
        let mut got_list = String::new();
        let mut dropped = false;
        let mut sent = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && src.finished.is_none() {
            while unsafe { (x.XPending)(tdpy) } > 0 {
                let mut ev = XEvent::zeroed();
                unsafe { (x.XNextEvent)(tdpy, &mut ev) };
                let send_back = |kind: Atom, d: [c_long; 5]| {
                    let mut r = XEvent::zeroed();
                    r.pad[0] = x11::ClientMessage as c_long;
                    r.pad[CM_WINDOW] = src_win as c_long;
                    r.pad[CM_TYPE] = kind as c_long;
                    r.pad[6] = 32;
                    r.pad[CM_DATA..CM_DATA + 5].copy_from_slice(&d);
                    unsafe {
                        (x.XSendEvent)(tdpy, src_win, 0, 0, &mut r);
                        (x.XFlush)(tdpy);
                    }
                };
                if ev.kind() == x11::ClientMessage {
                    let kind = ev.long(CM_TYPE) as Atom;
                    if kind == ta.position {
                        send_back(ta.status, [target as c_long, 1, 0, 0, ta.action_copy as c_long]);
                    } else if kind == ta.drop {
                        dropped = true;
                        let prop = x.atom(tdpy, "SKVM_TEST");
                        unsafe {
                            (x.XConvertSelection)(tdpy, ta.selection, ta.uri_list, prop, target, 0);
                            (x.XFlush)(tdpy);
                        }
                    }
                } else if ev.kind() == x11::SelectionNotify {
                    let (mut typ, mut fmt, mut n, mut after, mut data) = (0 as Atom, 0 as c_int, 0 as c_ulong, 0 as c_ulong, std::ptr::null_mut::<u8>());
                    let prop = ev.long(7) as Atom;
                    unsafe {
                        (x.XGetWindowProperty)(tdpy, target, prop, 0, 1 << 16, 1, 0, &mut typ, &mut fmt, &mut n, &mut after, &mut data);
                        if !data.is_null() {
                            got_list = String::from_utf8_lossy(std::slice::from_raw_parts(data, n as usize)).to_string();
                            (x.XFree)(data as *mut c_void);
                        }
                    }
                    send_back(ta.finished, [target as c_long, 1, ta.action_copy as c_long, 0, 0]);
                }
            }
            src.pump(20);
            if src.accepted && !sent {
                sent = true;
                assert!(src.drop_now(), "the target accepted, so it drops");
            }
        }
        assert!(dropped, "the target got the drop");
        assert_eq!(crate::drop_x11::parse_uri_list(&got_list), files, "the target received the files");
        assert_eq!(src.finished, Some(true));
    }
}
