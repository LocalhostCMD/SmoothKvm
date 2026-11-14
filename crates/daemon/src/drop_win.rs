//! Carrying drags across the screen edge (Windows). While you drag something, an invisible catcher lies along
//! the screen edge that faces your other computer(s). Let go of files against that edge and they are sent there;
//! push an app window against it and, after a moment, that window appears on the other computer.
//!
//! It is a topmost, never-activating, (almost) fully transparent window that only exists during a drag and
//! only when there is somewhere to send to. Files arrive through WM_DROPFILES (Explorer's classic file drop), so no COM
//! drop target is needed. Later phases also accept *windows* dropped on it (see `window_dropped`).
use crate::store::{self, Identity};
use crate::{shared, xfer};
use kvm_protocol::Edge;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::core::{implement, w};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, POINTL, RECT, WPARAM};
use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
use windows::Win32::System::Ole::{IDropTarget, IDropTarget_Impl, OleInitialize, RegisterDragDrop, ReleaseStgMedium, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Thickness of the catcher, in pixels (the pointer sits on the last pixels while you push against the edge).
const THICK: i32 = 6;
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
    // OLE drag and drop (the drop target below) needs this thread to be an OLE apartment
    let _ = OleInitialize(None);
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
    // An OLE drop target sees the files as soon as a drag reaches the edge (not only when you let go), so a
    // drag can carry on to the computer controlling this one.
    let target: IDropTarget = Catcher { hwnd: hwnd.0 as isize }.into();
    if let Err(e) = RegisterDragDrop(hwnd, &target) {
        shared::warn(&format!("Dragging files across from here works only by letting go at the edge: {e}"));
    }
    // still accept drops from Explorer if SmoothKvm itself is ever run elevated
    for m in [WM_DROPFILES, WM_COPYDATA, 0x0049 /* WM_COPYGLOBALDATA */] {
        let _ = ChangeWindowMessageFilterEx(hwnd, m, MSGFLT_ALLOW, None);
    }
    // alpha 1 of 255: invisible, but still there for the mouse and for file drops (alpha 0 would let them through)
    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 1, LWA_ALPHA);
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
    /// A window being dragged by its title bar (and where it was when the drag started).
    moving: Option<(isize, RECT)>,
    /// Since when that window has been pushed against the edge.
    pushing: Option<Instant>,
    /// A window already sent across: put it back where it was once you let go of it.
    restore: Option<(isize, RECT)>,
}

/// The window Windows is moving right now (title-bar drag), if any.
unsafe fn window_being_moved() -> Option<HWND> {
    let mut gi = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
    GetGUIThreadInfo(0, &mut gi).ok()?;
    (gi.flags.0 & GUI_INMOVESIZE.0 != 0 && !gi.hwndMoveSize.0.is_null()).then_some(gi.hwndMoveSize)
}

static WATCH: Mutex<Option<Watch>> = Mutex::new(None);

unsafe fn tick(hwnd: HWND) {
    let mut g = match WATCH.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let w = g.get_or_insert_with(|| Watch { press: None, dragging: false, released_at: None, targets: vec![], targets_at: None, edge: Edge::Right, moving: None, pushing: None, restore: None });
    let down = GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000 != 0;
    let mut p = POINT::default();
    let _ = GetCursorPos(&mut p);
    if down {
        w.released_at = None;
        // remember a window being dragged by its title bar, and where it started
        if w.moving.is_none() {
            if let Some(mw) = window_being_moved() {
                let mut r = RECT::default();
                if GetWindowRect(mw, &mut r).is_ok() {
                    w.moving = Some((mw.0 as isize, r));
                    let _ = InvalidateRect(hwnd, None, true);
                }
            }
        }
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
        // a window pushed against the edge for a moment goes across without having to let go
        if w.dragging && w.restore.is_none() {
            if let (Some((mw, start)), Some(i)) = (w.moving, segment_at(hwnd, p)) {
                let wait = Duration::from_millis(store::load_config().switch_delay_ms.max(150) as u64);
                if w.pushing.get_or_insert_with(Instant::now).elapsed() >= wait {
                    let target = STRIP.lock().ok().and_then(|s| s.segments.get(i).map(|(t, _)| t.name.clone()));
                    w.moving = None;
                    w.pushing = None;
                    w.restore = Some((mw, start));
                    w.dragging = false;
                    hide(hwnd);
                    if let Some(ident) = IDENT.get().cloned() {
                        match crate::stream_win::start(mw as i64, target, ident) {
                            Ok(to) => shared::info(&format!("Showing that window on {to}")),
                            Err(e) => shared::warn(&format!("Could not show that window: {e}")),
                        }
                    }
                }
            } else {
                w.pushing = None;
            }
        }
    } else {
        w.press = None;
        w.pushing = None;
        // the window that went across is put back where it was on this screen
        if let Some((mw, start)) = w.restore.take() {
            let _ = SetWindowPos(HWND(mw as *mut _), None, start.left, start.top, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
        // a window was let go over the strip: show it on that computer, and put it back where it was
        if let Some((mw, start)) = w.moving.take() {
            if w.dragging {
                if let Some(i) = segment_at(hwnd, p) {
                    let target = STRIP.lock().ok().and_then(|s| s.segments.get(i).map(|(t, _)| t.name.clone()));
                    let mw_h = HWND(mw as *mut _);
                    let _ = SetWindowPos(mw_h, None, start.left, start.top, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
                    if let Some(ident) = IDENT.get().cloned() {
                        match crate::stream_win::start(mw as i64, target, ident) {
                            Ok(to) => shared::info(&format!("Showing that window on {to}")),
                            Err(e) => shared::warn(&format!("Could not show that window: {e}")),
                        }
                    }
                    w.dragging = false;
                    w.released_at = None;
                    hide(hwnd);
                    return;
                }
            }
        }
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
    // the whole edge
    let (x, y, w, h) = match edge {
        Edge::Right => (l + vw - THICK, t, THICK, vh),
        Edge::Left => (l, t, THICK, vh),
        Edge::Top => (l, t, vw, THICK),
        Edge::Bottom => (l, t + vh - THICK, vw, THICK),
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

/// Invisible: just validate the area.
unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let _ = BeginPaint(hwnd, &mut ps);
    let _ = EndPaint(hwnd, &ps);
}

/// The files in a drag (Explorer and most apps offer them as CF_HDROP).
unsafe fn drag_paths(data: &IDataObject) -> Vec<PathBuf> {
    let fmt = FORMATETC { cfFormat: CF_HDROP.0, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 };
    let Ok(mut medium) = data.GetData(&fmt) else { return vec![] };
    let h = HDROP(medium.u.hGlobal.0);
    let n = DragQueryFileW(h, u32::MAX, None);
    let mut out = Vec::new();
    for i in 0..n.min(10_000) {
        let len = DragQueryFileW(h, i, None) as usize;
        let mut buf = vec![0u16; len + 1];
        DragQueryFileW(h, i, Some(&mut buf));
        out.push(PathBuf::from(String::from_utf16_lossy(&buf[..len])));
    }
    ReleaseStgMedium(&mut medium);
    out
}

/// What the drag currently over the edge carries, and whether it already went on to the other computer.
static CAUGHT: Mutex<(Vec<PathBuf>, bool)> = Mutex::new((Vec::new(), false));

/// Files dragged to the edge that faces the computer controlling this one: hand the drag over to it. It takes
/// the mouse back and carries on dragging them there, so you can drop them into any folder on that computer.
fn carry_to_controller(paths: &[PathBuf]) -> bool {
    let Some(side) = shared::controller_side() else { return false };
    let facing = WATCH.lock().ok().and_then(|g| g.as_ref().map(|w| w.edge)).map(|e| e == side).unwrap_or(false);
    if !facing {
        return false;
    }
    let Some(target) = xfer::targets().into_iter().find(|t| t.kind == "controller") else { return false };
    let Some(ident) = IDENT.get().cloned() else { return false };
    let mut b = [0u8; 4];
    let _ = getrandom::getrandom(&mut b);
    let drag = u32::from_le_bytes(b);
    let names: Vec<String> = paths.iter().filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).collect();
    if names.is_empty() || !crate::send_to_controller(kvm_protocol::Message::DragStart { drag, names }) {
        return false;
    }
    match xfer::send_async_as(Some(target.name.clone()), paths.to_vec(), ident, Some(drag)) {
        Ok(_) => true,
        Err(e) => {
            shared::warn(&format!("Could not carry the files over: {e}"));
            false
        }
    }
}

#[implement(IDropTarget)]
struct Catcher {
    hwnd: isize,
}

impl IDropTarget_Impl for Catcher_Impl {
    fn DragEnter(&self, data: Option<&IDataObject>, _keys: MODIFIERKEYS_FLAGS, _pt: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        let paths = data.map(|d| unsafe { drag_paths(d) }).unwrap_or_default();
        let ok = !paths.is_empty();
        let carried = ok && carry_to_controller(&paths);
        if let Ok(mut c) = CAUGHT.lock() {
            *c = (paths, carried);
        }
        unsafe { *effect = if ok { DROPEFFECT_COPY } else { DROPEFFECT_NONE } };
        Ok(())
    }

    fn DragOver(&self, _keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        let ok = CAUGHT.lock().map(|c| !c.0.is_empty()).unwrap_or(false);
        unsafe {
            hover(HWND(self.hwnd as *mut _), POINT { x: pt.x, y: pt.y });
            *effect = if ok { DROPEFFECT_COPY } else { DROPEFFECT_NONE };
        }
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn Drop(&self, _data: Option<&IDataObject>, _keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        let (paths, carried) = CAUGHT.lock().map(|mut c| std::mem::take(&mut *c)).unwrap_or_default();
        let hwnd = HWND(self.hwnd as *mut _);
        unsafe {
            *effect = if paths.is_empty() { DROPEFFECT_NONE } else { DROPEFFECT_COPY };
            if carried || paths.is_empty() {
                // already on its way (the drag went on to the other computer): this is only the drag ending here
                hide(hwnd);
                return Ok(());
            }
            let mut wr = RECT::default();
            let _ = GetWindowRect(hwnd, &mut wr);
            send_dropped(hwnd, paths, POINT { x: pt.x - wr.left, y: pt.y - wr.top });
        }
        Ok(())
    }
}

/// Files let go on the edge: send them to the computer whose part of the edge they were dropped on.
unsafe fn send_dropped(hwnd: HWND, paths: Vec<PathBuf>, p: POINT) {
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
    send_dropped(hwnd, paths, p);
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
