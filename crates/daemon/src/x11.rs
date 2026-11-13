//! libX11, loaded at runtime (dlopen) so SmoothKvm needs no X development files and still runs (without its
//! windows) where there is no X. Used by the drop strip and the app-window viewer. Each user of it opens its own
//! display connection on its own thread; XInitThreads is called once before anything else, and X protocol
//! errors are logged instead of ending the process (Xlib's default).
#![allow(non_snake_case, non_upper_case_globals, dead_code)]
use std::ffi::{c_char, c_int, c_long, c_uint, c_ulong, c_void, CString};

pub type Display = c_void;
pub type Window = c_ulong;
pub type Atom = c_ulong;

#[repr(C)]
pub struct XSetWindowAttributes {
    pub background_pixmap: c_ulong,
    pub background_pixel: c_ulong,
    pub border_pixmap: c_ulong,
    pub border_pixel: c_ulong,
    pub bit_gravity: c_int,
    pub win_gravity: c_int,
    pub backing_store: c_int,
    pub backing_planes: c_ulong,
    pub backing_pixel: c_ulong,
    pub save_under: c_int,
    pub event_mask: c_long,
    pub do_not_propagate_mask: c_long,
    pub override_redirect: c_int,
    pub colormap: c_ulong,
    pub cursor: c_ulong,
}

/// XEvent is a union of 24 longs; fields are read at their x86-64 offsets.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct XEvent {
    pub pad: [c_long; 24],
}

impl XEvent {
    pub fn zeroed() -> Self {
        XEvent { pad: [0; 24] }
    }
    pub fn kind(&self) -> c_int {
        self.pad[0] as c_int
    }
    pub fn long(&self, i: usize) -> c_long {
        self.pad[i]
    }
    /// A 32-bit field at a byte offset (for the int fields of key/button/configure events).
    pub fn int_at(&self, byte: usize) -> i32 {
        let b: &[u8; 192] = unsafe { &*(self.pad.as_ptr() as *const [u8; 192]) };
        i32::from_le_bytes(b[byte..byte + 4].try_into().unwrap())
    }
}

// event types
pub const KeyPress: c_int = 2;
pub const KeyRelease: c_int = 3;
pub const ButtonPress: c_int = 4;
pub const ButtonRelease: c_int = 5;
pub const MotionNotify: c_int = 6;
pub const FocusOut: c_int = 10;
pub const Expose: c_int = 12;
pub const ConfigureNotify: c_int = 22;
pub const SelectionNotify: c_int = 31;
pub const ClientMessage: c_int = 33;
// masks
pub const KeyPressMask: c_long = 1 << 0;
pub const KeyReleaseMask: c_long = 1 << 1;
pub const ButtonPressMask: c_long = 1 << 2;
pub const ButtonReleaseMask: c_long = 1 << 3;
pub const PointerMotionMask: c_long = 1 << 6;
pub const ExposureMask: c_long = 1 << 15;
pub const StructureNotifyMask: c_long = 1 << 17;
pub const FocusChangeMask: c_long = 1 << 21;
pub const CWBackPixel: c_ulong = 1 << 1;
pub const CWOverrideRedirect: c_ulong = 1 << 9;
pub const CWEventMask: c_ulong = 1 << 11;
pub const PropModeReplace: c_int = 0;
pub const XA_ATOM: Atom = 4;
pub const XA_STRING: Atom = 31;
pub const ZPixmap: c_int = 2;

type ErrorHandler = unsafe extern "C" fn(*mut Display, *mut c_void) -> c_int;

pub struct X {
    pub lib: *mut c_void,
    pub XOpenDisplay: unsafe extern "C" fn(*const c_char) -> *mut Display,
    pub XCloseDisplay: unsafe extern "C" fn(*mut Display) -> c_int,
    pub XDefaultRootWindow: unsafe extern "C" fn(*mut Display) -> Window,
    pub XDefaultScreen: unsafe extern "C" fn(*mut Display) -> c_int,
    pub XDefaultVisual: unsafe extern "C" fn(*mut Display, c_int) -> *mut c_void,
    pub XDefaultDepth: unsafe extern "C" fn(*mut Display, c_int) -> c_int,
    pub XDisplayWidth: unsafe extern "C" fn(*mut Display, c_int) -> c_int,
    pub XDisplayHeight: unsafe extern "C" fn(*mut Display, c_int) -> c_int,
    pub XCreateWindow: unsafe extern "C" fn(*mut Display, Window, c_int, c_int, c_uint, c_uint, c_uint, c_int, c_uint, *mut c_void, c_ulong, *mut XSetWindowAttributes) -> Window,
    pub XDestroyWindow: unsafe extern "C" fn(*mut Display, Window) -> c_int,
    pub XInternAtom: unsafe extern "C" fn(*mut Display, *const c_char, c_int) -> Atom,
    pub XChangeProperty: unsafe extern "C" fn(*mut Display, Window, Atom, Atom, c_int, c_int, *const u8, c_int) -> c_int,
    pub XMapRaised: unsafe extern "C" fn(*mut Display, Window) -> c_int,
    pub XUnmapWindow: unsafe extern "C" fn(*mut Display, Window) -> c_int,
    pub XMoveResizeWindow: unsafe extern "C" fn(*mut Display, Window, c_int, c_int, c_uint, c_uint) -> c_int,
    pub XResizeWindow: unsafe extern "C" fn(*mut Display, Window, c_uint, c_uint) -> c_int,
    pub XSelectInput: unsafe extern "C" fn(*mut Display, Window, c_long) -> c_int,
    pub XSetWMProtocols: unsafe extern "C" fn(*mut Display, Window, *mut Atom, c_int) -> c_int,
    pub XStoreName: unsafe extern "C" fn(*mut Display, Window, *const c_char) -> c_int,
    pub XPending: unsafe extern "C" fn(*mut Display) -> c_int,
    pub XNextEvent: unsafe extern "C" fn(*mut Display, *mut XEvent) -> c_int,
    pub XSendEvent: unsafe extern "C" fn(*mut Display, Window, c_int, c_long, *mut XEvent) -> c_int,
    pub XConvertSelection: unsafe extern "C" fn(*mut Display, Atom, Atom, Atom, Window, c_ulong) -> c_int,
    pub XSetSelectionOwner: unsafe extern "C" fn(*mut Display, Atom, Window, c_ulong) -> c_int,
    pub XGetWindowProperty: unsafe extern "C" fn(*mut Display, Window, Atom, c_long, c_long, c_int, Atom, *mut Atom, *mut c_int, *mut c_ulong, *mut c_ulong, *mut *mut u8) -> c_int,
    pub XFree: unsafe extern "C" fn(*mut c_void) -> c_int,
    pub XFlush: unsafe extern "C" fn(*mut Display) -> c_int,
    pub XSync: unsafe extern "C" fn(*mut Display, c_int) -> c_int,
    pub XConnectionNumber: unsafe extern "C" fn(*mut Display) -> c_int,
    pub XDefaultGC: unsafe extern "C" fn(*mut Display, c_int) -> *mut c_void,
    pub XSetForeground: unsafe extern "C" fn(*mut Display, *mut c_void, c_ulong) -> c_int,
    pub XDrawString: unsafe extern "C" fn(*mut Display, Window, *mut c_void, c_int, c_int, *const c_char, c_int) -> c_int,
    pub XSetWindowBackground: unsafe extern "C" fn(*mut Display, Window, c_ulong) -> c_int,
    pub XClearWindow: unsafe extern "C" fn(*mut Display, Window) -> c_int,
    pub XCreateImage: unsafe extern "C" fn(*mut Display, *mut c_void, c_uint, c_int, c_int, *mut c_char, c_uint, c_uint, c_int, c_int) -> *mut c_void,
    pub XPutImage: unsafe extern "C" fn(*mut Display, Window, *mut c_void, *mut c_void, c_int, c_int, c_int, c_int, c_uint, c_uint) -> c_int,
    pub XDestroyImage: unsafe extern "C" fn(*mut c_void) -> c_int,
    pub XGetImage: unsafe extern "C" fn(*mut Display, Window, c_int, c_int, c_uint, c_uint, c_ulong, c_int) -> *mut c_void,
    pub XCreateGC: unsafe extern "C" fn(*mut Display, Window, c_ulong, *mut c_void) -> *mut c_void,
    pub XFreeGC: unsafe extern "C" fn(*mut Display, *mut c_void) -> c_int,
    pub XMatchVisualInfo: unsafe extern "C" fn(*mut Display, c_int, c_int, c_int, *mut XVisualInfo) -> c_int,
    pub XCreateColormap: unsafe extern "C" fn(*mut Display, Window, *mut c_void, c_int) -> c_ulong,
    pub XFreeColormap: unsafe extern "C" fn(*mut Display, c_ulong) -> c_int,
}

#[repr(C)]
pub struct XVisualInfo {
    pub visual: *mut c_void,
    pub visualid: c_ulong,
    pub screen: c_int,
    pub depth: c_int,
    pub class: c_int,
    pub red_mask: c_ulong,
    pub green_mask: c_ulong,
    pub blue_mask: c_ulong,
    pub colormap_size: c_int,
    pub bits_per_rgb: c_int,
}

pub const CWBorderPixel: c_ulong = 1 << 3;
pub const CWColormap: c_ulong = 1 << 13;
pub const TrueColor: c_int = 4;

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

unsafe extern "C" fn on_x_error(_d: *mut Display, e: *mut c_void) -> c_int {
    // XErrorEvent: type, display, resourceid, serial, error_code (byte at offset 32)
    let code = *(e as *const u8).add(32);
    eprintln!("X11 error {code} (ignored)");
    0
}

static INIT: std::sync::Once = std::sync::Once::new();

impl X {
    pub fn load() -> Option<X> {
        unsafe {
            let name = CString::new("libX11.so.6").unwrap();
            let lib = libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if lib.is_null() {
                return None;
            }
            INIT.call_once(|| {
                let p = libc::dlsym(lib, b"XInitThreads\0".as_ptr() as *const c_char);
                if !p.is_null() {
                    let f: unsafe extern "C" fn() -> c_int = std::mem::transmute(p);
                    f();
                }
                let p = libc::dlsym(lib, b"XSetErrorHandler\0".as_ptr() as *const c_char);
                if !p.is_null() {
                    let f: unsafe extern "C" fn(Option<ErrorHandler>) -> Option<ErrorHandler> = std::mem::transmute(p);
                    f(Some(on_x_error));
                }
            });
            Some(X {
                lib,
                XOpenDisplay: sym!(lib, XOpenDisplay),
                XCloseDisplay: sym!(lib, XCloseDisplay),
                XDefaultRootWindow: sym!(lib, XDefaultRootWindow),
                XDefaultScreen: sym!(lib, XDefaultScreen),
                XDefaultVisual: sym!(lib, XDefaultVisual),
                XDefaultDepth: sym!(lib, XDefaultDepth),
                XDisplayWidth: sym!(lib, XDisplayWidth),
                XDisplayHeight: sym!(lib, XDisplayHeight),
                XCreateWindow: sym!(lib, XCreateWindow),
                XDestroyWindow: sym!(lib, XDestroyWindow),
                XInternAtom: sym!(lib, XInternAtom),
                XChangeProperty: sym!(lib, XChangeProperty),
                XMapRaised: sym!(lib, XMapRaised),
                XUnmapWindow: sym!(lib, XUnmapWindow),
                XMoveResizeWindow: sym!(lib, XMoveResizeWindow),
                XResizeWindow: sym!(lib, XResizeWindow),
                XSelectInput: sym!(lib, XSelectInput),
                XSetWMProtocols: sym!(lib, XSetWMProtocols),
                XStoreName: sym!(lib, XStoreName),
                XPending: sym!(lib, XPending),
                XNextEvent: sym!(lib, XNextEvent),
                XSendEvent: sym!(lib, XSendEvent),
                XConvertSelection: sym!(lib, XConvertSelection),
                XSetSelectionOwner: sym!(lib, XSetSelectionOwner),
                XGetWindowProperty: sym!(lib, XGetWindowProperty),
                XFree: sym!(lib, XFree),
                XFlush: sym!(lib, XFlush),
                XSync: sym!(lib, XSync),
                XConnectionNumber: sym!(lib, XConnectionNumber),
                XDefaultGC: sym!(lib, XDefaultGC),
                XSetForeground: sym!(lib, XSetForeground),
                XDrawString: sym!(lib, XDrawString),
                XSetWindowBackground: sym!(lib, XSetWindowBackground),
                XClearWindow: sym!(lib, XClearWindow),
                XCreateImage: sym!(lib, XCreateImage),
                XPutImage: sym!(lib, XPutImage),
                XDestroyImage: sym!(lib, XDestroyImage),
                XGetImage: sym!(lib, XGetImage),
                XCreateGC: sym!(lib, XCreateGC),
                XFreeGC: sym!(lib, XFreeGC),
                XMatchVisualInfo: sym!(lib, XMatchVisualInfo),
                XCreateColormap: sym!(lib, XCreateColormap),
                XFreeColormap: sym!(lib, XFreeColormap),
            })
        }
    }

    pub fn atom(&self, dpy: *mut Display, name: &str) -> Atom {
        let n = CString::new(name).unwrap_or_default();
        unsafe { (self.XInternAtom)(dpy, n.as_ptr(), 0) }
    }
}

/// Where the desktop's X server is (XWayland on KDE). A system service doesn't inherit the desktop session's
/// environment, so ask the user's systemd for it, or fall back to the usual places.
pub fn find_display() -> Option<(String, Option<String>)> {
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

/// Open the desktop's display (None when there is no desktop session / no X).
pub fn open(x: &X) -> Option<*mut Display> {
    let (display, auth) = find_display()?;
    if let Some(a) = &auth {
        if std::env::var_os("XAUTHORITY").is_none() {
            std::env::set_var("XAUTHORITY", a);
        }
    }
    let name = CString::new(display).ok()?;
    let dpy = unsafe { (x.XOpenDisplay)(name.as_ptr()) };
    (!dpy.is_null()).then_some(dpy)
}
