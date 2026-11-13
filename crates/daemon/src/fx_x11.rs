//! Linux: the screen-change effects (the same ones as on Windows, drawn by fx.rs) in a see-through window.
//!
//! Each effect gets a borderless, always-on-top 32-bit (with alpha) X11 window that mouse clicks pass straight
//! through (an empty input shape, via libXext), lives for the length of the effect, then disappears. KDE blends
//! it over the desktop. It runs on one worker thread with its own display connection. Without an X server or a
//! compositor (no 32-bit visual) it quietly does nothing.
use crate::fx::{self, Canvas};
use crate::x11::{self, Display, XSetWindowAttributes, XVisualInfo, X};
use kvm_protocol::{Animation, Edge, Visual};
use std::ffi::{c_char, c_int, c_void};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Play {
    a: Animation,
    edge: Edge,
    frac: f32,
}

static TX: Mutex<Option<Sender<Play>>> = Mutex::new(None);

fn send(p: Play) {
    if p.a.visual == Visual::None || p.a.duration_ms == 0 {
        return;
    }
    let Ok(mut g) = TX.lock() else { return };
    if g.is_none() {
        let (tx, rx) = channel();
        std::thread::spawn(move || worker(rx));
        *g = Some(tx);
    }
    if let Some(tx) = g.as_ref() {
        let _ = tx.send(p);
    }
}

/// Play the effect where the pointer leaves (or comes back) on `edge` of this screen, `frac` along it.
pub fn play_at_edge(a: Animation, edge: Edge, frac: f32) {
    send(Play { a, edge, frac });
}

/// "Preview on screen": middle of the right edge.
pub fn preview(a: Animation) {
    send(Play { a, edge: Edge::Right, frac: 0.5 });
}

type ShapeFn = unsafe extern "C" fn(*mut Display, x11::Window, c_int, c_int, c_int, *mut c_void, c_int, c_int, c_int);

fn worker(rx: Receiver<Play>) {
    let Some(x) = X::load() else { return };
    let Some(dpy) = x11::open(&x) else { return };
    // libXext's XShapeCombineRectangles makes the window click-through
    let shape: Option<ShapeFn> = unsafe {
        let lib = libc::dlopen(b"libXext.so.6\0".as_ptr() as *const c_char, libc::RTLD_NOW | libc::RTLD_LOCAL);
        let f = if lib.is_null() { std::ptr::null_mut() } else { libc::dlsym(lib, b"XShapeCombineRectangles\0".as_ptr() as *const c_char) };
        (!f.is_null()).then(|| std::mem::transmute::<*mut c_void, ShapeFn>(f))
    };
    while let Ok(p) = rx.recv() {
        // effects that piled up while one was playing: only the newest matters
        let mut p = p;
        while let Ok(newer) = rx.try_recv() {
            p = newer;
        }
        unsafe { run(&x, dpy, shape, &p) };
    }
}

unsafe fn run(x: &X, dpy: *mut Display, shape: Option<ShapeFn>, p: &Play) {
    let scr = (x.XDefaultScreen)(dpy);
    let root = (x.XDefaultRootWindow)(dpy);
    let (sw, sh) = ((x.XDisplayWidth)(dpy, scr), (x.XDisplayHeight)(dpy, scr));
    let bounds = (0, 0, sw, sh);
    let (px, py) = fx::edge_point(p.edge, p.frac, bounds);
    let (x0, y0, w, h) = fx::region(&p.a, px, py, p.edge, bounds);
    if w <= 0 || h <= 0 || w > 4000 || h > 4000 {
        return;
    }
    let mut vi: XVisualInfo = std::mem::zeroed();
    if (x.XMatchVisualInfo)(dpy, scr, 32, x11::TrueColor, &mut vi) == 0 {
        return; // no see-through windows here (no compositor)
    }
    let cmap = (x.XCreateColormap)(dpy, root, vi.visual, 0);
    let mut attr: XSetWindowAttributes = std::mem::zeroed();
    attr.override_redirect = 1;
    attr.colormap = cmap;
    attr.border_pixel = 0;
    attr.background_pixel = 0;
    let win = (x.XCreateWindow)(dpy, root, x0, y0, w as u32, h as u32, 0, 32, 1, vi.visual, x11::CWOverrideRedirect | x11::CWColormap | x11::CWBorderPixel | x11::CWBackPixel, &mut attr);
    if win == 0 {
        (x.XFreeColormap)(dpy, cmap);
        return;
    }
    if let Some(f) = shape {
        f(dpy, win, 2 /* ShapeInput */, 0, 0, std::ptr::null_mut(), 0, 0 /* ShapeSet */, 0);
    }
    let name = b"SmoothKvm effect\0";
    (x.XStoreName)(dpy, win, name.as_ptr() as *const c_char);
    (x.XMapRaised)(dpy, win);
    let gc = (x.XCreateGC)(dpy, win, 0, std::ptr::null_mut());
    let mut canvas = Canvas::new(w, h);
    // the canvas is premultiplied BGRA words: exactly a 32-bit ARGB ZPixmap on little-endian machines
    let img = (x.XCreateImage)(dpy, vi.visual, 32, x11::ZPixmap, 0, canvas.px.as_mut_ptr() as *mut c_char, w as u32, h as u32, 32, w * 4);
    if !img.is_null() {
        let (ox, oy) = (px - x0, py - y0);
        let dur = Duration::from_millis(p.a.duration_ms as u64).as_secs_f32();
        let start = Instant::now();
        loop {
            let t = (start.elapsed().as_secs_f32() / dur).min(1.0);
            fx::draw(&mut canvas, &p.a, t, p.edge, ox, oy);
            (x.XPutImage)(dpy, win, gc, img, 0, 0, 0, 0, w as u32, h as u32);
            (x.XFlush)(dpy);
            if t >= 1.0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // the pixels belong to the canvas: detach them (XImage.data, offset 16) before freeing the header
        *((img as *mut u8).add(16) as *mut *mut c_char) = std::ptr::null_mut();
        (x.XDestroyImage)(img);
    }
    (x.XFreeGC)(dpy, gc);
    (x.XDestroyWindow)(dpy, win);
    (x.XFreeColormap)(dpy, cmap);
    (x.XFlush)(dpy);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plays a slow effect on a real X server and checks the see-through window is there, with visible
    /// pixels, and that it is gone afterwards. Skipped without X or without 32-bit visuals.
    #[test]
    fn real_x11_effect() {
        if std::env::var_os("DISPLAY").is_none() {
            return;
        }
        let Some(x) = X::load() else { return };
        let Some(dpy) = x11::open(&x) else { return };
        let mut vi: XVisualInfo = unsafe { std::mem::zeroed() };
        if unsafe { (x.XMatchVisualInfo)(dpy, (x.XDefaultScreen)(dpy), 32, x11::TrueColor, &mut vi) } == 0 {
            return;
        }
        let mut a = Animation::default();
        a.visual = Visual::Glow;
        a.duration_ms = 1500;
        play_at_edge(a, Edge::Right, 0.5);
        std::thread::sleep(Duration::from_millis(500));
        let out = std::process::Command::new("xwininfo").args(["-name", "SmoothKvm effect"]).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let id = text.split_whitespace().find(|t| t.starts_with("0x")).expect("effect window is on screen");
        assert!(text.contains("Depth: 32"), "it is a see-through (32-bit) window");
        let wid = u64::from_str_radix(id.trim_start_matches("0x"), 16).unwrap() as x11::Window;
        let w = text.lines().find(|l| l.contains("Width:")).and_then(|l| l.split_whitespace().last()).and_then(|v| v.parse::<u32>().ok()).unwrap();
        let h = text.lines().find(|l| l.contains("Height:")).and_then(|l| l.split_whitespace().last()).and_then(|v| v.parse::<u32>().ok()).unwrap();
        let visible = unsafe {
            let im = (x.XGetImage)(dpy, wid, 0, 0, w, h, !0, x11::ZPixmap);
            assert!(!im.is_null());
            let data = *((im as *const u8).add(16) as *const *const u32);
            let n = (w * h) as usize;
            let v = std::slice::from_raw_parts(data, n).iter().filter(|p| **p >> 24 > 0).count();
            (x.XDestroyImage)(im);
            v
        };
        assert!(visible > 100, "the glow is drawn (pixels with alpha)");
        std::thread::sleep(Duration::from_millis(1500));
        let gone = std::process::Command::new("xwininfo").args(["-name", "SmoothKvm effect"]).output().unwrap();
        assert!(!gone.status.success(), "the effect window is removed when it ends");
        unsafe { (x.XCloseDisplay)(dpy) };
    }
}
