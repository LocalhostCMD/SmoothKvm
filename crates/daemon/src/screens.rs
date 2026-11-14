//! This computer's monitors: the size of the whole desktop and where each monitor is in it.
//! Sent to the controlling computer (it draws them on its map) and shown on this computer's own map.
use crate::shared::ScreenShape;

/// Monitors as (x, y, width, height) relative to the desktop's top-left, plus the desktop's size.
fn from_rects(mut rects: Vec<[i32; 4]>) -> ScreenShape {
    rects.retain(|r| r[2] > 0 && r[3] > 0);
    rects.truncate(16);
    if rects.is_empty() {
        rects.push([0, 0, 1920, 1080]);
    }
    let l = rects.iter().map(|r| r[0]).min().unwrap_or(0);
    let t = rects.iter().map(|r| r[1]).min().unwrap_or(0);
    let r = rects.iter().map(|r| r[0] + r[2]).max().unwrap_or(1920);
    let b = rects.iter().map(|r| r[1] + r[3]).max().unwrap_or(1080);
    let monitors = rects.iter().map(|m| [m[0] - l, m[1] - t, m[2], m[3]]).collect();
    ScreenShape { width: (r - l).clamp(100, 20000) as u32, height: (b - t).clamp(100, 20000) as u32, monitors }
}

#[cfg(windows)]
pub fn layout() -> ScreenShape {
    use windows::Win32::Foundation::{BOOL, LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
    unsafe extern "system" fn each(_m: HMONITOR, _dc: HDC, r: *mut RECT, data: LPARAM) -> BOOL {
        let v = &mut *(data.0 as *mut Vec<[i32; 4]>);
        let r = &*r;
        v.push([r.left, r.top, r.right - r.left, r.bottom - r.top]);
        BOOL(1)
    }
    let mut v: Vec<[i32; 4]> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut v as *mut _ as isize));
    }
    from_rects(v)
}

#[cfg(unix)]
pub fn layout() -> ScreenShape {
    use std::process::{Command, Stdio};
    // KDE Plasma (Wayland or X11): exact layout in logical pixels
    if let Ok(o) = Command::new("kscreen-doctor").arg("-j").stdin(Stdio::null()).stderr(Stdio::null()).output() {
        if let Some(r) = parse_kscreen(&o.stdout) {
            return from_rects(r);
        }
    }
    // anything else with X or XWayland
    if let Ok(o) = Command::new("xrandr").arg("--listmonitors").stdin(Stdio::null()).stderr(Stdio::null()).output() {
        let r = parse_xrandr(&String::from_utf8_lossy(&o.stdout));
        if !r.is_empty() {
            return from_rects(r);
        }
    }
    from_rects(vec![])
}

#[cfg(any(unix, test))]
fn parse_kscreen(json: &[u8]) -> Option<Vec<[i32; 4]>> {
    let v: serde_json::Value = serde_json::from_slice(json).ok()?;
    let mut out = Vec::new();
    for o in v["outputs"].as_array()? {
        if o["enabled"].as_bool() != Some(true) || o["connected"].as_bool() == Some(false) {
            continue;
        }
        let (x, y) = (o["pos"]["x"].as_i64()?, o["pos"]["y"].as_i64()?);
        // the size on the desktop: the current mode, scaled, turned if the output is rotated
        let cur = o["currentModeId"].as_str().map(String::from).or_else(|| o["currentModeId"].as_i64().map(|i| i.to_string()))?;
        let mode = o["modes"].as_array()?.iter().find(|m| m["id"].as_str().map(String::from).or_else(|| m["id"].as_i64().map(|i| i.to_string())) == Some(cur.clone()))?;
        let (mut w, mut h) = (mode["size"]["width"].as_f64()?, mode["size"]["height"].as_f64()?);
        let scale = o["scale"].as_f64().filter(|s| *s > 0.1).unwrap_or(1.0);
        if matches!(o["rotation"].as_i64(), Some(2) | Some(8)) {
            std::mem::swap(&mut w, &mut h);
        }
        out.push([x as i32, y as i32, (w / scale).round() as i32, (h / scale).round() as i32]);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Lines like ` 0: +*DP-1 2560/597x1440/336+0+0  DP-1`.
#[cfg(any(unix, test))]
fn parse_xrandr(s: &str) -> Vec<[i32; 4]> {
    s.lines()
        .filter_map(|l| {
            let geo = l.split_whitespace().nth(2)?;
            let (w, rest) = geo.split_once('/')?;
            let (_, rest) = rest.split_once('x')?;
            let (h, rest) = rest.split_once('/')?;
            let mut it = rest.splitn(3, '+');
            let _ = it.next()?;
            Some([it.next()?.parse().ok()?, it.next()?.parse().ok()?, w.parse().ok()?, h.parse().ok()?])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_spans_all_monitors() {
        let s = from_rects(vec![[-1920, 0, 1920, 1080], [0, -200, 2560, 1440]]);
        assert_eq!((s.width, s.height), (4480, 1440));
        assert_eq!(s.monitors[0], [0, 200, 1920, 1080]);
        assert_eq!(s.monitors[1], [1920, 0, 2560, 1440]);
        assert_eq!(from_rects(vec![]).monitors.len(), 1, "never empty");
    }

    #[test]
    fn reads_xrandr_and_kscreen() {
        let x = "Monitors: 2\n 0: +*DP-1 2560/597x1440/336+0+0  DP-1\n 1: +HDMI-1 1920/510x1080/290+2560+180  HDMI-1\n";
        assert_eq!(parse_xrandr(x), vec![[0, 0, 2560, 1440], [2560, 180, 1920, 1080]]);
        let k = br#"{"outputs":[{"enabled":true,"connected":true,"pos":{"x":0,"y":0},"currentModeId":"3","scale":1.25,"rotation":1,
            "modes":[{"id":"3","size":{"width":2560,"height":1440}}]},{"enabled":false,"pos":{"x":0,"y":0}}]}"#;
        assert_eq!(parse_kscreen(k), Some(vec![[0, 0, 2048, 1152]]));
    }
}
