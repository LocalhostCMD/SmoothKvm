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

/// What the running connections were started for: (name, address, enabled) per screen, the generation, and
/// each screen's queue. A settings change that leaves these alone keeps every connection (and control) as is.
type Running = (Vec<(String, String, bool)>, u64, Vec<std::sync::mpsc::Sender<Message>>);
static RUNNING: std::sync::Mutex<Option<Running>> = std::sync::Mutex::new(None);

/// Change one screen's saved settings (learned from the connection itself). Keeps the connections.
fn update_remote(name: &str, f: impl FnOnce(&mut Remote) -> bool) {
    let mut cfg = store::load_config();
    let Some(r) = cfg.remotes.iter_mut().find(|r| r.name == name) else { return };
    if f(r) && cfg.validate().is_ok() {
        let _ = store::save_config(&cfg);
    }
}

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

/// A computer that controls this one says where this one is on its map (`edge` of its desktop, `offset`
/// along it). Put it on this computer's map on the opposite side, mirrored, so the pointer can go both ways
/// and both maps agree. Only for paired computers (this is called from an authenticated session).
#[allow(clippy::too_many_arguments)]
pub fn mirror_neighbor(key: &str, my_key: &str, ip: std::net::IpAddr, port: u16, name: &str, edge: kvm_protocol::Edge, offset: Option<i32>, width: u32, height: u32, at: u64) {
    let mut cfg = store::load_config();
    if mirror_into(&mut cfg, key, my_key, ip, port, name, edge, offset, width, height, at) && cfg.validate().is_ok() {
        let _ = store::save_config(&cfg);
    }
}

/// The settings change for `mirror_neighbor`; true if anything changed. If both computers have placed each
/// other, the placement changed last (`at`) wins; changed at the same moment, the lower device key's.
#[allow(clippy::too_many_arguments)]
fn mirror_into(cfg: &mut Config, key: &str, my_key: &str, ip: std::net::IpAddr, port: u16, name: &str, edge: kvm_protocol::Edge, offset: Option<i32>, width: u32, height: u32, at: u64) -> bool {
    if !cfg.mirror_layout {
        return false;
    }
    let (want_edge, want_off) = (edge.opposite(), offset.map(|o| o.saturating_neg().clamp(-40000, 40000)));
    let (w, h) = (width.clamp(100, 20000), height.clamp(100, 20000));
    let addr = std::net::SocketAddr::new(ip, port).to_string();
    let host = ip.to_string();
    let found = cfg.remotes.iter().position(|r| (!r.key.is_empty() && r.key == key) || r.addr == addr || r.addr.rsplit_once(':').map(|(h, _)| h) == Some(host.as_str()));
    let taken = |cfg: &Config, skip: Option<usize>| cfg.remotes.iter().enumerate().any(|(j, o)| Some(j) != skip && o.enabled && o.edge == want_edge);
    match found {
        Some(i) => {
            let free = !taken(cfg, Some(i));
            let r = &mut cfg.remotes[i];
            let before = (r.edge, r.offset, r.width, r.height, r.key.clone());
            let theirs_wins = at > r.layout_at || (at == r.layout_at && key < my_key);
            if theirs_wins && free {
                r.edge = want_edge;
                r.offset = want_off;
                r.layout_at = at;
            }
            r.width = w;
            r.height = h;
            if r.key.is_empty() {
                r.key = key.to_string();
            }
            before != (r.edge, r.offset, r.width, r.height, r.key.clone())
        }
        None => {
            if cfg.remotes.len() >= 4 || taken(cfg, None) {
                return false;
            }
            let base: String = shared::clean(name).trim().chars().take(28).collect();
            let base = if base.is_empty() { host.clone() } else { base };
            let mut n = base.clone();
            let mut k = 2;
            while cfg.remotes.iter().any(|r| r.name == n) {
                n = format!("{base} {k}");
                k += 1;
            }
            shared::info(&format!("{n} controls this computer, so it was added as a screen here too ({want_edge:?} side): the pointer now goes both ways"));
            cfg.remotes.push(Remote { name: n, addr, edge: want_edge, width: w, height: h, enabled: true, key: key.to_string(), offset: want_off, layout_at: at });
            true
        }
    }
}

fn apply(cfg: &Config, ctl: &Controller, ident: &Arc<Identity>) {
    let links: Vec<_> = cfg.remotes.iter().map(|r| (r.name.clone(), r.addr.clone(), r.enabled)).collect();
    if let Ok(g) = RUNNING.lock() {
        if let Some((l, _, _)) = g.as_ref() {
            if *l == links {
                ctl.update_config(cfg.clone());
                crate::clip::ENABLED.store(cfg.clipboard, Ordering::SeqCst);
                #[cfg(windows)]
                crate::sync_receiver(cfg.receive, cfg.port);
                return;
            }
        }
    }
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
    if let Ok(mut g) = RUNNING.lock() {
        *g = Some((links, generation, senders.clone()));
    }
    ctl.set_remotes(generation, cfg.clone(), senders);
    crate::clip::ENABLED.store(cfg.clipboard, Ordering::SeqCst);
    #[cfg(windows)]
    // "Let other computers control this PC" (off by default).
    crate::sync_receiver(cfg.receive, cfg.port);
}

/// Clears the flag when dropped: a helper thread checks it and stops.
struct StopOnDrop(Arc<std::sync::atomic::AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
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
    let mut fails = 0u32;
    while GEN.load(Ordering::SeqCst) == generation {
        if r.addr.trim().is_empty() {
            shared::set_status(&r.name, "offline", "no address set");
            sleep_until_event(generation, &r.name, Duration::from_secs(2));
            continue;
        }
        shared::set_status(&r.name, "connecting", "");
        let pin = shared::take_pair_request(&r.name);
        match net::connect_keyed(&r.addr, &ident, pin.as_deref(), &r.name) {
            Ok((mut conn, key)) => {
                shared::set_status(&r.name, "connected", "");
                shared::info(&format!("Connected to {} ({})", r.name, r.addr));
                fails = 0;
                // remember who this is, so it can be found again if its address changes
                if r.key != key {
                    update_remote(&r.name, |x| {
                        x.key = key.clone();
                        true
                    });
                }
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
                // what we know about the other end: when it last said anything, whether it answers pings
                // (0.11+ does), and its protocol version
                let heard = Arc::new(AtomicU64::new(shared::now_ms()));
                let pongs = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let peer_ver = Arc::new(std::sync::atomic::AtomicU16::new(0));
                {
                    // The other computer may send its clipboard text (applied only when sharing is on).
                    let cs = cs.clone();
                    let name = r.name.clone();
                    let (heard, pongs, peer_ver) = (heard.clone(), pongs.clone(), peer_ver.clone());
                    std::thread::spawn(move || {
                        while let Ok(m) = rd.recv() {
                            heard.store(shared::now_ms(), Ordering::SeqCst);
                            match m {
                                Message::Pong => pongs.store(true, Ordering::SeqCst),
                                Message::Caps { version } => peer_ver.store(version, Ordering::SeqCst),
                                // its real monitors: drawn on the map, and the size used for crossing over
                                Message::ScreenInfo { width, height, monitors } => {
                                    let (w, h) = (width.clamp(100, 20000), height.clamp(100, 20000));
                                    let monitors: Vec<[i32; 4]> = monitors.into_iter().take(16).collect();
                                    shared::set_screen(&name, shared::ScreenShape { width: w, height: h, monitors });
                                    update_remote(&name, |x| {
                                        let changed = (x.width, x.height) != (w, h);
                                        x.width = w;
                                        x.height = h;
                                        changed
                                    });
                                }
                                Message::Clipboard(t) => {
                                    if let Ok(mut c) = cs.lock() {
                                        c.incoming(t);
                                    }
                                }
                                // you are dragging files on that screen against its edge facing this one
                                #[cfg(target_os = "linux")]
                                Message::DragStart { drag, names } => crate::capture_linux::drag_from_screen(generation, idx, drag, names),
                                _ => {}
                            }
                        }
                        rd.shutdown();
                    });
                }
                // The slow, occasional checks (the clipboard: Linux starts wl-paste for it; the map: reads the
                // settings file) run on their own thread and hand over what to send. This thread only sends, so
                // mouse moves never wait behind them (that was a stutter twice a second).
                let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
                let (xtx, xrx) = channel::<Message>();
                // (also stops it when this connection ends early, e.g. the settings changed)
                let _stop = StopOnDrop(alive.clone());
                {
                    let (alive, cs, peer_ver, name) = (alive.clone(), cs.clone(), peer_ver.clone(), r.name.clone());
                    std::thread::spawn(move || {
                        let mut last_clip = std::time::Instant::now();
                        let mut last_layout_check = std::time::Instant::now() - Duration::from_secs(5);
                        let mut sent_layout = None;
                        while alive.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(100));
                            // tell it where it is on this computer's map (it mirrors that, so it can control us too)
                            if listening && peer_ver.load(Ordering::SeqCst) >= 11 && last_layout_check.elapsed() >= Duration::from_secs(1) {
                                last_layout_check = std::time::Instant::now();
                                if let Some(me) = store::load_config().remotes.into_iter().find(|x| x.name == name) {
                                    let local = crate::screens::layout();
                                    let cur = (me.edge, me.offset, local.width, local.height, me.layout_at);
                                    if sent_layout != Some(cur) {
                                        sent_layout = Some(cur);
                                        let m = Message::Neighbor { edge: cur.0, offset: cur.1, width: cur.2, height: cur.3, name: crate::discover::hostname(), at: cur.4 };
                                        if xtx.send(m).is_err() {
                                            return;
                                        }
                                    }
                                }
                            }
                            if last_clip.elapsed() >= Duration::from_millis(600) {
                                last_clip = std::time::Instant::now();
                                let now = crate::clip::read();
                                let text = cs.lock().ok().and_then(|mut c| c.outgoing(now));
                                if let Some(t) = text {
                                    if xtx.send(Message::Clipboard(t)).is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                    });
                }
                crate::boost_thread(true);
                let mut last_ping = std::time::Instant::now();
                let end = loop {
                    if GEN.load(Ordering::SeqCst) != generation {
                        break io::Error::new(io::ErrorKind::Other, "configuration changed");
                    }
                    // It answers pings but has been silent for 4 seconds: it is gone (unplugged, asleep,
                    // crashed). Give control back now instead of waiting minutes for the network to notice.
                    if pongs.load(Ordering::SeqCst) && shared::now_ms().saturating_sub(heard.load(Ordering::SeqCst)) > 4000 {
                        break io::Error::new(io::ErrorKind::TimedOut, "no answer for 4 seconds");
                    }
                    // keep pinging even while input is flowing, so "silent" always means "gone"
                    if last_ping.elapsed() >= Duration::from_secs(1) {
                        last_ping = std::time::Instant::now();
                        if let Err(e) = wr.send(&Message::Ping) {
                            break e;
                        }
                    }
                    // what the checking thread above has for the other computer
                    let mut failed = None;
                    while let Ok(m) = xrx.try_recv() {
                        if let Err(e) = wr.send(&m) {
                            failed = Some(e);
                            break;
                        }
                    }
                    if let Some(e) = failed {
                        break e;
                    }
                    match rx.recv_timeout(Duration::from_millis(100)) {
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
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                };
                alive.store(false, Ordering::SeqCst);
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
                fails += 1;
                // Unreachable a few times in a row: it may have a new address (DHCP, another network).
                // Ask the network for it by its device key, and move it there if it answers.
                if state == "offline" && fails % 3 == 0 && !r.key.is_empty() {
                    if let Some(k) = kvm_protocol::secure::from_hex(&r.key) {
                        let port = r.addr.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).unwrap_or(24800);
                        if let Some(found) = crate::discover::locate(port, &ident.public, &k) {
                            let new = found.to_string();
                            if new != r.addr {
                                shared::info(&format!("Found {} at a new address: {new} (was {})", r.name, r.addr));
                                update_remote(&r.name, |x| {
                                    x.addr = new.clone();
                                    true
                                });
                                return; // the new address restarts this connection
                            }
                        }
                    }
                }
            }
        }
        // quick tries at first (a blip, a restart), then every 3 seconds; refused as unpaired: every 30 s
        // (pairing again wakes this up at once), so the other computer is not flooded with alerts
        let wait = if last_err.contains("not paired") { 30 } else if fails <= 5 { 1 } else { 3 };
        sleep_until_event(generation, &r.name, Duration::from_secs(wait));
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
    fn a_controlling_computer_is_mirrored_onto_this_map() {
        use kvm_protocol::Edge;
        let ip: std::net::IpAddr = "10.0.0.20".parse().unwrap();
        let go = |cfg: &mut Config, edge, off, at| mirror_into(cfg, "aa", "bb", ip, 24800, "LAPTOP", edge, off, 1920, 1080, at);
        let mut cfg = Config::default();
        // it has us on its Right, 200 px down: it goes on our Left, 200 px up
        assert!(go(&mut cfg, Edge::Right, Some(200), 50));
        let r = &cfg.remotes[0];
        assert_eq!((r.name.as_str(), r.addr.as_str(), r.edge, r.offset, r.key.as_str(), r.layout_at), ("LAPTOP", "10.0.0.20:24800", Edge::Left, Some(-200), "aa", 50));
        assert!(cfg.validate().is_ok());
        // the same again changes nothing (so the two computers never ping-pong)
        assert!(!go(&mut cfg, Edge::Right, Some(200), 50));
        // dragged on the other computer later: follows
        assert!(go(&mut cfg, Edge::Top, None, 90));
        assert_eq!((cfg.remotes[0].edge, cfg.remotes[0].offset), (Edge::Bottom, None));
        // an older placement than ours loses
        assert!(!go(&mut cfg, Edge::Left, None, 60));
        assert_eq!(cfg.remotes[0].edge, Edge::Bottom);
        // same moment: the lower device key wins ("aa" < "bb" here, so theirs)
        assert!(go(&mut cfg, Edge::Left, None, 90));
        assert_eq!(cfg.remotes[0].edge, Edge::Right);
        assert!(!mirror_into(&mut cfg, "cc", "bb", ip, 24800, "LAPTOP", Edge::Top, None, 1920, 1080, 90), "higher key at the same moment: ours stays");
        // never onto a side another screen already has
        cfg.remotes.push(Remote { name: "other".into(), addr: "10.0.0.9:24800".into(), edge: Edge::Left, ..Remote::default() });
        go(&mut cfg, Edge::Right, None, 200);
        assert_eq!(cfg.remotes[0].edge, Edge::Right);
        // switched off: untouched
        let mut off = Config { mirror_layout: false, ..Config::default() };
        assert!(!go(&mut off, Edge::Right, None, 10));
        assert!(off.remotes.is_empty());
    }

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
    #[cfg(target_os = "linux")]
    crate::drop_x11::start(ident.clone());
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
