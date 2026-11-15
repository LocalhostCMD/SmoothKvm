//! Linux (KDE Plasma): show an app's window on another computer ("drag the app over"), the other direction
//! of `stream_win`.
//!
//! The app keeps running here. KWin takes the pictures (its ScreenShot2 interface, which works for Wayland
//! and X11 apps alike and also when the window is covered), only changed tiles are sent (see wincodec), and
//! the clicks, wheel and keys used on the other computer's copy are played back here through the virtual
//! input devices, with the pointer placed inside the real window.
//!
//! KWin only gives pictures to programs whose .desktop file asks for it
//! (`X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2`; the SmoothKvm package's does).
use crate::dbus_min::{dict_u32, pad, put_sig, put_str, Bus};
use crate::store::Identity;
use crate::{net, shared, wincodec, xfer};
use kvm_protocol::{InputEvent, Message, WindowEvent};
use serde::Serialize;
use std::io::{self, Read};
use std::os::fd::FromRawFd;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Serialize, Clone, Debug)]
pub struct AppWindow {
    pub id: String,
    pub title: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Serialize, Clone, Debug)]
pub struct Stream {
    pub id: u32,
    pub window: String,
    pub title: String,
    pub to: String,
    #[serde(skip)]
    stop: Arc<AtomicBool>,
}

static STREAMS: Mutex<Vec<Stream>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU32 = AtomicU32::new(1);

fn err(m: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::Other, m.into())
}

// ---- asking KWin about windows (a one-shot script that answers over D-Bus) ----

const SCRIPT_NAME: &str = "smoothkvm-windows";
const SCRIPT: &str = r#"// SmoothKvm: lists app windows / reports where one is / brings it to the front / resizes it. Runs once.
(function () {
    var act = "@ACT@", id = "@ID@", W = @W@, H = @H@;
    var list = workspace.windowList ? workspace.windowList() : workspace.clientList();
    var out = [];
    for (var i = 0; i < list.length; i++) {
        var w = list[i];
        var wid = String(w.internalId);
        if (act === "list") {
            if (!w.normalWindow || w.skipTaskbar) continue;
        } else if (wid !== id) continue;
        var f = w.frameGeometry, c = w.clientGeometry ? w.clientGeometry : w.frameGeometry;
        if (act === "activate") {
            if (w.minimized) w.minimized = false;
            if (workspace.activeWindow !== undefined) workspace.activeWindow = w; else workspace.activeClient = w;
        }
        if (act === "resize") {
            w.frameGeometry = { x: f.x, y: f.y, width: W + (f.width - c.width), height: H + (f.height - c.height) };
        }
        out.push({ id: wid, title: String(w.caption), x: Math.round(c.x), y: Math.round(c.y), w: Math.round(c.width), h: Math.round(c.height) });
    }
    callDBus("org.smoothkvm.Edge", "/Edge", "org.smoothkvm.Edge", "Windows", JSON.stringify(out));
})();
"#;

static REPLY: Mutex<Option<String>> = Mutex::new(None);
static REPLY_CV: Condvar = Condvar::new();
static KWIN_LOCK: Mutex<()> = Mutex::new(());

/// The script's answer arrived (called by the D-Bus listener in edge_linux).
pub fn script_reply(s: String) {
    if let Ok(mut g) = REPLY.lock() {
        *g = Some(s);
        REPLY_CV.notify_all();
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Geom {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

/// A window id as KWin prints it: "{8-4-4-4-12 hex}". Anything else is refused (it goes into a script).
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 40 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-' || c == '{' || c == '}')
}

fn kwin(act: &str, id: &str, w: u32, h: u32) -> io::Result<Vec<(AppWindow, Geom)>> {
    if !(id.is_empty() || valid_id(id)) {
        return Err(err("bad window id"));
    }
    let _one = KWIN_LOCK.lock().map_err(|_| err("busy"))?;
    let path = crate::store::config_dir().join("kwin-windows.js");
    std::fs::write(&path, SCRIPT.replace("@ACT@", act).replace("@ID@", id).replace("@W@", &w.to_string()).replace("@H@", &h.to_string()))?;
    if let Ok(mut g) = REPLY.lock() {
        *g = None;
    }
    let mut bus = Bus::session()?;
    let p = path.to_string_lossy().to_string();
    let _ = bus.call("org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting", "unloadScript", &[SCRIPT_NAME]);
    bus.call("org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting", "loadScript", &[&p, SCRIPT_NAME]).map_err(|e| err(format!("KWin (KDE Plasma) is needed to show apps on another computer: {e}")))?;
    bus.call("org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting", "start", &[])?;
    let answer = {
        let g = REPLY.lock().map_err(|_| err("busy"))?;
        let (mut g, _) = REPLY_CV.wait_timeout_while(g, Duration::from_secs(3), |r| r.is_none()).map_err(|_| err("busy"))?;
        g.take()
    };
    let _ = bus.call("org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting", "unloadScript", &[SCRIPT_NAME]);
    parse_windows(&answer.ok_or_else(|| err("KWin did not answer (is edge switching running?)"))?)
}

fn parse_windows(json: &str) -> io::Result<Vec<(AppWindow, Geom)>> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| err(e.to_string()))?;
    let mut out = Vec::new();
    for o in v.as_array().ok_or_else(|| err("unexpected answer from KWin"))?.iter().take(200) {
        let id = o["id"].as_str().unwrap_or("").to_string();
        let (w, h) = (o["w"].as_i64().unwrap_or(0).clamp(0, 16384) as u32, o["h"].as_i64().unwrap_or(0).clamp(0, 16384) as u32);
        if !valid_id(&id) || w == 0 || h == 0 {
            continue;
        }
        let title: String = shared::clean(o["title"].as_str().unwrap_or("")).chars().take(120).collect();
        let g = Geom { x: o["x"].as_i64().unwrap_or(0) as i32, y: o["y"].as_i64().unwrap_or(0) as i32, w, h };
        out.push((AppWindow { id, title: if title.is_empty() { "(no title)".into() } else { title }, width: w, height: h }, g));
    }
    Ok(out)
}

pub fn list() -> Vec<AppWindow> {
    kwin("list", "", 0, 0).map(|v| v.into_iter().map(|(a, _)| a).collect()).unwrap_or_default()
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

// ---- pictures from KWin ----

struct Shot {
    w: u32,
    h: u32,
    /// BGRA, top-down, w*4 bytes per row
    px: Vec<u8>,
}

/// Turn what KWin wrote (QImage rows of `stride` bytes) into tight BGRA rows.
fn to_bgra(raw: &[u8], w: u32, h: u32, stride: u32, format: u32) -> io::Result<Vec<u8>> {
    let (w, h, stride) = (w as usize, h as usize, stride as usize);
    if stride < w * 4 || raw.len() < stride * h {
        return Err(err("unexpected picture layout from KWin"));
    }
    // QImage formats: 4/5/6 = (A)RGB32, which is B,G,R,A in memory here; 16/17/18 = R,G,B,A
    let swap = match format {
        4..=6 => false,
        16..=18 => true,
        f => return Err(err(format!("KWin sent a picture format this version does not know ({f})"))),
    };
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let (src, dst) = (&raw[y * stride..y * stride + w * 4], &mut out[y * w * 4..(y + 1) * w * 4]);
        dst.copy_from_slice(src);
        if swap {
            for p in dst.chunks_exact_mut(4) {
                p.swap(0, 2);
            }
        }
    }
    Ok(out)
}

/// Read exactly `n` bytes from a pipe, giving up if nothing arrives for 2 seconds.
fn read_pipe(f: &mut std::fs::File, n: usize) -> io::Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    let mut buf = vec![0u8; n];
    let mut got = 0;
    while got < n {
        let mut pfd = libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, 2000) } <= 0 {
            return Err(err("KWin did not send the picture"));
        }
        match f.read(&mut buf[got..])? {
            0 => return Err(err("the picture from KWin was cut short")),
            k => got += k,
        }
    }
    Ok(buf)
}

fn capture(bus: &mut Bus, handle: &str) -> io::Result<Shot> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut rd = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    // (s handle, a{sv} options, h pipe)
    let mut body = Vec::new();
    put_str(&mut body, handle);
    pad(&mut body, 4);
    let len_at = body.len();
    body.extend_from_slice(&[0; 4]);
    pad(&mut body, 8);
    let start = body.len();
    for (k, v) in [("include-cursor", false), ("include-decoration", false), ("native-resolution", true)] {
        pad(&mut body, 8);
        put_str(&mut body, k);
        put_sig(&mut body, "b");
        pad(&mut body, 4);
        body.extend_from_slice(&(v as u32).to_le_bytes());
    }
    let len = (body.len() - start) as u32;
    body[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
    pad(&mut body, 4);
    body.extend_from_slice(&0u32.to_le_bytes()); // the first (only) file descriptor sent along
    let reply = bus.call_with_fd("org.kde.KWin", "/org/kde/KWin/ScreenShot2", "org.kde.KWin.ScreenShot2", "CaptureWindow", "sa{sv}h", &body, fds[1]);
    unsafe { libc::close(fds[1]) };
    let reply = reply.map_err(|e| {
        let m = e.to_string();
        if m.contains("NoAuthorized") || m.contains("not authorized") {
            err("KWin refused to take the picture. Install the SmoothKvm package (its app entry asks KDE for this), then log out and in once.")
        } else {
            err(format!("KWin could not take the picture: {m}"))
        }
    })?;
    let meta = dict_u32(&reply);
    let get = |k: &str| meta.get(k).copied().unwrap_or(0);
    let (w, h, stride, format) = (get("width"), get("height"), get("stride"), get("format"));
    if w == 0 || h == 0 || w > wincodec::MAX_SIDE || h > wincodec::MAX_SIDE || stride > wincodec::MAX_SIDE * 4 + 64 {
        return Err(err("KWin sent a picture with an impossible size"));
    }
    let raw = read_pipe(&mut rd, stride as usize * h as usize)?;
    Ok(Shot { w, h, px: to_bgra(&raw, w, h, stride, format)? })
}

// ---- playing input back on the real window ----

struct Live {
    geom: Geom,
    /// the picture's size in pixels (bigger than the window's size on a scaled display)
    pic: (u32, u32),
}

fn apply(ev: WindowEvent, handle: &str, live: &Mutex<Live>) {
    match ev {
        WindowEvent::Move { x, y } => {
            let Ok(l) = live.lock() else { return };
            let (sx, sy) = (l.pic.0.max(1) as f32 / l.geom.w.max(1) as f32, l.pic.1.max(1) as f32 / l.geom.h.max(1) as f32);
            let px = l.geom.x + (x.clamp(0, l.pic.0 as i32 - 1) as f32 / sx) as i32;
            let py = l.geom.y + (y.clamp(0, l.pic.1 as i32 - 1) as f32 / sy) as i32;
            drop(l);
            let d = crate::screens::layout();
            crate::inject_linux::abs_move(px, py, d.width, d.height);
        }
        WindowEvent::Button { button, pressed } => crate::inject_linux::inject(&InputEvent::MouseButton { button, pressed }),
        WindowEvent::Wheel { dx, dy } => crate::inject_linux::inject(&InputEvent::Scroll { dx: dx.clamp(-50, 50), dy: dy.clamp(-50, 50) }),
        WindowEvent::Key { code, pressed } => crate::inject_linux::inject(&InputEvent::Key { code, pressed }),
        WindowEvent::Resize { width, height } => {
            let (sx, sy) = live.lock().map(|l| (l.pic.0.max(1) as f32 / l.geom.w.max(1) as f32, l.pic.1.max(1) as f32 / l.geom.h.max(1) as f32)).unwrap_or((1.0, 1.0));
            let (w, h) = (((width as f32 / sx) as u32).clamp(120, 16000), ((height as f32 / sy) as u32).clamp(80, 16000));
            if let Ok(v) = kwin("resize", handle, w, h) {
                refresh(handle, live, Some(v));
            }
        }
        WindowEvent::Close => {}
    }
}

/// Look up where the window is now (it may have been moved or resized).
fn refresh(handle: &str, live: &Mutex<Live>, known: Option<Vec<(AppWindow, Geom)>>) -> bool {
    let v = match known {
        Some(v) => v,
        None => match kwin("info", handle, 0, 0) {
            Ok(v) => v,
            Err(_) => return true, // could not ask: assume it is still there
        },
    };
    match v.first() {
        Some((_, g)) => {
            if let Ok(mut l) = live.lock() {
                l.geom = *g;
            }
            true
        }
        None => false, // the window is gone
    }
}

static IDENT: std::sync::OnceLock<Arc<Identity>> = std::sync::OnceLock::new();

/// This computer's identity, for windows sent by dragging them to an edge (see edge_linux).
pub fn set_ident(ident: Arc<Identity>) {
    let _ = IDENT.set(ident);
}

/// An app window was let go against the edge where computer `to` is.
pub fn start_dropped(window: &str, to: &str) -> io::Result<String> {
    let ident = IDENT.get().cloned().ok_or_else(|| err("not ready yet"))?;
    start(window, Some(to.to_string()), ident)
}

/// Start showing window `window` on computer `to` (None = the only/first one). Returns that computer's name.
pub fn start(window: &str, to: Option<String>, ident: Arc<Identity>) -> io::Result<String> {
    if !valid_id(window) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "which window?"));
    }
    if streams().iter().any(|s| s.window == window) {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "that window is already shown on another computer"));
    }
    let target = xfer::find_target(to.as_deref())?;
    // bring it to the front (input goes to whatever is under the pointer) and learn where it is
    let found = kwin("activate", window, 0, 0)?;
    let (app, geom) = found.into_iter().next().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "that window is gone"))?;
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    let stop = Arc::new(AtomicBool::new(false));
    if let Ok(mut s) = STREAMS.lock() {
        s.push(Stream { id, window: window.to_string(), title: app.title.clone(), to: target.name.clone(), stop: stop.clone() });
    }
    let (name, window, title) = (target.name.clone(), window.to_string(), app.title);
    std::thread::spawn(move || {
        shared::info(&format!("Showing \"{title}\" on {}", target.name));
        match run(id, &window, &title, geom, &target, &ident, stop) {
            Ok(()) => shared::info(&format!("Stopped showing \"{title}\" on {}", target.name)),
            Err(e) => shared::warn(&format!("Showing \"{title}\" on {} stopped: {e}", target.name)),
        }
        if let Ok(mut s) = STREAMS.lock() {
            s.retain(|x| x.id != id);
        }
    });
    Ok(name)
}

fn run(id: u32, handle: &str, title: &str, geom: Geom, target: &xfer::Target, ident: &Identity, stop: Arc<AtomicBool>) -> io::Result<()> {
    let mut bus = Bus::session()?;
    // the first picture before connecting: if KWin refuses, say so instead of opening an empty window over there
    let first = capture(&mut bus, handle)?;
    let live = Arc::new(Mutex::new(Live { geom, pic: (first.w, first.h) }));
    let mut conn = net::connect(&target.addr, ident, None, &target.name)?;
    conn.send(&Message::WindowOpen { id, title: title.to_string(), width: first.w, height: first.h })?;
    conn.set_timeouts(None, Some(Duration::from_secs(10)))?;
    let (mut rd, mut wr) = conn.split()?;
    let acked = Arc::new(AtomicU32::new(0));
    {
        let (acked, stop, live, handle) = (acked.clone(), stop.clone(), live.clone(), handle.to_string());
        std::thread::spawn(move || {
            while let Ok(m) = rd.recv() {
                match m {
                    Message::WindowAck { seq, .. } => acked.store(seq, Ordering::SeqCst),
                    Message::WindowInput { ev: WindowEvent::Close, .. } | Message::WindowClose { .. } => break,
                    Message::WindowInput { ev, .. } => apply(ev, &handle, &live),
                    _ => {}
                }
            }
            stop.store(true, Ordering::SeqCst);
        });
    }
    let mut enc = wincodec::Encoder::new(first.w, first.h);
    let mut shot = Some(first);
    let mut seq = 0u32;
    let mut last_sent = Instant::now();
    let mut last_geom = Instant::now();
    let frame_time = Duration::from_millis(50);
    let mut misses = 0u32;
    while !stop.load(Ordering::SeqCst) {
        let t0 = Instant::now();
        // at most 2 frames on the way: on a slow link you get fewer frames, never growing lag
        if seq.wrapping_sub(acked.load(Ordering::SeqCst)) >= 2 {
            std::thread::sleep(Duration::from_millis(3));
            continue;
        }
        // where the window is now, every 2 seconds (it can be moved); also notices when it is closed
        if last_geom.elapsed() >= Duration::from_secs(2) {
            last_geom = Instant::now();
            if !refresh(handle, &live, None) {
                break;
            }
        }
        let s = match shot.take() {
            Some(s) => s,
            None => match capture(&mut bus, handle) {
                Ok(s) => {
                    misses = 0;
                    s
                }
                Err(e) => {
                    // minimised or mid-resize: try again; closed: give up
                    misses += 1;
                    if misses > 40 {
                        let _ = wr.send(&Message::WindowClose { id });
                        return Err(e);
                    }
                    std::thread::sleep(Duration::from_millis(150));
                    continue;
                }
            },
        };
        if (s.w, s.h) != enc.size() {
            wr.send(&Message::WindowOpen { id, title: title.to_string(), width: s.w, height: s.h })?;
            enc = wincodec::Encoder::new(s.w, s.h);
            if let Ok(mut l) = live.lock() {
                l.pic = (s.w, s.h);
            }
            last_geom = Instant::now() - Duration::from_secs(5);
        }
        let packets = enc.encode(&s.px);
        if !packets.is_empty() {
            for p in packets {
                wr.send(&Message::WindowTiles { id, data: p })?;
            }
            seq = seq.wrapping_add(1);
            wr.send(&Message::WindowFrameEnd { id, seq })?;
            last_sent = Instant::now();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_ids_cannot_carry_script() {
        assert!(valid_id("{0f3c2a10-7b1e-4c55-9d3e-2a6f0c9b1d77}"));
        assert!(!valid_id("\"; workspace.x = 1; //"));
        assert!(!valid_id(""));
    }

    #[test]
    fn reads_the_window_list() {
        let j = r#"[{"id":"{0f3c2a10-7b1e-4c55-9d3e-2a6f0c9b1d77}","title":"Dolphin","x":100,"y":80,"w":900,"h":600},
                    {"id":"bad id","title":"x","x":0,"y":0,"w":10,"h":10},{"id":"{aa}","title":"","x":0,"y":0,"w":0,"h":5}]"#;
        let v = parse_windows(j).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].0.title.as_str(), v[0].1), ("Dolphin", Geom { x: 100, y: 80, w: 900, h: 600 }));
        assert!(parse_windows("{}").is_err());
    }

    #[test]
    fn pictures_from_kwin_become_bgra() {
        // 2x1, stride 12 (padding after the pixels), RGBA order
        let raw = [1, 2, 3, 255, 4, 5, 6, 255, 9, 9, 9, 9];
        assert_eq!(to_bgra(&raw, 2, 1, 12, 17).unwrap(), vec![3, 2, 1, 255, 6, 5, 4, 255]);
        assert_eq!(to_bgra(&raw, 2, 1, 12, 6).unwrap(), vec![1, 2, 3, 255, 4, 5, 6, 255]);
        assert!(to_bgra(&raw, 2, 1, 12, 99).is_err());
        assert!(to_bgra(&raw[..6], 2, 1, 12, 6).is_err());
    }
}
