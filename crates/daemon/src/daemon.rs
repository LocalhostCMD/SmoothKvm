//! The SmoothKvm background app (Windows): keeps one secure connection per remote screen,
//! runs the input capture and serves the config panel.
use crate::store::{self, Identity};
use crate::{capture_win, net, panel, shared};
use kvm_protocol::{Config, DisplayAction, Message, Remote};
use std::collections::HashMap;
use std::io;
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static GEN: AtomicU64 = AtomicU64::new(0);

pub fn run() -> io::Result<()> {
    let ident = Arc::new(Identity::load_or_create()?);
    let token = store::panel_token()?;
    // Binding the panel port doubles as the single-instance lock.
    let server = panel::bind().map_err(|e| io::Error::new(io::ErrorKind::AddrInUse, format!("SmoothKvm is already running ({e})")))?;
    let mut cfg = store::load_config();
    let ctl = capture_win::start(cfg.clone())?;
    {
        let ident = ident.clone();
        std::thread::spawn(move || panel::serve(server, token, ident));
    }
    apply(&cfg, &ctl, &ident);
    println!("SmoothKvm running");
    let mut seen = store::config_mtime();
    loop {
        std::thread::sleep(Duration::from_millis(700));
        let now = store::config_mtime();
        if now != seen {
            seen = now;
            cfg = store::load_config();
            apply(&cfg, &ctl, &ident);
        }
    }
}

fn apply(cfg: &Config, ctl: &capture_win::Controller, ident: &Arc<Identity>) {
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
    ctl: capture_win::Controller,
) {
    let confirm = |_ip: &str, sas: &str| shared::prompt_pair(&r.name, sas);
    while GEN.load(Ordering::SeqCst) == generation {
        if r.addr.trim().is_empty() {
            shared::set_status(&r.name, "offline", "no address set");
            sleep_until_event(generation, &r.name, Duration::from_secs(2));
            continue;
        }
        shared::set_status(&r.name, "connecting", "");
        let want_pair = shared::take_pair_request(&r.name);
        match net::connect(&r.addr, &ident, want_pair, &confirm) {
            Ok(mut conn) => {
                shared::set_status(&r.name, "connected", "");
                while rx.try_recv().is_ok() {}
                ctl.set_connected(generation, idx, true);
                let end = loop {
                    if GEN.load(Ordering::SeqCst) != generation {
                        break io::Error::new(io::ErrorKind::Other, "configuration changed");
                    }
                    match rx.recv_timeout(Duration::from_secs(1)) {
                        Ok(m) => {
                            if let Err(e) = conn.send(&m) {
                                break e;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            if let Err(e) = conn.send(&Message::Ping) {
                                break e;
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                };
                ctl.set_connected(generation, idx, false);
                shared::set_status(&r.name, "offline", &end.to_string());
            }
            Err(e) => {
                let msg = e.to_string();
                let state = if msg.contains("not paired") { "not_paired" } else { "offline" };
                shared::set_status(&r.name, state, &msg);
            }
        }
        sleep_until_event(generation, &r.name, Duration::from_secs(3));
    }
}

fn find_remote(name: &str) -> io::Result<Remote> {
    store::load_config()
        .remotes
        .into_iter()
        .find(|r| r.name == name)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown screen"))
}

/// Start/stop/query the virtual display on a receiver over a short control session.
pub fn control_display(name: &str, ident: &Identity, action: DisplayAction) -> io::Result<(bool, String)> {
    let r = find_remote(name)?;
    let mut conn = net::connect(&r.addr, ident, false, &|_, _| false)?;
    conn.send(&Message::DisplayCmd(action))?;
    conn.set_timeouts(Some(Duration::from_secs(45)), Some(Duration::from_secs(5)))?;
    match conn.recv()? {
        Message::DisplayState { running, detail } => Ok((running, detail)),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected reply")),
    }
}

static TUNNELS: Mutex<Option<HashMap<String, u16>>> = Mutex::new(None);

/// Make sure a localhost listener tunnels to this screen's virtual display; returns its port.
pub fn ensure_tunnel(name: &str, ident: Arc<Identity>) -> io::Result<u16> {
    find_remote(name)?;
    let mut map = TUNNELS.lock().map_err(|_| io::Error::new(io::ErrorKind::Other, "lock"))?;
    let map = map.get_or_insert_with(HashMap::new);
    if let Some(p) = map.get(name) {
        return Ok(*p);
    }
    let (listener, port) = (5901u16..5931)
        .find_map(|p| TcpListener::bind(("127.0.0.1", p)).ok().map(|l| (l, p)))
        .ok_or_else(|| io::Error::new(io::ErrorKind::AddrInUse, "no free local port for the viewer"))?;
    map.insert(name.to_string(), port);
    let name = name.to_string();
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let Ok(s) = s else { continue };
            let (name, ident) = (name.clone(), ident.clone());
            std::thread::spawn(move || {
                let res = (|| -> io::Result<()> {
                    let r = find_remote(&name)?;
                    let mut conn = net::connect(&r.addr, &ident, false, &|_, _| false)?;
                    conn.send(&Message::OpenDisplay)?;
                    net::pump(s, conn)
                })();
                if let Err(e) = res {
                    eprintln!("display tunnel: {e}");
                }
            });
        }
    });
    Ok(port)
}

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
