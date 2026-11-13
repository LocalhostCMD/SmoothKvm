//! The SmoothKvm background app (Windows): keeps one secure connection per remote screen,
//! runs the input capture and serves the config panel.
use crate::store::{self, Identity};
#[cfg(unix)]
use crate::capture_linux::Controller;
#[cfg(windows)]
use crate::capture_win::Controller;
use crate::{net, shared};
#[cfg(windows)]
use crate::{capture_win, panel};
use kvm_protocol::{Config, DisplayAction, InputEvent, Message, Remote};
use std::io;
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

static GEN: AtomicU64 = AtomicU64::new(0);

#[cfg(windows)]
/// Start everything except the window: input capture, one connection per screen, the panel
/// server and a config watcher. Returns immediately; the work runs on background threads.
pub fn start_backend() -> io::Result<()> {
    let ident = Arc::new(Identity::load_or_create()?);
    let token = store::panel_token()?;
    // Binding the panel port doubles as the single-instance lock.
    let server = panel::bind().map_err(|e| io::Error::new(io::ErrorKind::AddrInUse, format!("SmoothKvm is already running ({e})")))?;
    let cfg = store::load_config();
    let ctl = capture_win::start(cfg.clone())?;
    {
        let ident = ident.clone();
        std::thread::spawn(move || panel::serve(server, token, ident));
    }
    apply(&cfg, &ctl, &ident);
    crate::drop_win::start(ident.clone());
    crate::update::start_checker();
    std::thread::spawn(move || {
        let mut seen = store::config_mtime();
        loop {
            std::thread::sleep(Duration::from_millis(700));
            let now = store::config_mtime();
            if now != seen {
                seen = now;
                apply(&store::load_config(), &ctl, &ident);
            }
        }
    });
    Ok(())
}

fn apply(cfg: &Config, ctl: &Controller, ident: &Arc<Identity>) {
    let generation = GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let mut senders = Vec::new();
    for (i, r) in cfg.remotes.iter().enumerate() {
        let (tx, rx) = channel();
        senders.push(tx);
        if r.enabled {
            let (r, ident, ctl) = (r.clone(), ident.clone(), ctl.clone());
            std::thread::spawn(move || remote_thread(generation, i, r, rx, ident, ctl));
        } else {
            shared::set_status(&r.name, "disabled", "");
        }
    }
    ctl.set_remotes(generation, cfg.clone(), senders);
    crate::clip::ENABLED.store(cfg.clipboard, Ordering::SeqCst);
    #[cfg(windows)]
    // "Let other computers control this PC" (off by default).
    crate::sync_receiver(cfg.receive, cfg.port);
}

fn sleep_until_event(generation: u64, name: &str, total: Duration) {
    let step = Duration::from_millis(250);
    let mut waited = Duration::ZERO;
    while waited < total && GEN.load(Ordering::SeqCst) == generation && !shared::pair_requested(name) {
        std::thread::sleep(step);
        waited += step;
    }
}

fn remote_thread(
    generation: u64,
    idx: usize,
    r: Remote,
    rx: Receiver<Message>,
    ident: Arc<Identity>,
    ctl: Controller,
) {
    let mut last_err = String::new();
    while GEN.load(Ordering::SeqCst) == generation {
        if r.addr.trim().is_empty() {
            shared::set_status(&r.name, "offline", "no address set");
            sleep_until_event(generation, &r.name, Duration::from_secs(2));
            continue;
        }
        shared::set_status(&r.name, "connecting", "");
        let pin = shared::take_pair_request(&r.name);
        match net::connect(&r.addr, &ident, pin.as_deref(), &r.name) {
            Ok(mut conn) => {
                shared::set_status(&r.name, "connected", "");
                shared::info(&format!("Connected to {} ({})", r.name, r.addr));
                last_err.clear();
                while rx.try_recv().is_ok() {}
                ctl.set_connected(generation, idx, true);
                // Tell the screen where we listen ourselves, so it can send files/windows back to us.
                let cfg = store::load_config();
                let listening = cfg!(unix) || cfg.receive;
                if listening {
                    if let Err(e) = conn.send(&Message::Announce { port: cfg.port, version: kvm_protocol::PROTO_VERSION }) {
                        ctl.set_connected(generation, idx, false);
                        shared::set_status(&r.name, "offline", &e.to_string());
                        continue;
                    }
                }
                let (mut rd, mut wr) = match conn.split() {
                    Ok(x) => x,
                    Err(e) => {
                        shared::set_status(&r.name, "offline", &e.to_string());
                        continue;
                    }
                };
                let cs = Arc::new(std::sync::Mutex::new(crate::clip::ClipSync::new()));
                {
                    // The other computer may send its clipboard text (applied only when sharing is on).
                    let cs = cs.clone();
                    std::thread::spawn(move || {
                        while let Ok(m) = rd.recv() {
                            if let Message::Clipboard(t) = m {
                                if let Ok(mut c) = cs.lock() {
                                    c.incoming(t);
                                }
                            }
                        }
                        rd.shutdown();
                    });
                }
                let mut last_clip = std::time::Instant::now();
                let end = loop {
                    if GEN.load(Ordering::SeqCst) != generation {
                        break io::Error::new(io::ErrorKind::Other, "configuration changed");
                    }
                    if last_clip.elapsed() >= Duration::from_millis(600) {
                        last_clip = std::time::Instant::now();
                        let text = cs.lock().ok().and_then(|mut c| c.outgoing());
                        if let Some(t) = text {
                            if let Err(e) = wr.send(&Message::Clipboard(t)) {
                                break e;
                            }
                        }
                    }
                    match rx.recv_timeout(Duration::from_secs(1)) {
                        Ok(m) => {
                            // A mouse can produce 1000 moves a second: merge whatever is already queued into
                            // fewer messages so the network carries far fewer packets (same result, less latency).
                            let mut batch = vec![m];
                            while batch.len() < 512 {
                                match rx.try_recv() {
                                    Ok(n) => batch.push(n),
                                    Err(_) => break,
                                }
                            }
                            let mut failed = None;
                            for msg in coalesce(batch) {
                                if let Err(e) = wr.send(&msg) {
                                    failed = Some(e);
                                    break;
                                }
                            }
                            if let Some(e) = failed {
                                break e;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            if let Err(e) = wr.send(&Message::Ping) {
                                break e;
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                };
                ctl.set_connected(generation, idx, false);
                wr.shutdown();
                shared::set_status(&r.name, "offline", &end.to_string());
                shared::warn(&format!("Connection to {} ended: {end}", r.name));
            }
            Err(e) => {
                let msg = e.to_string();
                let state = if msg.contains("not paired") { "not_paired" } else { "offline" };
                if msg != last_err {
                    if state == "not_paired" { shared::alert(&format!("{} refused us: not paired (or its key changed). Pair again only if you expect this.", r.name)); }
                    else { shared::warn(&format!("Cannot reach {}: {msg}", r.name)); }
                    last_err = msg.clone();
                }
                shared::set_status(&r.name, state, &msg);
            }
        }
        sleep_until_event(generation, &r.name, Duration::from_secs(3));
    }
}

/// Merge runs of relative mouse moves and scrolls into one message each; every other message keeps its place,
/// so key presses and button clicks still happen in exactly the order they were made.
fn coalesce(batch: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(batch.len());
    for m in batch {
        match (&m, out.last_mut()) {
            (
                Message::Input(InputEvent::MouseMoveRel { dx, dy }),
                Some(Message::Input(InputEvent::MouseMoveRel { dx: ax, dy: ay })),
            ) => {
                *ax = ax.saturating_add(*dx);
                *ay = ay.saturating_add(*dy);
            }
            (Message::Input(InputEvent::Scroll { dx, dy }), Some(Message::Input(InputEvent::Scroll { dx: ax, dy: ay }))) => {
                *ax = ax.saturating_add(*dx);
                *ay = ay.saturating_add(*dy);
            }
            _ => out.push(m),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use kvm_protocol::MouseButton;

    #[test]
    fn merges_moves_but_keeps_order_of_clicks() {
        let mv = |dx, dy| Message::Input(InputEvent::MouseMoveRel { dx, dy });
        let click = Message::Input(InputEvent::MouseButton { button: MouseButton::Left, pressed: true });
        let out = coalesce(vec![mv(1, 2), mv(3, 4), click, mv(5, 6), mv(1, 1)]);
        assert_eq!(out.len(), 3);
        assert!(matches!(out[0], Message::Input(InputEvent::MouseMoveRel { dx: 4, dy: 6 })));
        assert!(matches!(out[1], Message::Input(InputEvent::MouseButton { .. })));
        assert!(matches!(out[2], Message::Input(InputEvent::MouseMoveRel { dx: 6, dy: 7 })));
    }
}

#[cfg_attr(unix, allow(dead_code))]
fn find_remote(name: &str) -> io::Result<Remote> {
    store::load_config()
        .remotes
        .into_iter()
        .find(|r| r.name == name)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown screen"))
}

/// Start/stop/query the virtual display on a receiver over a short control session.
#[cfg_attr(unix, allow(dead_code))]
pub fn control_display(name: &str, ident: &Identity, action: DisplayAction) -> io::Result<(bool, String)> {
    let r = find_remote(name)?;
    let mut conn = net::connect(&r.addr, ident, None, &r.name)?;
    conn.send(&Message::DisplayCmd(action))?;
    conn.set_timeouts(Some(Duration::from_secs(45)), Some(Duration::from_secs(5)))?;
    match conn.recv()? {
        Message::DisplayState { running, detail } => Ok((running, detail)),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected reply")),
    }
}

/// Open a ONE-SHOT localhost listener that tunnels to this screen's virtual display; returns its port.
/// It accepts exactly one connection within 60 seconds and then closes. Nothing else running on this PC
/// (or any other account on it) gets a second chance to use your remote desktop through it.
#[cfg_attr(unix, allow(dead_code))]
pub fn ensure_tunnel(name: &str, ident: Arc<Identity>) -> io::Result<u16> {
    find_remote(name)?;
    let (listener, port) = (5901u16..5931)
        .find_map(|p| TcpListener::bind(("127.0.0.1", p)).ok().map(|l| (l, p)))
        .ok_or_else(|| io::Error::new(io::ErrorKind::AddrInUse, "no free local port for the viewer"))?;
    listener.set_nonblocking(true)?;
    let name = name.to_string();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let sock = loop {
            match listener.accept() {
                Ok((s, _)) => break Some(s),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() > deadline {
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => break None,
            }
        };
        drop(listener); // the port is closed from here on
        let Some(s) = sock else { return };
        let _ = s.set_nonblocking(false);
        let res = (|| -> io::Result<()> {
            let r = find_remote(&name)?;
            let mut conn = net::connect(&r.addr, &ident, None, &r.name)?;
            conn.send(&Message::OpenDisplay)?;
            net::pump(s, conn)
        })();
        if let Err(e) = res {
            eprintln!("display tunnel: {}", shared::clean(&e.to_string()));
        }
    });
    Ok(port)
}

#[cfg(windows)]
/// Launch a VNC viewer pointed at the tunnel. Returns false if no viewer was found.
pub fn launch_viewer(port: u16) -> bool {
    let target = format!("localhost:{port}");
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let candidates = [
        r"C:\Program Files\TigerVNC\vncviewer.exe".to_string(),
        r"C:\Program Files (x86)\TigerVNC\vncviewer.exe".to_string(),
        format!(r"{local}\Programs\TigerVNC\vncviewer.exe"),
        r"C:\Program Files\RealVNC\VNC Viewer\vncviewer.exe".to_string(),
    ];
    for c in candidates {
        if std::path::Path::new(&c).exists() {
            return std::process::Command::new(c).arg(&target).spawn().is_ok();
        }
    }
    std::process::Command::new("vncviewer").arg(&target).spawn().is_ok()
}

/// Linux: also act as a controller (hotkey switching to the screens in the config) while the receiver runs.
#[cfg(unix)]
pub fn start_links(ident: Arc<Identity>) -> io::Result<()> {
    let cfg = store::load_config();
    let ctl = crate::capture_linux::start(cfg.clone())?;
    apply(&cfg, &ctl, &ident);
    std::thread::spawn(move || {
        let mut seen = store::config_mtime();
        loop {
            std::thread::sleep(Duration::from_millis(700));
            let now = store::config_mtime();
            if now != seen {
                seen = now;
                apply(&store::load_config(), &ctl, &ident);
            }
        }
    });
    Ok(())
}
