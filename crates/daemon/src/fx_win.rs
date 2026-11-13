//! On-screen effects (Windows): colourful, purely visual, and they never move or capture the mouse.
//!
//! Each effect is drawn by software into a small per-pixel-alpha layered window that is topmost,
//! click-through and never takes focus, lives for the length of the effect, then disappears.
use crate::fx::{self, Canvas};
use kvm_protocol::{Animation, Edge, Visual};
use std::ffi::c_void;
use std::sync::Once;
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Start an effect at screen position (x, y) on the given edge. Returns immediately.
pub fn play(a: Animation, x: i32, y: i32, edge: Edge, bounds: (i32, i32, i32, i32)) {
    if a.visual == Visual::None || a.duration_ms == 0 {
        return;
    }
    std::thread::spawn(move || unsafe { run(a, x, y, edge, bounds) });
}

/// Play the effect where the pointer arrives on (or leaves by) `edge` of this PC's desktop, `frac` along it.
pub fn play_at_edge(a: Animation, edge: Edge, frac: f32) {
    let bounds = unsafe {
        let (l, t) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
        (l, t, l + GetSystemMetrics(SM_CXVIRTUALSCREEN), t + GetSystemMetrics(SM_CYVIRTUALSCREEN))
    };
    let (x, y) = fx::edge_point(edge, frac, bounds);
    play(a, x, y, edge, bounds);
}

/// Play the effect on `edge` of this PC's desktop at the height (or width) where the mouse pointer is now.
pub fn play_at_pointer_edge(a: Animation, edge: Edge) {
    let frac = unsafe {
        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p);
        let (l, t) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
        let (w, h) = (GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1), GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1));
        match edge {
            Edge::Left | Edge::Right => (p.y - t) as f32 / h as f32,
            Edge::Top | Edge::Bottom => (p.x - l) as f32 / w as f32,
        }
    };
    play_at_edge(a, edge, frac);
}

/// Preview in the middle of the right edge of the primary screen (used by the "Preview on screen" button).
pub fn preview(a: Animation) {
    unsafe {
        let w = GetSystemMetrics(SM_CXSCREEN);
        let h = GetSystemMetrics(SM_CYSCREEN);
        play(a, w - 1, h / 2, Edge::Right, (0, 0, w, h));
    }
}

// ---- the overlay window -----------------------------------------------------------

unsafe extern "system" fn wndproc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    DefWindowProcW(h, m, wp, lp)
}

fn class_name() -> PCWSTR {
    static REG: Once = Once::new();
    REG.call_once(|| unsafe {
        let hinst = GetModuleHandleW(None).map(|m| HINSTANCE(m.0)).unwrap_or_default();
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst, lpszClassName: w!("SmoothKvmFx"), ..Default::default() };
        RegisterClassW(&wc);
    });
    w!("SmoothKvmFx")
}

unsafe fn run(a: Animation, x: i32, y: i32, edge: Edge, bounds: (i32, i32, i32, i32)) {
    let (x0, y0, w, h) = fx::region(&a, x, y, edge, bounds);
    if w <= 0 || h <= 0 || w > 4000 || h > 4000 {
        return;
    }
    let hinst = GetModuleHandleW(None).map(|m| HINSTANCE(m.0)).unwrap_or_default();
    let hwnd = match CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        class_name(),
        w!("SmoothKvm effect"),
        WS_POPUP,
        x0,
        y0,
        w,
        h,
        HWND::default(),
        HMENU::default(),
        hinst,
        None,
    ) {
        Ok(h) => h,
        Err(_) => return,
    };
    let screen_dc = GetDC(HWND::default());
    let mem_dc = CreateCompatibleDC(screen_dc);
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let bmp = match CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
        Ok(b) if !bits.is_null() => b,
        _ => {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(HWND::default(), screen_dc);
            let _ = DestroyWindow(hwnd);
            return;
        }
    };
    let old = SelectObject(mem_dc, HGDIOBJ(bmp.0));
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

    let mut canvas = Canvas::new(w, h);
    let (ox, oy) = (x - x0, y - y0);
    let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
    let (pos, size, src) = (POINT { x: x0, y: y0 }, SIZE { cx: w, cy: h }, POINT { x: 0, y: 0 });
    let dur = Duration::from_millis(a.duration_ms as u64);
    let start = Instant::now();
    loop {
        let t = (start.elapsed().as_secs_f32() / dur.as_secs_f32()).min(1.0);
        fx::draw(&mut canvas, &a, t, edge, ox, oy);
        std::ptr::copy_nonoverlapping(canvas.px.as_ptr(), bits as *mut u32, canvas.px.len());
        let _ = UpdateLayeredWindow(hwnd, screen_dc, Some(&pos), Some(&size), mem_dc, Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA);
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if t >= 1.0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    SelectObject(mem_dc, old);
    let _ = DeleteObject(HGDIOBJ(bmp.0));
    let _ = DeleteDC(mem_dc);
    ReleaseDC(HWND::default(), screen_dc);
    let _ = DestroyWindow(hwnd);
}
