//! Windows: show an app window that runs on another computer (the other direction of `stream_win`).
//!
//! The picture arrives as tile packets (see wincodec) and is drawn in a normal window. Mouse, wheel and keys
//! used on that window go back to the real app; resizing it resizes the real window; closing it only stops
//! showing it. The network is read on its own thread, the window lives on the session's thread.
use crate::{shared, wincodec};
use kvm_protocol::secure::{SecureConn, SecureWriter};
use kvm_protocol::{Message, MouseButton, WindowEvent};
use std::collections::HashSet;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_NEWSIZE: u32 = WM_APP + 1;
const WM_GONE: u32 = WM_APP + 2;
const STYLE: WINDOW_STYLE = WS_OVERLAPPEDWINDOW;

fn err(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, m.to_string())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Everything the window procedure needs (owned by `run`, reached through GWLP_USERDATA).
struct Ctx {
    id: u32,
    pic: Arc<Mutex<wincodec::Decoder>>,
    wr: Arc<Mutex<SecureWriter>>,
    keys_down: HashSet<u16>,
    buttons_down: u32,
    last_move: Instant,
    pending_move: Option<(i32, i32)>,
    sent_pos: Option<(i32, i32)>,
    sizing: bool,
    /// set while we resize the window ourselves (to the real window's new size), so it isn't sent back
    own_resize: bool,
}

impl Ctx {
    fn send(&self, ev: WindowEvent) {
        if let Ok(mut w) = self.wr.lock() {
            let _ = w.send(&Message::WindowInput { id: self.id, ev });
        }
    }
    fn flush_move(&mut self) {
        if let Some((x, y)) = self.pending_move.take() {
            if self.sent_pos != Some((x, y)) {
                self.sent_pos = Some((x, y));
                self.last_move = Instant::now();
                self.send(WindowEvent::Move { x, y });
            }
        }
    }
    fn release_all(&mut self) {
        for code in self.keys_down.drain().collect::<Vec<_>>() {
            self.send(WindowEvent::Key { code, pressed: false });
        }
    }
}

fn xy(l: LPARAM) -> (i32, i32) {
    ((l.0 & 0xFFFF) as i16 as i32, ((l.0 >> 16) & 0xFFFF) as i16 as i32)
}

/// Make the window's client area exactly w x h.
unsafe fn fit(hwnd: HWND, w: u32, h: u32) {
    let mut r = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
    let _ = AdjustWindowRectEx(&mut r, STYLE, false, WINDOW_EX_STYLE(0));
    let _ = SetWindowPos(hwnd, None, 0, 0, r.right - r.left, r.bottom - r.top, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
}

unsafe fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut r = RECT::default();
    let _ = GetClientRect(hwnd, &mut r);
    ((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ctx;
    if p.is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let c = &mut *p;
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut ps);
            if let Ok(pic) = c.pic.lock() {
                let mut bi = BITMAPINFO::default();
                bi.bmiHeader = BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: pic.w as i32,
                    biHeight: -(pic.h as i32), // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                };
                // the paint region clips this to what actually changed
                SetDIBitsToDevice(dc, 0, 0, pic.w as u32, pic.h as u32, 0, 0, 0, pic.h as u32, pic.buf.as_ptr() as *const _, &bi, DIB_RGB_COLORS);
            }
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEMOVE => {
            c.pending_move = Some(xy(lp));
            if c.last_move.elapsed() >= Duration::from_millis(8) {
                c.flush_move();
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let (button, pressed) = match msg {
                WM_LBUTTONDOWN => (MouseButton::Left, true),
                WM_LBUTTONUP => (MouseButton::Left, false),
                WM_RBUTTONDOWN => (MouseButton::Right, true),
                WM_RBUTTONUP => (MouseButton::Right, false),
                WM_MBUTTONDOWN => (MouseButton::Middle, true),
                WM_MBUTTONUP => (MouseButton::Middle, false),
                _ => (if (wp.0 >> 16) & 0xFFFF == 1 { MouseButton::Back } else { MouseButton::Forward }, msg == WM_XBUTTONDOWN),
            };
            c.pending_move = Some(xy(lp));
            c.flush_move();
            c.send(WindowEvent::Button { button, pressed });
            // keep getting the mouse while a button is held, even outside the window (dragging)
            if pressed {
                if c.buttons_down == 0 {
                    SetCapture(hwnd);
                }
                c.buttons_down += 1;
            } else {
                c.buttons_down = c.buttons_down.saturating_sub(1);
                if c.buttons_down == 0 {
                    let _ = ReleaseCapture();
                }
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let d = ((wp.0 >> 16) & 0xFFFF) as i16 as i32 / 120;
            if d != 0 {
                c.flush_move();
                c.send(if msg == WM_MOUSEWHEEL { WindowEvent::Wheel { dx: 0, dy: d } } else { WindowEvent::Wheel { dx: d, dy: 0 } });
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => {
            let scan = ((lp.0 >> 16) & 0xFF) as u16;
            let ext = (lp.0 >> 24) & 1 == 1;
            let pressed = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            if let Some(code) = crate::capture_win::scan_to_hid(scan, ext) {
                if pressed {
                    c.keys_down.insert(code);
                } else {
                    c.keys_down.remove(&code);
                }
                c.send(WindowEvent::Key { code, pressed });
            }
            LRESULT(0) // every key goes to the app over there (no menu beeps, no Alt+F4 here)
        }
        WM_SYSCHAR => LRESULT(0),
        WM_KILLFOCUS => {
            c.release_all();
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_ENTERSIZEMOVE => {
            c.sizing = true;
            LRESULT(0)
        }
        WM_EXITSIZEMOVE => {
            c.sizing = false;
            let (w, h) = client_size(hwnd);
            let same = c.pic.lock().map(|p| (p.w as u32, p.h as u32) == (w, h)).unwrap_or(true);
            if !same && w >= 64 && h >= 64 {
                c.send(WindowEvent::Resize { width: w, height: h });
            }
            LRESULT(0)
        }
        WM_SIZE => {
            // maximised / restored (dragging the border is handled when the drag ends)
            if !c.sizing && !c.own_resize && (wp.0 as u32 == SIZE_MAXIMIZED || wp.0 as u32 == SIZE_RESTORED) {
                let (w, h) = ((lp.0 & 0xFFFF) as u32, ((lp.0 >> 16) & 0xFFFF) as u32);
                let same = c.pic.lock().map(|p| (p.w as u32, p.h as u32) == (w, h)).unwrap_or(true);
                if !same && w >= 64 && h >= 64 {
                    c.send(WindowEvent::Resize { width: w, height: h });
                }
            }
            LRESULT(0)
        }
        // the real window has a new size: follow it
        WM_NEWSIZE => {
            if !IsZoomed(hwnd).as_bool() {
                c.own_resize = true;
                fit(hwnd, wp.0 as u32, lp.0 as u32);
                c.own_resize = false;
            }
            let _ = InvalidateRect(hwnd, None, true);
            LRESULT(0)
        }
        WM_GONE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_CLOSE => {
            c.release_all();
            c.send(WindowEvent::Close);
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

pub fn run(conn: SecureConn, peer: &str, id: u32, title: String, w: u32, h: u32) -> io::Result<()> {
    conn.set_timeouts(Some(Duration::from_secs(15)), Some(Duration::from_secs(10)))?;
    let (mut rd, wr) = conn.split()?;
    let wr = Arc::new(Mutex::new(wr));
    let pic = Arc::new(Mutex::new(wincodec::Decoder::new(w, h)?));
    let hwnd = unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None).map_err(|e| err(&e.to_string()))?.into();
        let class = wide("SmoothKvmAppView");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: PCWSTR(class.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc); // (fails harmlessly when it is already registered)
        let t = wide(&format!("{} - on {peer} (SmoothKvm)", shared::clean(&title)));
        let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), PCWSTR(class.as_ptr()), PCWSTR(t.as_ptr()), STYLE, CW_USEDEFAULT, CW_USEDEFAULT, 640, 480, None, None, hinst, None)
            .map_err(|e| err(&format!("could not open a window: {e}")))?;
        fit(hwnd, w, h);
        hwnd
    };
    let mut ctx = Box::new(Ctx {
        id,
        pic: pic.clone(),
        wr: wr.clone(),
        keys_down: HashSet::new(),
        buttons_down: 0,
        last_move: Instant::now(),
        pending_move: None,
        sent_pos: None,
        sizing: false,
        own_resize: false,
    });
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, &mut *ctx as *mut Ctx as isize);
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(hwnd);
    }
    shared::info(&format!("\"{}\" from {peer} is shown here", shared::clean(&title)));

    // network -> picture (and "repaint this part"), on its own thread
    let raw = hwnd.0 as isize;
    let (pic2, wr2, peer2) = (pic.clone(), wr.clone(), peer.to_string());
    std::thread::spawn(move || {
        let hwnd = HWND(raw as *mut _);
        let mut dirty: Vec<(usize, usize, usize, usize)> = Vec::new();
        loop {
            match rd.recv() {
                Ok(Message::WindowTiles { data, .. }) => match pic2.lock().map(|mut p| p.apply(&data)) {
                    Ok(Ok(r)) => dirty.extend(r),
                    _ => break,
                },
                Ok(Message::WindowFrameEnd { seq, .. }) => {
                    for (x, y, w, h) in dirty.drain(..) {
                        let r = RECT { left: x as i32, top: y as i32, right: (x + w) as i32, bottom: (y + h) as i32 };
                        unsafe {
                            let _ = InvalidateRect(hwnd, Some(&r), false);
                        }
                    }
                    if wr2.lock().map(|mut w| w.send(&Message::WindowAck { id, seq }).is_err()).unwrap_or(true) {
                        break;
                    }
                }
                // the real window changed size: start a new picture of that size
                Ok(Message::WindowOpen { width, height, .. }) => match wincodec::Decoder::new(width, height) {
                    Ok(d) => {
                        if let Ok(mut p) = pic2.lock() {
                            *p = d;
                        }
                        dirty.clear();
                        unsafe {
                            let _ = PostMessageW(hwnd, WM_NEWSIZE, WPARAM(width as usize), LPARAM(height as isize));
                        }
                    }
                    Err(_) => break,
                },
                Ok(Message::WindowClose { .. }) | Err(_) => {
                    shared::info(&format!("The window from {peer2} was closed there"));
                    break;
                }
                Ok(_) => {}
            }
        }
        rd.shutdown();
        unsafe {
            let _ = PostMessageW(hwnd, WM_GONE, WPARAM(0), LPARAM(0));
        }
    });

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
            // a mouse move held back by the 8 ms limit goes out once the queue is empty
            if PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE).as_bool() {
                continue;
            }
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ctx;
            if !p.is_null() {
                (*p).flush_move();
            }
        }
    }
    drop(ctx);
    if let Ok(w) = wr.lock() {
        w.shutdown();
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kvm_protocol::secure;
    use std::net::{TcpListener, TcpStream};

    /// End to end over an encrypted loopback connection, with a real window: a frame goes in and is on
    /// screen, a click on the window comes back, the window follows a new size, and closing ends cleanly.
    #[test]
    fn real_window_viewer() {
        let (a_priv, _) = secure::generate_keypair().unwrap();
        let (b_priv, _) = secure::generate_keypair().unwrap();
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let (w, h) = (200u32, 120u32);
        let viewer = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut conn = secure::accept(s, &b_priv, None).unwrap().conn;
            let Message::WindowOpen { id, title, width, height } = conn.recv().unwrap() else { panic!() };
            run(conn, "test", id, title, width, height)
        });
        let mut conn = secure::connect(TcpStream::connect(addr).unwrap(), &a_priv, None).unwrap().conn;
        conn.send(&Message::WindowOpen { id: 7, title: "Viewer test".into(), width: w, height: h }).unwrap();
        let mut frame = vec![0u8; (w * h * 4) as usize];
        for px in frame.chunks_exact_mut(4) {
            px.copy_from_slice(&[0x20, 0x80, 0xF0, 0xFF]);
        }
        let mut enc = wincodec::Encoder::new(w, h);
        for p in enc.encode(&frame) {
            conn.send(&Message::WindowTiles { id: 7, data: p }).unwrap();
        }
        conn.send(&Message::WindowFrameEnd { id: 7, seq: 1 }).unwrap();
        conn.set_timeouts(Some(Duration::from_secs(5)), None).unwrap();
        let next = |conn: &mut SecureConn| conn.recv().unwrap();
        assert!(matches!(next(&mut conn), Message::WindowAck { seq: 1, .. }), "the viewer showed the frame");
        std::thread::sleep(Duration::from_millis(400));
        let hwnd = unsafe { FindWindowW(PCWSTR(wide("SmoothKvmAppView").as_ptr()), PCWSTR(wide("Viewer test - on test (SmoothKvm)").as_ptr())) }.expect("viewer window exists");
        unsafe {
            assert_eq!(client_size(hwnd), (w, h));
            let dc = GetDC(hwnd);
            let px = GetPixel(dc, 100, 60);
            ReleaseDC(hwnd, dc);
            assert_eq!(px.0, 0x002080F0, "the pixel in the window is the streamed colour");
            // a click at (30, 40) comes back as a move and a button press
            PostMessageW(hwnd, WM_LBUTTONDOWN, WPARAM(1), LPARAM((40 << 16) | 30)).unwrap();
            PostMessageW(hwnd, WM_LBUTTONUP, WPARAM(0), LPARAM((40 << 16) | 30)).unwrap();
        }
        assert!(matches!(next(&mut conn), Message::WindowInput { id: 7, ev: WindowEvent::Move { x: 30, y: 40 } }));
        assert!(matches!(next(&mut conn), Message::WindowInput { ev: WindowEvent::Button { button: MouseButton::Left, pressed: true }, .. }));
        assert!(matches!(next(&mut conn), Message::WindowInput { ev: WindowEvent::Button { button: MouseButton::Left, pressed: false }, .. }));
        // the real window got bigger: the viewer follows, and does not send that size back as a request
        conn.send(&Message::WindowOpen { id: 7, title: "Viewer test".into(), width: 260, height: 150 }).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        unsafe { assert_eq!(client_size(hwnd), (260, 150)) };
        conn.send(&Message::WindowClose { id: 7 }).unwrap();
        assert!(viewer.join().unwrap().is_ok());
    }
}
