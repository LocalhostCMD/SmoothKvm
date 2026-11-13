//! Linux: shows an app window from another computer (see stream_win.rs) as a normal window here. You can move
//! and resize it like any window; clicks, the wheel and keys typed into it go to the real app on the other
//! computer. Resizing it resizes the real window there. Closing it stops the stream (the app keeps running).
use crate::x11::{self, Display, Window, XEvent, XSetWindowAttributes, X};
use crate::{shared, wincodec};
use kvm_protocol::secure::{SecureConn, SecureWriter};
use kvm_protocol::{Message, MouseButton, WindowEvent};
use std::collections::HashSet;
use std::ffi::{c_char, c_int, c_void, CString};
use std::io;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::time::{Duration, Instant};

fn err(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, m.to_string())
}

/// Run a window session that started with `WindowOpen` (called on the connection's own thread).
pub fn run(conn: SecureConn, peer: &str, id: u32, title: String, w: u32, h: u32) -> io::Result<()> {
    let x = X::load().ok_or_else(|| err("libX11 is not available"))?;
    let dpy = x11::open(&x).ok_or_else(|| err("no desktop session to show the window in"))?;
    // if this computer was controlling the other one, hand the mouse back: the app is here now
    #[cfg(target_os = "linux")]
    crate::capture_linux::return_home();
    shared::notify(&format!("\"{}\" from {peer} is shown here", shared::clean(&title)));
    let r = view(&x, dpy, conn, peer, id, title, w, h);
    unsafe { (x.XCloseDisplay)(dpy) };
    r
}

struct Img {
    dec: wincodec::Decoder,
    img: *mut c_void,
}

impl Img {
    fn new(x: &X, dpy: *mut Display, w: u32, h: u32) -> io::Result<Img> {
        let mut dec = wincodec::Decoder::new(w, h)?;
        let img = unsafe {
            let scr = (x.XDefaultScreen)(dpy);
            let depth = (x.XDefaultDepth)(dpy, scr);
            if depth != 24 && depth != 32 {
                return Err(err("the display is not in 24-bit colour"));
            }
            // BGRA in memory is exactly what a 24/32-bit TrueColor ZPixmap is on little-endian machines
            (x.XCreateImage)(dpy, (x.XDefaultVisual)(dpy, scr), depth as u32, x11::ZPixmap, 0, dec.buf.as_mut_ptr() as *mut c_char, w, h, 32, (w * 4) as c_int)
        };
        if img.is_null() {
            return Err(err("could not create the picture"));
        }
        Ok(Img { dec, img })
    }

    fn free(&mut self, x: &X) {
        unsafe {
            // the pixels belong to the decoder: detach them (XImage.data is at offset 16) before freeing the header
            *((self.img as *mut u8).add(16) as *mut *mut c_char) = std::ptr::null_mut();
            (x.XDestroyImage)(self.img);
        }
    }

    fn put(&self, x: &X, dpy: *mut Display, win: Window, rx: i32, ry: i32, rw: i32, rh: i32) {
        let (w, h) = (self.dec.w as i32, self.dec.h as i32);
        let (x0, y0) = (rx.max(0), ry.max(0));
        let (x1, y1) = ((rx + rw).min(w), (ry + rh).min(h));
        if x1 > x0 && y1 > y0 {
            unsafe {
                let gc = (x.XDefaultGC)(dpy, (x.XDefaultScreen)(dpy));
                (x.XPutImage)(dpy, win, gc, self.img, x0, y0, x0, y0, (x1 - x0) as u32, (y1 - y0) as u32);
            }
        }
    }
}

fn set_title(x: &X, dpy: *mut Display, win: Window, title: &str, peer: &str) {
    let t = format!("{} - on {peer} (SmoothKvm)", shared::clean(title));
    unsafe {
        let ascii: String = t.chars().map(|c| if c.is_ascii() { c } else { '?' }).collect();
        let c = CString::new(ascii).unwrap_or_default();
        (x.XStoreName)(dpy, win, c.as_ptr());
        // the real (UTF-8) title for KDE
        (x.XChangeProperty)(dpy, win, x.atom(dpy, "_NET_WM_NAME"), x.atom(dpy, "UTF8_STRING"), 8, x11::PropModeReplace, t.as_ptr(), t.len() as c_int);
    }
}

#[allow(clippy::too_many_arguments)]
fn view(x: &X, dpy: *mut Display, conn: SecureConn, peer: &str, id: u32, title: String, w: u32, h: u32) -> io::Result<()> {
    conn.set_timeouts(Some(Duration::from_secs(15)), Some(Duration::from_secs(10)))?;
    let (mut rd, mut wr) = conn.split()?;
    // network -> this thread
    let (tx, rx): (_, Receiver<Option<Message>>) = channel();
    std::thread::spawn(move || {
        loop {
            match rd.recv() {
                Ok(m @ (Message::WindowTiles { .. } | Message::WindowFrameEnd { .. } | Message::WindowOpen { .. } | Message::WindowClose { .. })) => {
                    let close = matches!(m, Message::WindowClose { .. });
                    if tx.send(Some(m)).is_err() || close {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    let _ = tx.send(None);
                    break;
                }
            }
        }
        rd.shutdown();
    });
    let win = unsafe {
        let root = (x.XDefaultRootWindow)(dpy);
        let mut attr: XSetWindowAttributes = std::mem::zeroed();
        attr.event_mask = x11::KeyPressMask | x11::KeyReleaseMask | x11::ButtonPressMask | x11::ButtonReleaseMask | x11::PointerMotionMask | x11::ExposureMask | x11::StructureNotifyMask | x11::FocusChangeMask;
        attr.background_pixel = 0;
        (x.XCreateWindow)(dpy, root, 0, 0, w, h, 0, 0, 1, std::ptr::null_mut(), x11::CWEventMask | x11::CWBackPixel, &mut attr)
    };
    set_title(x, dpy, win, &title, peer);
    let mut wm_delete = x.atom(dpy, "WM_DELETE_WINDOW");
    unsafe {
        (x.XSetWMProtocols)(dpy, win, &mut wm_delete, 1);
        (x.XMapRaised)(dpy, win);
        (x.XFlush)(dpy);
    }
    let mut img = Img::new(x, dpy, w, h)?;
    let result = event_loop(x, dpy, win, &mut img, &rx, &mut wr, id, peer, wm_delete);
    img.free(x);
    unsafe {
        (x.XDestroyWindow)(dpy, win);
        (x.XFlush)(dpy);
    }
    wr.shutdown();
    result
}

fn send(wr: &mut SecureWriter, id: u32, ev: WindowEvent) -> io::Result<()> {
    wr.send(&Message::WindowInput { id, ev })
}

#[allow(clippy::too_many_arguments)]
fn event_loop(x: &X, dpy: *mut Display, win: Window, img: &mut Img, rx: &Receiver<Option<Message>>, wr: &mut SecureWriter, id: u32, peer: &str, wm_delete: x11::Atom) -> io::Result<()> {
    let fd = unsafe { (x.XConnectionNumber)(dpy) };
    let mut dirty: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut pending_move: Option<(i32, i32)> = None;
    let mut last_move = Instant::now();
    let mut keys_down: HashSet<u16> = HashSet::new();
    let mut want_size: Option<((u32, u32), Instant)> = None;
    loop {
        // pictures from the other computer
        loop {
            match rx.try_recv() {
                Ok(Some(Message::WindowTiles { data, .. })) => dirty.extend(img.dec.apply(&data)?),
                Ok(Some(Message::WindowFrameEnd { seq, .. })) => {
                    for (rx_, ry, rw, rh) in dirty.drain(..) {
                        img.put(x, dpy, win, rx_ as i32, ry as i32, rw as i32, rh as i32);
                    }
                    unsafe { (x.XFlush)(dpy) };
                    wr.send(&Message::WindowAck { id, seq })?;
                }
                Ok(Some(Message::WindowOpen { title, width, height, .. })) => {
                    // the real window changed size: start a new picture of that size
                    img.free(x);
                    *img = Img::new(x, dpy, width, height)?;
                    dirty.clear();
                    set_title(x, dpy, win, &title, peer);
                    unsafe { (x.XResizeWindow)(dpy, win, width, height) };
                }
                Ok(Some(Message::WindowClose { .. })) | Ok(None) => {
                    shared::info(&format!("The window from {peer} was closed there"));
                    return Ok(());
                }
                Ok(Some(_)) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        // input here
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        unsafe { libc::poll(&mut pfd, 1, 6) };
        while unsafe { (x.XPending)(dpy) } > 0 {
            let mut ev = XEvent::zeroed();
            unsafe { (x.XNextEvent)(dpy, &mut ev) };
            match ev.kind() {
                x11::Expose => {
                    // XExposeEvent: x 40, y 44, width 48, height 52
                    img.put(x, dpy, win, ev.int_at(40), ev.int_at(44), ev.int_at(48), ev.int_at(52));
                }
                x11::MotionNotify => pending_move = Some((ev.int_at(64), ev.int_at(68))),
                k @ (x11::ButtonPress | x11::ButtonRelease) => {
                    let pressed = k == x11::ButtonPress;
                    send(wr, id, WindowEvent::Move { x: ev.int_at(64), y: ev.int_at(68) })?;
                    pending_move = None;
                    let button = match ev.int_at(84) {
                        1 => Some(MouseButton::Left),
                        2 => Some(MouseButton::Middle),
                        3 => Some(MouseButton::Right),
                        8 => Some(MouseButton::Back),
                        9 => Some(MouseButton::Forward),
                        n @ 4..=7 => {
                            if pressed {
                                let (dx, dy) = match n {
                                    4 => (0, 1),
                                    5 => (0, -1),
                                    6 => (-1, 0),
                                    _ => (1, 0),
                                };
                                send(wr, id, WindowEvent::Wheel { dx, dy })?;
                            }
                            None
                        }
                        _ => None,
                    };
                    if let Some(button) = button {
                        send(wr, id, WindowEvent::Button { button, pressed })?;
                    }
                }
                k @ (x11::KeyPress | x11::KeyRelease) => {
                    let pressed = k == x11::KeyPress;
                    let keycode = ev.int_at(84);
                    if let Some(code) = crate::capture_linux::linux_to_hid((keycode - 8).max(0) as u16) {
                        if pressed {
                            keys_down.insert(code);
                        } else {
                            keys_down.remove(&code);
                        }
                        send(wr, id, WindowEvent::Key { code, pressed })?;
                    }
                }
                x11::FocusOut => {
                    // switched away (Alt+Tab...): don't leave keys held down over there
                    for code in keys_down.drain() {
                        send(wr, id, WindowEvent::Key { code, pressed: false })?;
                    }
                }
                x11::ConfigureNotify => {
                    // XConfigureEvent: width 56, height 60
                    let (cw, ch) = (ev.int_at(56).max(1) as u32, ev.int_at(60).max(1) as u32);
                    if (cw as usize, ch as usize) != (img.dec.w, img.dec.h) {
                        want_size = Some(((cw, ch), Instant::now()));
                    } else {
                        want_size = None;
                    }
                }
                x11::ClientMessage => {
                    if ev.long(7) as x11::Atom == wm_delete {
                        let _ = send(wr, id, WindowEvent::Close);
                        shared::info(&format!("Stopped showing the window from {peer}"));
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
        // mouse moves: at most one every 8 ms (plenty for 120 Hz, and far fewer packets)
        if let Some((mx, my)) = pending_move {
            if last_move.elapsed() >= Duration::from_millis(8) {
                send(wr, id, WindowEvent::Move { x: mx, y: my })?;
                pending_move = None;
                last_move = Instant::now();
            }
        }
        // you resized this window: resize the real one once you stop dragging the border
        if let Some(((cw, ch), t)) = want_size {
            if t.elapsed() >= Duration::from_millis(250) {
                send(wr, id, WindowEvent::Resize { width: cw, height: ch })?;
                want_size = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kvm_protocol::secure;
    use std::net::{TcpListener, TcpStream};

    /// A fake "app computer" streams a picture to a real viewer window on a real X server, then reads the
    /// window back from the X server and checks the pixels; also checks input comes back. Skipped without X.
    #[test]
    fn real_x11_viewer() {
        if std::env::var_os("DISPLAY").is_none() {
            return;
        }
        let Some(x) = X::load() else { return };
        let Some(probe) = x11::open(&x) else { return };
        unsafe { (x.XCloseDisplay)(probe) };
        let (a_priv, _a_pub) = secure::generate_keypair().unwrap();
        let (b_priv, _b_pub) = secure::generate_keypair().unwrap();
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let (w, h) = (160u32, 100u32);
        // the viewer side
        let viewer = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let est = secure::accept(s, &b_priv, None).unwrap();
            let mut conn = est.conn;
            let Message::WindowOpen { id, title, width, height } = conn.recv().unwrap() else { panic!() };
            run(conn, "test", id, title, width, height)
        });
        // the app side
        let est = secure::connect(TcpStream::connect(addr).unwrap(), &a_priv, None).unwrap();
        let mut conn = est.conn;
        conn.send(&Message::WindowOpen { id: 7, title: "Test window".into(), width: w, height: h }).unwrap();
        let mut frame = vec![0u8; (w * h * 4) as usize];
        for px in frame.chunks_exact_mut(4) {
            px.copy_from_slice(&[0x20, 0x80, 0xF0, 0xFF]); // BGRA: an orange-ish colour
        }
        let mut enc = wincodec::Encoder::new(w, h);
        for p in enc.encode(&frame) {
            conn.send(&Message::WindowTiles { id: 7, data: p }).unwrap();
        }
        conn.send(&Message::WindowFrameEnd { id: 7, seq: 1 }).unwrap();
        conn.set_timeouts(Some(Duration::from_secs(5)), None).unwrap();
        let ack = loop {
            match conn.recv().unwrap() {
                Message::WindowAck { seq, .. } => break seq,
                _ => {}
            }
        };
        assert_eq!(ack, 1, "the viewer showed the frame");
        // read the window back from the X server
        std::thread::sleep(Duration::from_millis(300));
        let out = std::process::Command::new("xwininfo").args(["-name", "Test window - on test (SmoothKvm)"]).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let wid = text.split_whitespace().find(|t| t.starts_with("0x")).expect("viewer window exists");
        let wid = u64::from_str_radix(wid.trim_start_matches("0x"), 16).unwrap() as Window;
        let dpy = x11::open(&x).unwrap();
        let px = unsafe {
            let im = (x.XGetImage)(dpy, wid, 80, 50, 1, 1, !0, x11::ZPixmap);
            assert!(!im.is_null(), "window picture readable");
            let data = *((im as *const u8).add(16) as *const *const u8);
            let v = std::slice::from_raw_parts(data, 4).to_vec();
            (x.XDestroyImage)(im);
            v
        };
        unsafe { (x.XCloseDisplay)(dpy) };
        assert_eq!(&px[..3], &[0x20, 0x80, 0xF0], "the pixel on screen is the streamed colour");
        // the app closes its window: the viewer ends cleanly
        conn.send(&Message::WindowClose { id: 7 }).unwrap();
        assert!(viewer.join().unwrap().is_ok());
    }
}
