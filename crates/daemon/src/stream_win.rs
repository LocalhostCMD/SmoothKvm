//! Windows: show an app's window on another computer ("drag the app over").
//!
//! The app keeps running here. Its window is captured about 30 times a second (PrintWindow with full content,
//! which also works while it is covered by other windows), only the changed tiles are sent (see wincodec),
//! and the clicks, wheel and keys you use on the other computer's copy of the window are played back here,
//! on that window. Frames are acknowledged, so a slow network shows fewer frames instead of growing lag.
use crate::store::Identity;
use crate::{inject_win, net, shared, wincodec, xfer};
use kvm_protocol::{InputEvent, Message, WindowEvent};
use serde::Serialize;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Serialize, Clone, Debug)]
pub struct AppWindow {
    pub id: i64,
    pub title: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Serialize, Clone, Debug)]
pub struct Stream {
    pub id: u32,
    pub window: i64,
    pub title: String,
    pub to: String,
    #[serde(skip)]
    stop: Arc<AtomicBool>,
}

static STREAMS: Mutex<Vec<Stream>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU32 = AtomicU32::new(1);

fn hwnd_of(id: i64) -> HWND {
    HWND(id as isize as *mut _)
}

unsafe fn title_of(h: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = GetWindowTextW(h, &mut buf);
    shared::clean(&String::from_utf16_lossy(&buf[..n.max(0) as usize]))
}

unsafe fn client_size(h: HWND) -> Option<(u32, u32)> {
    let mut r = RECT::default();
    GetClientRect(h, &mut r).ok()?;
    let (w, ht) = ((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32);
    (w > 0 && ht > 0).then_some((w.min(wincodec::MAX_SIDE), ht.min(wincodec::MAX_SIDE)))
}

unsafe extern "system" fn enum_proc(h: HWND, lp: LPARAM) -> BOOL {
    let v = &mut *(lp.0 as *mut Vec<AppWindow>);
    let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
    let owner = GetWindow(h, GW_OWNER).unwrap_or_default();
    let mut pid = 0u32;
    GetWindowThreadProcessId(h, Some(&mut pid));
    if !IsWindowVisible(h).as_bool() || ex & WS_EX_TOOLWINDOW.0 != 0 || !owner.0.is_null() || pid == std::process::id() {
        return BOOL(1);
    }
    let title = title_of(h);
    if title.is_empty() || title == "Program Manager" {
        return BOOL(1);
    }
    let (w, ht) = if IsIconic(h).as_bool() { (0, 0) } else { client_size(h).unwrap_or((0, 0)) };
    if IsIconic(h).as_bool() || (w >= 120 && ht >= 80) {
        v.push(AppWindow { id: h.0 as isize as i64, title, width: w, height: ht });
    }
    BOOL(1)
}

/// The app windows you could show on another computer (visible, titled, not SmoothKvm's own).
pub fn list() -> Vec<AppWindow> {
    let mut v: Vec<AppWindow> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut v as *mut _ as isize));
    }
    v
}

pub fn streams() -> Vec<Stream> {
    STREAMS.lock().map(|s| s.clone()).unwrap_or_default()
}

pub fn stop(id: u32) {
    if let Ok(s) = STREAMS.lock() {
        for x in s.iter().filter(|x| x.id == id) {
            x.stop.store(true, Ordering::SeqCst);
        }
    }
}

/// Start showing window `window` on computer `to` (None = the only/first one). Returns that computer's name.
pub fn start(window: i64, to: Option<String>, ident: Arc<Identity>) -> io::Result<String> {
    let h = hwnd_of(window);
    if !unsafe { IsWindow(h) }.as_bool() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "that window is gone"));
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    if pid == std::process::id() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "SmoothKvm's own windows are not shown on other computers"));
    }
    if streams().iter().any(|s| s.window == window) {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "that window is already shown on another computer"));
    }
    let target = xfer::find_target(to.as_deref())?;
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    let title = unsafe { title_of(h) };
    let stop = Arc::new(AtomicBool::new(false));
    if let Ok(mut s) = STREAMS.lock() {
        s.push(Stream { id, window, title: title.clone(), to: target.name.clone(), stop: stop.clone() });
    }
    let name = target.name.clone();
    std::thread::spawn(move || {
        shared::info(&format!("Showing \"{title}\" on {}", target.name));
        match run(id, hwnd_of(window), &title, &target, &ident, stop) {
            Ok(()) => shared::info(&format!("Stopped showing \"{title}\" on {}", target.name)),
            Err(e) => shared::warn(&format!("Showing \"{title}\" on {} stopped: {e}", target.name)),
        }
        if let Ok(mut s) = STREAMS.lock() {
            s.retain(|x| x.id != id);
        }
    });
    Ok(name)
}

/// `SmoothKvm stream "<part of a window title>" --to <host:port> [--pin PIN]`: show a window on a computer
/// directly from the command line (pairs first when a PIN is given). Runs until the window or viewer closes.
pub fn run_cli(title_part: &str, addr: &str, pin: Option<&str>) -> io::Result<()> {
    let ident = Identity::load_or_create()?;
    let want = title_part.to_lowercase();
    let w = list().into_iter().find(|w| w.title.to_lowercase().contains(&want)).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no window titled like \"{title_part}\"")))?;
    if let Some(pin) = pin {
        drop(net::connect(addr, &ident, Some(pin), "screen")?);
        println!("Paired with {addr}.");
    }
    let target = xfer::Target { name: addr.to_string(), addr: addr.to_string(), kind: "screen" };
    println!("Showing \"{}\" on {addr} (Ctrl+C to stop)...", w.title);
    run(NEXT_ID.fetch_add(1, Ordering::SeqCst), hwnd_of(w.id), &w.title, &target, &ident, Arc::new(AtomicBool::new(false)))
}

/// An off-screen 32-bit picture the window is drawn into.
struct Grab {
    w: u32,
    h: u32,
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
}

impl Grab {
    unsafe fn new(w: u32, h: u32) -> Option<Grab> {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(screen);
        ReleaseDC(None, screen);
        let mut bi = BITMAPINFO::default();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w as i32;
        bi.bmiHeader.biHeight = -(h as i32); // top-down rows
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB.0;
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        if bits.is_null() {
            let _ = DeleteDC(dc);
            return None;
        }
        let old = SelectObject(dc, bmp);
        Some(Grab { w, h, dc, bmp, old, bits: bits as *mut u8 })
    }

    unsafe fn capture(&self, h: HWND) -> bool {
        // PW_CLIENTONLY | PW_RENDERFULLCONTENT: just the content, including GPU-drawn (DirectX/OpenGL) parts
        let ok = PrintWindow(h, self.dc, PRINT_WINDOW_FLAGS(1 | 2)).as_bool();
        let _ = GdiFlush();
        ok
    }

    fn pixels(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.bits, (self.w * self.h * 4) as usize) }
    }
}

impl Drop for Grab {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp);
            let _ = DeleteDC(self.dc);
        }
    }
}

fn run(id: u32, h: HWND, title: &str, target: &xfer::Target, ident: &Identity, stop: Arc<AtomicBool>) -> io::Result<()> {
    unsafe {
        if IsIconic(h).as_bool() {
            let _ = ShowWindow(h, SW_RESTORE);
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    let (w, ht) = unsafe { client_size(h) }.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the window has no size"))?;
    let mut conn = net::connect(&target.addr, ident, None, &target.name)?;
    conn.send(&Message::WindowOpen { id, title: title.to_string(), width: w, height: ht })?;
    conn.set_timeouts(None, Some(Duration::from_secs(10)))?;
    let (mut rd, mut wr) = conn.split()?;
    let acked = Arc::new(AtomicU32::new(0));
    {
        let (acked, stop) = (acked.clone(), stop.clone());
        let hw = h.0 as isize;
        std::thread::spawn(move || {
            let h = HWND(hw as *mut _);
            while let Ok(m) = rd.recv() {
                match m {
                    Message::WindowAck { seq, .. } => acked.store(seq, Ordering::SeqCst),
                    Message::WindowInput { ev: WindowEvent::Close, .. } | Message::WindowClose { .. } => break,
                    Message::WindowInput { ev, .. } => unsafe { apply(h, ev) },
                    _ => {}
                }
            }
            stop.store(true, Ordering::SeqCst);
        });
    }
    let mut enc = wincodec::Encoder::new(w, ht);
    let mut grab = unsafe { Grab::new(w, ht) }.ok_or_else(|| io::Error::new(io::ErrorKind::Other, "cannot capture the window"))?;
    let mut seq = 0u32;
    let mut last_sent = Instant::now();
    let frame_time = Duration::from_millis(33);
    while !stop.load(Ordering::SeqCst) {
        let t0 = Instant::now();
        if !unsafe { IsWindow(h) }.as_bool() {
            let _ = wr.send(&Message::WindowClose { id });
            return Ok(());
        }
        // at most 2 frames on the way: on a slow link you get fewer frames, never growing lag
        if seq.wrapping_sub(acked.load(Ordering::SeqCst)) >= 2 {
            std::thread::sleep(Duration::from_millis(3));
            continue;
        }
        if unsafe { IsIconic(h) }.as_bool() {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        if let Some((cw, ch)) = unsafe { client_size(h) } {
            if (cw, ch) != enc.size() {
                wr.send(&Message::WindowOpen { id, title: title.to_string(), width: cw, height: ch })?;
                enc = wincodec::Encoder::new(cw, ch);
                grab = unsafe { Grab::new(cw, ch) }.ok_or_else(|| io::Error::new(io::ErrorKind::Other, "cannot capture the window"))?;
            }
        }
        if unsafe { grab.capture(h) } {
            let packets = enc.encode(grab.pixels());
            if !packets.is_empty() {
                for p in packets {
                    wr.send(&Message::WindowTiles { id, data: p })?;
                }
                seq = seq.wrapping_add(1);
                wr.send(&Message::WindowFrameEnd { id, seq })?;
                last_sent = Instant::now();
            }
        }
        if last_sent.elapsed() > Duration::from_secs(3) {
            wr.send(&Message::Ping)?;
            last_sent = Instant::now();
        }
        if let Some(rest) = frame_time.checked_sub(t0.elapsed()) {
            std::thread::sleep(rest);
        }
    }
    let _ = wr.send(&Message::WindowClose { id });
    wr.shutdown();
    Ok(())
}

unsafe fn focus(h: HWND) {
    if GetForegroundWindow() != h {
        let _ = SetForegroundWindow(h);
    }
}

/// Play input from the other computer's copy of the window back on the real window.
unsafe fn apply(h: HWND, ev: WindowEvent) {
    match ev {
        WindowEvent::Move { x, y } => {
            let mut p = POINT { x, y };
            let _ = ClientToScreen(h, &mut p);
            let _ = SetCursorPos(p.x, p.y);
        }
        WindowEvent::Button { button, pressed } => {
            if pressed {
                focus(h);
            }
            inject_win::inject(&InputEvent::MouseButton { button, pressed });
        }
        WindowEvent::Wheel { dx, dy } => inject_win::inject(&InputEvent::Scroll { dx: dx.clamp(-20, 20), dy: dy.clamp(-20, 20) }),
        WindowEvent::Key { code, pressed } => {
            if pressed {
                focus(h);
            }
            inject_win::inject(&InputEvent::Key { code, pressed });
        }
        WindowEvent::Resize { width, height } => {
            let (width, height) = (width.clamp(200, wincodec::MAX_SIDE) as i32, height.clamp(150, wincodec::MAX_SIDE) as i32);
            let (mut wr, mut cr) = (RECT::default(), RECT::default());
            if GetWindowRect(h, &mut wr).is_ok() && GetClientRect(h, &mut cr).is_ok() {
                if IsZoomed(h).as_bool() {
                    let _ = ShowWindow(h, SW_RESTORE);
                }
                let (dw, dh) = ((wr.right - wr.left) - (cr.right - cr.left), (wr.bottom - wr.top) - (cr.bottom - cr.top));
                let _ = SetWindowPos(h, None, 0, 0, width + dw, height + dh, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
            }
        }
        WindowEvent::Close => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captures a real window on this PC (the taskbar is always there) and checks the picture is not empty.
    #[test]
    fn captures_a_real_window() {
        unsafe {
            let Ok(h) = FindWindowW(windows::core::w!("Shell_TrayWnd"), None) else { return };
            let Some((w, ht)) = client_size(h) else { return };
            let g = Grab::new(w, ht).unwrap();
            assert!(g.capture(h), "PrintWindow worked");
            let px = g.pixels();
            assert!(px.chunks_exact(4).any(|p| p[0] != 0 || p[1] != 0 || p[2] != 0), "the picture has content");
            let mut enc = wincodec::Encoder::new(w, ht);
            let packets = enc.encode(px);
            let mut dec = wincodec::Decoder::new(w, ht).unwrap();
            for p in &packets {
                dec.apply(p).unwrap();
            }
            assert!(dec.buf.chunks_exact(4).zip(px.chunks_exact(4)).all(|(a, b)| a[..3] == b[..3]));
        }
        assert!(!list().is_empty() || std::env::var_os("CI").is_some());
    }
}
