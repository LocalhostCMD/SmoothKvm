//! On-screen effects (Windows): colourful, purely visual, and they never move or capture the mouse.
//!
//! Each effect is drawn by software into a small per-pixel-alpha layered window that is topmost,
//! click-through and never takes focus, lives for the length of the effect, then disappears.
use kvm_protocol::{ease, Animation, Edge, Palette, Visual};
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

/// Preview in the middle of the right edge of the primary screen (used by the "Preview on screen" button).
pub fn preview(a: Animation) {
    unsafe {
        let w = GetSystemMetrics(SM_CXSCREEN);
        let h = GetSystemMetrics(SM_CYSCREEN);
        play(a, w - 1, h / 2, Edge::Right, (0, 0, w, h));
    }
}

// ---- colours ---------------------------------------------------------------

type Rgb = (f32, f32, f32);

fn hsv(h: f32, s: f32, v: f32) -> Rgb {
    let h = (h.rem_euclid(1.0)) * 6.0;
    let i = h.floor();
    let f = h - i;
    let (p, q, t) = (v * (1.0 - s), v * (1.0 - s * f), v * (1.0 - s * (1.0 - f)));
    match i as i32 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

fn stops(list: &[Rgb], u: f32) -> Rgb {
    let u = u.rem_euclid(1.0) * list.len() as f32;
    let i = u.floor() as usize % list.len();
    let f = u - u.floor();
    let (a, b) = (list[i], list[(i + 1) % list.len()]);
    (a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f, a.2 + (b.2 - a.2) * f)
}

fn pal(p: Palette, u: f32) -> Rgb {
    match p {
        Palette::Rainbow => hsv(u, 0.78, 1.0),
        Palette::Ocean => stops(&[(0.0, 0.6, 1.0), (0.0, 0.9, 0.8), (0.47, 0.4, 1.0)], u),
        Palette::Sunset => stops(&[(1.0, 0.47, 0.16), (1.0, 0.24, 0.47), (0.59, 0.24, 0.86)], u),
        Palette::Neon => stops(&[(1.0, 0.16, 0.63), (0.16, 0.94, 1.0), (0.59, 1.0, 0.24)], u),
        Palette::Mono => stops(&[(1.0, 1.0, 1.0), (0.59, 0.75, 1.0)], u),
    }
}

// ---- pixel buffer ------------------------------------------------------------

struct Canvas {
    w: i32,
    h: i32,
    px: Vec<u32>, // premultiplied BGRA
}

impl Canvas {
    fn clear(&mut self) {
        self.px.iter_mut().for_each(|p| *p = 0);
    }

    /// Composite a colour with alpha `a` (0..1) over the existing pixel (premultiplied "over").
    #[inline]
    fn blend(&mut self, x: i32, y: i32, c: Rgb, a: f32) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h || a <= 0.003 {
            return;
        }
        let a = a.min(1.0);
        let i = (y * self.w + x) as usize;
        let d = self.px[i];
        let (da, dr, dg, db) = ((d >> 24) as f32, ((d >> 16) & 255) as f32, ((d >> 8) & 255) as f32, (d & 255) as f32);
        let k = 1.0 - a;
        let na = (a * 255.0 + da * k).min(255.0);
        let nr = (c.0 * a * 255.0 + dr * k).min(255.0);
        let ng = (c.1 * a * 255.0 + dg * k).min(255.0);
        let nb = (c.2 * a * 255.0 + db * k).min(255.0);
        self.px[i] = ((na as u32) << 24) | ((nr as u32) << 16) | ((ng as u32) << 8) | nb as u32;
    }
}

/// Pseudo-random in 0..1 from an integer seed (deterministic, so an effect looks the same each frame).
fn rnd(seed: u32) -> f32 {
    let mut x = seed.wrapping_mul(0x9E37_79B1) ^ 0x85EB_CA6B;
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    ((x & 0xFFFF) as f32) / 65535.0
}

/// Position in "edge space": (distance in from the screen edge, distance along the edge from the origin).
fn edge_space(edge: Edge, c: &Canvas, ox: i32, oy: i32, ix: i32, iy: i32) -> (f32, f32) {
    match edge {
        Edge::Right => ((c.w - 1 - ix) as f32, (iy - oy) as f32),
        Edge::Left => (ix as f32, (iy - oy) as f32),
        Edge::Top => (iy as f32, (ix - ox) as f32),
        Edge::Bottom => ((c.h - 1 - iy) as f32, (ix - ox) as f32),
    }
}

// ---- the effects ----------------------------------------------------------------

fn draw_ripple(c: &mut Canvas, a: &Animation, t: f32, ox: i32, oy: i32) {
    let size = a.size_px as f32;
    for k in 0..4 {
        let p = (t * 1.25 - k as f32 * 0.13).clamp(0.0, 1.0);
        if p <= 0.0 || p >= 1.0 {
            continue;
        }
        let radius = size * 0.5 * ease(a, p).max(0.0);
        let thick = 3.0 + 11.0 * (1.0 - p);
        let alpha = (1.0 - p).powf(1.15);
        let col = pal(a.palette, k as f32 / 4.0 + t * 0.6);
        let reach = (radius + thick + 2.0) as i32;
        for iy in (oy - reach).max(0)..(oy + reach).min(c.h) {
            for ix in (ox - reach).max(0)..(ox + reach).min(c.w) {
                let d = (((ix - ox) * (ix - ox) + (iy - oy) * (iy - oy)) as f32).sqrt();
                let edge = 1.0 - ((d - radius).abs() / thick).min(1.0);
                if edge > 0.0 {
                    c.blend(ix, iy, col, edge * edge * alpha);
                }
            }
        }
    }
    // a quick soft flash at the centre
    let flash = (1.0 - t * 3.0).max(0.0);
    if flash > 0.0 {
        let r = size * 0.18;
        for iy in (oy - r as i32).max(0)..(oy + r as i32).min(c.h) {
            for ix in (ox - r as i32).max(0)..(ox + r as i32).min(c.w) {
                let d = (((ix - ox) * (ix - ox) + (iy - oy) * (iy - oy)) as f32).sqrt() / r;
                if d < 1.0 {
                    c.blend(ix, iy, (1.0, 1.0, 1.0), (1.0 - d).powi(2) * flash * 0.9);
                }
            }
        }
    }
}

fn draw_glow(c: &mut Canvas, a: &Animation, t: f32, edge: Edge, ox: i32, oy: i32) {
    let size = a.size_px as f32;
    let env = (std::f32::consts::PI * t).sin().powf(0.8); // swells, then fades
    let spread = size * (0.55 + 0.5 * ease(a, t).clamp(0.0, 1.4));
    for iy in 0..c.h {
        for ix in 0..c.w {
            let (depth, along) = edge_space(edge, c, ox, oy, ix, iy);
            let g_depth = (-(depth / (size * 0.28)).powi(2)).exp();
            let g_along = (-(along / spread).powi(2)).exp();
            let alpha = g_depth * g_along * env * 0.95;
            if alpha > 0.01 {
                let col = pal(a.palette, along.abs() / (size * 2.0) + t * 0.8);
                c.blend(ix, iy, col, alpha);
            }
        }
    }
}

fn draw_sparks(c: &mut Canvas, a: &Animation, t: f32, edge: Edge, ox: i32, oy: i32) {
    let size = a.size_px as f32;
    let (dx, dy) = match edge {
        Edge::Right => (-1.0f32, 0.0f32),
        Edge::Left => (1.0, 0.0),
        Edge::Top => (0.0, 1.0),
        Edge::Bottom => (0.0, -1.0),
    };
    let e = ease(a, t).max(0.0);
    for i in 0..64u32 {
        let ang = (rnd(i * 3 + 1) - 0.5) * 2.4; // fan of about +-70 degrees around "into the screen"
        let (s, co) = ang.sin_cos();
        let (vx, vy) = (dx * co - dy * s, dx * s + dy * co);
        let speed = size * (0.55 + 0.9 * rnd(i * 3 + 2));
        let grav = size * 0.35 * t * t;
        let x = ox as f32 + vx * speed * e;
        let y = oy as f32 + vy * speed * e + grav;
        let r = (2.0 + 5.0 * rnd(i * 3 + 3)) * (1.0 - t * 0.6);
        let alpha = (1.0 - t).powf(0.9);
        let col = pal(a.palette, rnd(i * 7 + 5) + t * 0.3);
        let reach = r.ceil() as i32 + 1;
        for iy in (y as i32 - reach)..=(y as i32 + reach) {
            for ix in (x as i32 - reach)..=(x as i32 + reach) {
                let d = (((ix as f32 - x).powi(2) + (iy as f32 - y).powi(2)) as f32).sqrt();
                let soft = (1.0 - (d - r + 1.0).max(0.0)).clamp(0.0, 1.0);
                if soft > 0.0 {
                    c.blend(ix, iy, col, soft * alpha);
                }
            }
        }
    }
}

fn draw_comet(c: &mut Canvas, a: &Animation, t: f32, edge: Edge, ox: i32, oy: i32) {
    let size = a.size_px as f32;
    let span = size * 2.2;
    let tail = size * 1.3;
    let head = (0.5 * t + 0.5 * ease(a, t).clamp(0.0, 1.0) - 0.5) * 2.0 * span; // steady travel along the edge, flavoured by the motion
    let fade = (1.0 - t).powf(0.6);
    for iy in 0..c.h {
        for ix in 0..c.w {
            let (depth, along) = edge_space(edge, c, ox, oy, ix, iy);
            let behind = head - along; // >0 when this pixel is behind the head
            if !(0.0..=tail).contains(&behind) && behind.abs() > size * 0.12 {
                continue;
            }
            let width = size * (0.05 + 0.04 * (1.0 - behind.clamp(0.0, tail) / tail));
            let across = (-(depth / (width * 2.4)).powi(2)).exp();
            let trail = if behind >= 0.0 { (1.0 - behind / tail).powi(2) } else { 0.0 };
            let headglow = (-((behind / (size * 0.07)).powi(2) + (depth / (size * 0.1)).powi(2))).exp();
            let alpha = (across * trail + headglow) * fade;
            if alpha > 0.01 {
                let col = if headglow > 0.5 { (1.0, 1.0, 1.0) } else { pal(a.palette, behind / tail + t) };
                c.blend(ix, iy, col, alpha);
            }
        }
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

/// Pick the on-screen rectangle (x0, y0, w, h) the effect needs.
fn region(a: &Animation, x: i32, y: i32, edge: Edge, bounds: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    let s = a.size_px as i32;
    let (l, t, r, b) = bounds;
    match a.visual {
        Visual::Ripple | Visual::Sparks => {
            let half = if a.visual == Visual::Ripple { s * 3 / 4 } else { s * 2 };
            (x - half, y - half, half * 2, half * 2)
        }
        Visual::Glow | Visual::Comet => {
            let depth = s;
            let len = s * 5;
            match edge {
                Edge::Right => (r - depth, y - len / 2, depth, len),
                Edge::Left => (l, y - len / 2, depth, len),
                Edge::Top => (x - len / 2, t, len, depth),
                Edge::Bottom => (x - len / 2, b - depth, len, depth),
            }
        }
        Visual::None => (x, y, 1, 1),
    }
}

unsafe fn run(a: Animation, x: i32, y: i32, edge: Edge, bounds: (i32, i32, i32, i32)) {
    let (x0, y0, w, h) = region(&a, x, y, edge, bounds);
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

    let mut canvas = Canvas { w, h, px: vec![0; (w * h) as usize] };
    let (ox, oy) = (x - x0, y - y0);
    let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
    let (pos, size, src) = (POINT { x: x0, y: y0 }, SIZE { cx: w, cy: h }, POINT { x: 0, y: 0 });
    let dur = Duration::from_millis(a.duration_ms as u64);
    let start = Instant::now();
    loop {
        let t = (start.elapsed().as_secs_f32() / dur.as_secs_f32()).min(1.0);
        canvas.clear();
        match a.visual {
            Visual::Ripple => draw_ripple(&mut canvas, &a, t, ox, oy),
            Visual::Glow => draw_glow(&mut canvas, &a, t, edge, ox, oy),
            Visual::Sparks => draw_sparks(&mut canvas, &a, t, edge, ox, oy),
            Visual::Comet => draw_comet(&mut canvas, &a, t, edge, ox, oy),
            Visual::None => {}
        }
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
