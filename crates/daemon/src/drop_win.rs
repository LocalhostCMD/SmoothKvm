//! The drop strip (Windows): while you drag something, a strip slides in on the screen edge that faces
//! your other computer(s). Drop files on it and they are sent there.
//!
//! It is a topmost, never-activating window that only exists on screen during a drag and only when there
//! is somewhere to send to. Files arrive through WM_DROPFILES (Explorer's classic file drop), so no COM
//! drop target is needed. Later phases also accept *windows* dropped on it (see `window_dropped`).
use crate::store::{self, Identity};
use crate::{shared, xfer};
use kvm_protocol::Edge;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Thickness of the strip, in pixels.
const THICK: i32 = 84;
/// A press must last this long and move this far before it counts as a drag (so clicks never flash it).
const DRAG_DELAY: Duration = Duration::from_millis(250);
const DRAG_DIST: i32 = 24;

struct Strip {
    /// Where each target sits on screen.
    segments: Vec<(xfer::Target, RECT)>,
    hover: Option<usize>,
    shown: bool,
}

static STRIP: Mutex<Strip> = Mutex::new(Strip { segments: Vec::new(), hover: None, shown: false });
static IDENT: OnceLock<Arc<Identity>> = OnceLock::new();

pub fn start(ident: Arc<Identity>) {
    let _ = IDENT.set(ident);
    std::thread::spawn(|| unsafe { run() });
}

unsafe fn run() {
    let Ok(hmod) = GetModuleHandleW(None) else { return };
    let hinst = HINSTANCE(hmod.0);
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: hinst,
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        lpszClassName: w!("SmoothKvmDropStrip"),
        ..Default::default()
    };
    RegisterClassExW(&wc);
    let Ok(hwnd) = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_ACCEPTFILES,
        w!("SmoothKvmDropStrip"),
        w!("SmoothKvm - drop to send"),
        WS_POPUP,
        0,
        0,
        THICK,
        THICK,
        None,
        None,
        hinst,
        None,
    ) else {
        shared::warn("The drop strip could not be created; use /send instead");
        return;
    };
    DragAcceptFiles(hwnd, true);
    // still accept drops from Explorer if SmoothKvm itself is ever run elevated
    for m in [WM_DROPFILES, WM_COPYDATA, 0x0049 /* WM_COPYGLOBALDATA */] {
        let _ = ChangeWindowMessageFilterEx(hwnd, m, MSGFLT_ALLOW, None);
    }
    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 215, LWA_ALPHA);
    SetTimer(hwnd, 1, 40, None);
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

/// Watches the mouse (40 ms timer): show the strip during a drag, hide it after.
struct Watch {
    press: Option<(Instant, POINT)>,
    dragging: bool,
    released_at: Option<Instant>,
    targets: Vec<xfer::Target>,
    targets_at: Option<Instant>,
    edge: Edge,
}

static WATCH: Mutex<Option<Watch>> = Mutex::new(None);

unsafe fn tick(hwnd: HWND) {
    let mut g = match WATCH.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let w = g.get_or_insert_with(|| Watch { press: None, dragging: false, released_at: None, targets: vec![], targets_at: None, edge: Edge::Right });
    let down = GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000 != 0;
    let mut p = POINT::default();
    let _ = GetCursorPos(&mut p);
    if down {
        w.released_at = None;
        match w.press {
            None => w.press = Some((Instant::now(), p)),
            Some((t, at)) if !w.dragging => {
                let moved = (p.x - at.x).abs().max((p.y - at.y).abs()) >= DRAG_DIST;
                if moved && t.elapsed() >= DRAG_DELAY {
                    // refresh the target list at most every 2 s (it reads the config file)
                    if w.targets_at.map(|t| t.elapsed() > Duration::from_secs(2)).unwrap_or(true) {
                        w.targets = xfer::targets();
                        // While another computer controls this one, the strip goes on the side facing it, so
                        // you drag the file toward that computer (otherwise the side chosen in the settings).
                        w.edge = shared::controller_side().unwrap_or_else(|| store::load_config().drop_edge);
                        w.targets_at = Some(Instant::now());
                    }
                    if !w.targets.is_empty() {
                        w.dragging = true;
                        show(hwnd, &w.targets, w.edge);
                    }
                }
            }
            _ => {}
        }
    } else {
        w.press = None;
        if w.dragging {
            // keep it up a moment so the drop message (sent after the button goes up) still lands
            let r = *w.released_at.get_or_insert_with(Instant::now);
            if r.elapsed() > Duration::from_millis(400) {
                w.dragging = false;
                w.released_at = None;
                hide(hwnd);
            }
        }
    }
    if w.dragging {
        drop(g);
        hover(hwnd, p);
    }
}

unsafe fn show(hwnd: HWND, targets: &[xfer::Target], edge: Edge) {
    // the outer edge of the whole desktop (all monitors)
    let l = GetSystemMetrics(SM_XVIRTUALSCREEN);
    let t = GetSystemMetrics(SM_YVIRTUALSCREEN);
    let (vw, vh) = (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN));
    let vertical = matches!(edge, Edge::Left | Edge::Right);
    let len = ((if vertical { vh } else { vw }) as f32 * 0.6) as i32;
    let (x, y, w, h) = match edge {
        Edge::Right => (l + vw - THICK, t + (vh - len) / 2, THICK, len),
        Edge::Left => (l, t + (vh - len) / 2, THICK, len),
        Edge::Top => (l + (vw - len) / 2, t, len, THICK),
        Edge::Bottom => (l + (vw - len) / 2, t + vh - THICK, len, THICK),
    };
    let n = targets.len() as i32;
    let segments = targets
        .iter()
        .enumerate()
        .map(|(i, tg)| {
            let i = i as i32;
            let r = if vertical {
                RECT { left: 0, top: h * i / n, right: w, bottom: h * (i + 1) / n }
            } else {
                RECT { left: w * i / n, top: 0, right: w * (i + 1) / n, bottom: h }
            };
            (tg.clone(), r)
        })
        .collect();
    if let Ok(mut s) = STRIP.lock() {
        s.segments = segments;
        s.hover = None;
        s.shown = true;
    }
    let _ = SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    let _ = InvalidateRect(hwnd, None, true);
}

unsafe fn hide(hwnd: HWND) {
    if let Ok(mut s) = STRIP.lock() {
        s.shown = false;
        s.hover = None;
    }
    let _ = ShowWindow(hwnd, SW_HIDE);
}

/// Which segment the screen point is over, if any.
unsafe fn segment_at(hwnd: HWND, p: POINT) -> Option<usize> {
    let mut wr = RECT::default();
    GetWindowRect(hwnd, &mut wr).ok()?;
    let (x, y) = (p.x - wr.left, p.y - wr.top);
    let s = STRIP.lock().ok()?;
    if !s.shown {
        return None;
    }
    s.segments.iter().position(|(_, r)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
}

unsafe fn hover(hwnd: HWND, p: POINT) {
    let h = segment_at(hwnd, p);
    let changed = STRIP.lock().map(|mut s| std::mem::replace(&mut s.hover, h) != h).unwrap_or(false);
    if changed {
        let _ = InvalidateRect(hwnd, None, true);
    }
}

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let (segments, hover) = match STRIP.lock() {
        Ok(s) => (s.segments.clone(), s.hover),
        Err(_) => (vec![], None),
    };
    let edge = WATCH.lock().ok().and_then(|g| g.as_ref().map(|w| w.edge)).unwrap_or(Edge::Right);
    let vertical = matches!(edge, Edge::Left | Edge::Right);
    let font = CreateFontW(
        if vertical { 20 } else { 22 },
        0,
        if vertical { if edge == Edge::Right { 900 } else { 2700 } } else { 0 },
        0,
        FW_SEMIBOLD.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        CLEARTYPE_QUALITY.0 as u32,
        0,
        w!("Segoe UI"),
    );
    let old = SelectObject(hdc, font);
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(0x00FFFFFF));
    for (i, (t, r)) in segments.iter().enumerate() {
        let on = hover == Some(i);
        // BGR: accent blue normally, bright cyan under the cursor
        let brush = CreateSolidBrush(COLORREF(if on { 0x00FFC800 } else { 0x00F06D3B }));
        FillRect(hdc, r, brush);
        let _ = DeleteObject(brush);
        // a thin divider between segments
        if i > 0 {
            let line = CreateSolidBrush(COLORREF(0x00FFFFFF));
            let d = if vertical { RECT { left: r.left, top: r.top, right: r.right, bottom: r.top + 2 } } else { RECT { left: r.left, top: r.top, right: r.left + 2, bottom: r.bottom } };
            FillRect(hdc, &d, line);
            let _ = DeleteObject(line);
        }
        let arrow = match edge {
            Edge::Right => "→",
            Edge::Left => "←",
            Edge::Top => "↑",
            Edge::Bottom => "↓",
        };
        let label: Vec<u16> = format!("{arrow}  Drop to send to {}", shared::clean(&t.name)).encode_utf16().collect();
        if vertical {
            // rotated text is drawn from a start point, centred by measuring it first
            let mut sz = windows::Win32::Foundation::SIZE::default();
            let _ = GetTextExtentPoint32W(hdc, &label, &mut sz);
            let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
            let (x, y) = if edge == Edge::Right { (cx - sz.cy / 2, cy + sz.cx / 2) } else { (cx + sz.cy / 2, cy - sz.cx / 2) };
            let _ = TextOutW(hdc, x, y, &label);
        } else {
            let mut rr = *r;
            let mut label = label;
            DrawTextW(hdc, &mut label, &mut rr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        }
    }
    SelectObject(hdc, old);
    let _ = DeleteObject(font);
    let _ = EndPaint(hwnd, &ps);
}

unsafe fn dropped(hwnd: HWND, hdrop: HDROP) {
    let n = DragQueryFileW(hdrop, u32::MAX, None);
    let mut paths = Vec::new();
    for i in 0..n {
        let len = DragQueryFileW(hdrop, i, None) as usize;
        let mut buf = vec![0u16; len + 1];
        DragQueryFileW(hdrop, i, Some(&mut buf));
        paths.push(PathBuf::from(String::from_utf16_lossy(&buf[..len])));
    }
    let mut p = POINT::default();
    let _ = windows::Win32::UI::Shell::DragQueryPoint(hdrop, &mut p); // client coordinates
    DragFinish(hdrop);
    let target = STRIP.lock().ok().and_then(|s| {
        s.segments
            .iter()
            .find(|(_, r)| p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom)
            .or(s.segments.first())
            .map(|(t, _)| t.name.clone())
    });
    hide(hwnd);
    let Some(ident) = IDENT.get().cloned() else { return };
    match xfer::send_async(target, paths, ident) {
        Ok(t) => shared::info(&format!("Sending dropped files to {}", t.name)),
        Err(e) => shared::warn(&format!("Could not send the dropped files: {e}")),
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            tick(hwnd);
            LRESULT(0)
        }
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_DROPFILES => {
            dropped(hwnd, HDROP(wp.0 as _));
            LRESULT(0)
        }
        // never take focus away from what you are dragging from
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
