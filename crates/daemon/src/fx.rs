//! The screen-change effects (Ripple, Glow, Sparks, Comet), drawn in software into a premultiplied BGRA picture.
//! Shared by Windows (fx_win: layered window) and Linux (fx_x11: see-through X11 window), so they look the same.
use kvm_protocol::{ease, Animation, Edge, Palette, Visual};

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

pub struct Canvas {
    pub w: i32,
    pub h: i32,
    pub px: Vec<u32>, // premultiplied BGRA
}

impl Canvas {
    pub fn new(w: i32, h: i32) -> Canvas {
        Canvas { w, h, px: vec![0; (w.max(0) * h.max(0)) as usize] }
    }

    pub fn clear(&mut self) {
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

/// Pick the on-screen rectangle (x0, y0, w, h) the effect needs.
pub fn region(a: &Animation, x: i32, y: i32, edge: Edge, bounds: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
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


/// Draw frame `t` (0..1) of the effect, with its origin at (ox, oy) inside the canvas.
pub fn draw(c: &mut Canvas, a: &Animation, t: f32, edge: Edge, ox: i32, oy: i32) {
    c.clear();
    match a.visual {
        Visual::Ripple => draw_ripple(c, a, t, ox, oy),
        Visual::Glow => draw_glow(c, a, t, edge, ox, oy),
        Visual::Sparks => draw_sparks(c, a, t, edge, ox, oy),
        Visual::Comet => draw_comet(c, a, t, edge, ox, oy),
        Visual::None => {}
    }
}

/// The point on `edge` at `frac` (0..1) along it, for a desktop spanning `bounds` (left, top, right, bottom).
pub fn edge_point(edge: Edge, frac: f32, bounds: (i32, i32, i32, i32)) -> (i32, i32) {
    let (l, t, r, b) = bounds;
    let f = frac.clamp(0.0, 1.0);
    let along_y = t + (f * (b - t - 1).max(0) as f32) as i32;
    let along_x = l + (f * (r - l - 1).max(0) as f32) as i32;
    match edge {
        Edge::Right => (r - 1, along_y),
        Edge::Left => (l, along_y),
        Edge::Top => (along_x, t),
        Edge::Bottom => (along_x, b - 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_effect_draws_something_and_ends_empty() {
        for v in [Visual::Ripple, Visual::Glow, Visual::Sparks, Visual::Comet] {
            let mut a = Animation::default();
            a.visual = v;
            let (x, y) = edge_point(Edge::Right, 0.5, (0, 0, 1920, 1080));
            let (x0, y0, w, h) = region(&a, x, y, Edge::Right, (0, 0, 1920, 1080));
            let mut c = Canvas::new(w, h);
            draw(&mut c, &a, 0.3, Edge::Right, x - x0, y - y0);
            assert!(c.px.iter().any(|p| p >> 24 > 0), "{v:?} is visible mid-way");
            draw(&mut c, &a, 1.0, Edge::Right, x - x0, y - y0);
            assert!(c.px.iter().all(|p| p >> 24 < 40), "{v:?} has faded out at the end");
        }
    }
}
