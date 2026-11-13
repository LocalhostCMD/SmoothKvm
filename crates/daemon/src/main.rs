// SmoothKvm: software KVM.
//
//   SmoothKvm                  interactive terminal UI (type /help)
//   SmoothKvm open             Windows: open the app window
//   SmoothKvm start | stop     Windows: start / stop the background app
//   SmoothKvm service          Linux: run the receiver as a background service (installed by the .deb)
//   SmoothKvm listen [--pair]  Linux: run the receiver in this terminal
//   SmoothKvm display ...      Linux: virtual display control
//   SmoothKvm view <addr>      tunnel a remote display to localhost:5901 for a VNC viewer
//
// All traffic is encrypted and mutually authenticated (see kvm_protocol::secure).
use kvm_protocol::secure;
use kvm_protocol::{DisplayAction, InputEvent, Message, MouseButton};
use std::collections::HashSet;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

// Route console output through shared::console so it can be silenced while the terminal UI owns the screen.
#[allow(unused_macros)]
macro_rules! println {
    ($($t:tt)*) => { $crate::shared::console(format_args!($($t)*), false) };
}
#[allow(unused_macros)]
macro_rules! eprintln {
    ($($t:tt)*) => { $crate::shared::console(format_args!($($t)*), true) };
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

mod api;
mod clip;
mod ctl;
mod discover;
mod httpd;
mod update;
mod net;
mod rctl;
mod shared;
mod store;
mod tui;
mod wincodec;
mod xfer;
#[cfg(windows)]
mod stream_win;
use store::Identity;

#[cfg(unix)]
mod display;
#[cfg(windows)]
mod app;
#[cfg(windows)]
mod capture_win;
mod daemon;
#[cfg(windows)]
mod dpapi;
#[cfg(windows)]
mod drop_win;
#[cfg(windows)]
mod fx_win;
#[cfg(windows)]
mod gui;
#[cfg(windows)]
mod inject_win;
#[cfg(target_os = "linux")]
mod capture_linux;
#[cfg(target_os = "linux")]
mod inject_linux;
#[cfg(target_os = "linux")]
mod dbus_min;
#[cfg(target_os = "linux")]
mod edge_linux;
#[cfg(target_os = "linux")]
mod drop_x11;
#[cfg(target_os = "linux")]
mod view_x11;
#[cfg(target_os = "linux")]
mod x11;
#[cfg(unix)]
mod linux_app;
#[cfg(windows)]
mod panel;

#[cfg(windows)]
fn inject(ev: &InputEvent) {
    inject_win::inject(ev)
}
#[cfg(target_os = "linux")]
fn inject(ev: &InputEvent) {
    inject_linux::inject(ev)
}
#[cfg(not(any(windows, target_os = "linux")))]
fn inject(_ev: &InputEvent) {
    eprintln!("injection not implemented on this OS yet");
}

#[cfg(unix)]
fn display_command(a: DisplayAction) -> (bool, String) {
    display::command(a)
}
#[cfg(not(unix))]
fn display_command(_a: DisplayAction) -> (bool, String) {
    (false, "virtual displays are created on the Linux machine".into())
}

/// Release every key and mouse button on this machine (emergency stop on the receiver).
pub fn release_everything() {
    for code in 1..=0xE7u16 {
        inject(&InputEvent::Key { code, pressed: false });
    }
    for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle, MouseButton::Back, MouseButton::Forward] {
        inject(&InputEvent::MouseButton { button, pressed: false });
    }
}

/// Tracks pressed inputs so nothing stays stuck if the sender leaves or disappears.
#[derive(Default)]
struct Held {
    keys: HashSet<u16>,
    buttons: HashSet<MouseButton>,
}

impl Held {
    fn track(&mut self, ev: &InputEvent) {
        match *ev {
            InputEvent::Key { code, pressed } => {
                if pressed {
                    self.keys.insert(code);
                } else {
                    self.keys.remove(&code);
                }
            }
            InputEvent::MouseButton { button, pressed } => {
                if pressed {
                    self.buttons.insert(button);
                } else {
                    self.buttons.remove(&button);
                }
            }
            _ => {}
        }
    }

    fn release_all(&mut self) {
        for code in self.keys.drain() {
            inject(&InputEvent::Key { code, pressed: false });
        }
        for button in self.buttons.drain() {
            inject(&InputEvent::MouseButton { button, pressed: false });
        }
    }
}

/// Never trust numbers that arrive over the network: a (paired, but possibly compromised) controller must not be
/// able to fling the pointer across the planet, overflow arithmetic or scroll forever with a single event.
fn sanitize(ev: InputEvent) -> InputEvent {
    match ev {
        InputEvent::MouseMoveRel { dx, dy } => InputEvent::MouseMoveRel { dx: dx.clamp(-30_000, 30_000), dy: dy.clamp(-30_000, 30_000) },
        InputEvent::MouseMoveAbs { x, y } => InputEvent::MouseMoveAbs { x: x.clamp(-32_768, 32_767), y: y.clamp(-32_768, 32_767) },
        InputEvent::Scroll { dx, dy } => InputEvent::Scroll { dx: dx.clamp(-100, 100), dy: dy.clamp(-100, 100) },
        other => other,
    }
}

fn handle_session(stream: TcpStream, ident: &Identity) -> io::Result<()> {
    let ip = stream.peer_addr()?.ip();
    let peer = ip.to_string();
    // Strangers get a tiny budget (count + time) until they have proven who they are.
    let Some(slot) = shared::preauth_enter(ip) else {
        shared::alert(&format!("Refused {peer}: too many unauthenticated connections at once (possible flood)"));
        return Err(io::Error::new(io::ErrorKind::Other, "too many unauthenticated connections"));
    };
    let pin = shared::pairing_pin();
    let est = match secure::accept(stream, &ident.private, pin.as_deref()) {
        Ok(e) => e,
        Err(e) => {
            // A failed attempt at the pairing PIN burns it: a guessed or captured PIN is never usable later.
            if pin.is_some() && e.kind() == io::ErrorKind::PermissionDenied {
                shared::burn_pairing();
                shared::alert(&format!("Pairing attempt from {peer} FAILED (wrong PIN). The PIN was destroyed; open pairing again if that was you."));
            } else if e.to_string().contains("unknown connection mode") {
                // Not SmoothKvm at all. Usually another keyboard-sharing app that uses the same port.
                shared::warn(&format!(
                    "Ignored a connection from {peer} that is not SmoothKvm. Deskflow, Synergy, Barrier and Input Leap use the same port ({}); stop them or give them another port.",
                    store::load_config().port
                ));
            } else {
                shared::alert(&format!("Connection from {peer} was rejected during the secure handshake: {e}"));
            }
            return Err(e);
        }
    };
    if est.pin_paired {
        shared::burn_pairing(); // single use
    }
    let mut conn = est.conn;
    let who = store::short(&secure::to_hex(&est.remote_key));
    if let Err(e) = net::authorize(&mut conn, &est.remote_key, est.pin_paired, &peer) {
        shared::alert(&format!("Unpaired device {peer} (id {who}) tried to connect and was refused"));
        return Err(e);
    }
    if est.pin_paired {
        shared::info(&format!("New device paired: {peer} (id {who})"));
    }
    drop(slot);
    let id = shared::session_start(&peer, ip);
    let result = session_loop(conn, &peer, &who, id);
    shared::session_end(id);
    result
}

fn session_loop(mut conn: secure::SecureConn, peer: &str, who: &str, id: u64) -> io::Result<()> {
    println!("secure session with {peer}");
    // The sender pings every second; silence for 10s means it's gone.
    conn.set_timeouts(Some(Duration::from_secs(10)), Some(Duration::from_secs(5)))?;
    let mut held = Held::default();
    // The first message decides what kind of session this is.
    let first = match conn.recv() {
        Ok(m) => m,
        Err(e) => {
            println!("session ended: {e}");
            return Ok(());
        }
    };
    match first {
        Message::OpenDisplay => {
            // Paired peers may reach the local VNC server (127.0.0.1:5900 only, never anything else).
            println!("display tunnel opened by {peer}");
            shared::info(&format!("{peer} opened the remote display"));
            conn.set_timeouts(None, Some(Duration::from_secs(30)))?;
            let local = TcpStream::connect_timeout(&"127.0.0.1:5900".parse().unwrap(), Duration::from_secs(3))?;
            net::pump(local, conn)?;
            println!("display tunnel closed");
            return Ok(());
        }
        Message::DisplayCmd(action) => {
            println!("display command from {peer}: {action:?}");
            shared::info(&format!("{peer} sent a display command: {action:?}"));
            let (running, detail) = display_command(action);
            conn.send(&Message::DisplayState { running, detail })?;
            return Ok(());
        }
        Message::SendFiles { count, total } => {
            return xfer::receive(conn, peer, count, total);
        }
        // An app window from the other computer, to show here.
        Message::WindowOpen { id, title, width, height } => {
            shared::info(&format!("{peer} is showing \"{}\" here", shared::clean(&title)));
            #[cfg(target_os = "linux")]
            {
                return view_x11::run(conn, peer, id, title, width, height);
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (title, width, height);
                let _ = conn.send(&Message::WindowClose { id });
                return Err(io::Error::new(io::ErrorKind::Unsupported, "showing app windows from other computers works on Linux so far"));
            }
        }
        Message::Announce { port, .. } => shared::session_control(id, Some(port)),
        other => {
            shared::session_control(id, None);
            if let Message::Enter { edge, .. } = &other {
                shared::session_side(id, *edge);
            }
            apply_message(other, &mut held, &mut clip::ClipSync::new())
        }
    }
    shared::info(&format!("{peer} (id {who}) is now controlling this PC"));
    // Input session: read in this thread, and (when sharing is on) send our clipboard changes from another.
    let (mut rd, mut wr) = conn.split()?;
    let cs = Arc::new(std::sync::Mutex::new(clip::ClipSync::new()));
    let alive = Arc::new(AtomicBool::new(true));
    {
        let (cs, alive) = (cs.clone(), alive.clone());
        std::thread::spawn(move || {
            while alive.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(600));
                let text = cs.lock().ok().and_then(|mut c| c.outgoing());
                if let Some(t) = text {
                    if wr.send(&Message::Clipboard(t)).is_err() {
                        break;
                    }
                }
            }
        });
    }
    let end = loop {
        match rd.recv() {
            Ok(m) => {
                if let Message::Enter { edge, .. } = &m {
                    shared::session_side(id, *edge);
                }
                if let Ok(mut c) = cs.lock() {
                    apply_message(m, &mut held, &mut c);
                }
            }
            Err(e) => break e,
        }
    };
    alive.store(false, Ordering::SeqCst);
    held.release_all();
    println!("session ended: {end}");
    shared::info(&format!("{peer} stopped controlling this PC"));
    Ok(())
}

fn apply_message(m: Message, held: &mut Held, cs: &mut clip::ClipSync) {
    match m {
        Message::Input(ev) => {
            let ev = sanitize(ev);
            held.track(&ev);
            inject(&ev);
        }
        Message::Leave | Message::ReleaseAll => held.release_all(),
        Message::Clipboard(t) => cs.incoming(t),
        _ => {}
    }
}

const MAX_SESSIONS: usize = 16;

/// Whether this machine currently accepts incoming control (always on for the Linux service;
/// on Windows it follows the "let other computers control this PC" setting).
pub static RECEIVE_ENABLED: AtomicBool = AtomicBool::new(true);

/// `ctl_server`: also run the receiver control API (Linux service).
fn serve(addr: &str, pair: bool, ctl_server: bool) -> io::Result<()> {
    let ident = Arc::new(Identity::load_or_create()?);
    let listener = TcpListener::bind(addr)?;
    println!("SmoothKvm receiver {VERSION} listening on {addr} (device id {})", store::short(&secure::to_hex(&ident.public)));
    shared::info(&format!("Listening for paired controllers on {addr}. Unpaired connections are refused and logged as alerts."));
    // Lets the "Add a computer" screen on the other machine find this one, but only while pairing is open.
    discover::start_responder(listener.local_addr()?.port(), api::my_id(&ident));
    if ctl_server {
        update::start_checker();
        let token = store::panel_token()?;
        let server = rctl::bind()?;
        let id = ident.clone();
        #[cfg(unix)]
        {
            if let Err(e) = daemon::start_links(ident.clone()) {
                shared::warn(&format!("Controlling other computers from here is unavailable: {e}"));
            }
        }
        std::thread::spawn(move || rctl::serve(server, token, id));
    }
    if pair {
        let pin = shared::open_pairing(120);
        println!("PAIRING OPEN for 120 seconds. On the controlling PC, pair using this PIN:  {pin}");
    }
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if !RECEIVE_ENABLED.load(Ordering::SeqCst) {
            continue; // switched off in the settings: drop the connection before any handshake
        }
        // Input, control and display sessions run side by side; cap them so strangers can't flood us.
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_SESSIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let (ident, active) = (ident.clone(), active.clone());
        std::thread::spawn(move || {
            if let Err(e) = handle_session(stream, &ident) {
                println!("rejected/ended: {}", shared::clean(&e.to_string()));
            }
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}

/// Make this machine controllable (used by the Windows app when "let other computers control this PC"
/// is on): start the receiver once, then just flip RECEIVE_ENABLED as the setting changes.
#[cfg(windows)]
pub fn sync_receiver(enabled: bool, port: u16) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    RECEIVE_ENABLED.store(enabled, Ordering::SeqCst);
    if enabled && !STARTED.swap(true, Ordering::SeqCst) {
        std::thread::spawn(move || {
            if let Err(e) = serve(&format!("0.0.0.0:{port}"), false, false) {
                eprintln!("cannot start the receiver on port {port}: {e}");
                STARTED.store(false, Ordering::SeqCst);
            }
        });
    }
}

fn test(addr: &str, pin: Option<&str>) -> io::Result<()> {
    let ident = Identity::load_or_create()?;
    let mut conn = net::connect(addr, &ident, pin, "")?;
    for (dx, dy) in [(8, 0), (0, 8), (-8, 0), (0, -8)] {
        for _ in 0..20 {
            conn.send(&Message::Input(InputEvent::MouseMoveRel { dx, dy }))?;
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    println!("test sent");
    Ok(())
}

/// Listen on localhost and tunnel each connection (e.g. a VNC viewer) to the receiver's display.
fn view(addr_override: Option<String>) -> io::Result<()> {
    let ident = Arc::new(Identity::load_or_create()?);
    let addr = addr_override
        .or_else(|| store::load_config().remotes.into_iter().find(|r| !r.addr.is_empty()).map(|r| r.addr))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "usage: SmoothKvm view <receiver-addr:24800>"))?;
    let listener = TcpListener::bind("127.0.0.1:5901")?;
    println!("Point a VNC viewer at  localhost:5901  (encrypted tunnel to {addr}). Ctrl+C to stop.");
    for s in listener.incoming() {
        let Ok(s) = s else { continue };
        let (ident, addr) = (ident.clone(), addr.clone());
        std::thread::spawn(move || {
            let res = (|| -> io::Result<()> {
                let mut conn = net::connect(&addr, &ident, None, "")?;
                conn.send(&Message::OpenDisplay)?;
                net::pump(s, conn)
            })();
            if let Err(e) = res {
                eprintln!("display tunnel: {e}");
            }
        });
    }
    Ok(())
}

fn send_cmd(args: &[String]) -> io::Result<()> {
    let mut to = None;
    let mut paths = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--to" {
            to = it.next().cloned();
        } else {
            let p = std::fs::canonicalize(a).map_err(|e| io::Error::new(e.kind(), format!("{a}: {e}")))?;
            paths.push(p.to_string_lossy().trim_start_matches(r"\\?\").to_string());
        }
    }
    if paths.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "usage: SmoothKvm send <file or folder>... [--to <screen>]"));
    }
    if !ctl::alive() {
        return Err(io::Error::new(io::ErrorKind::NotConnected, "SmoothKvm is not running"));
    }
    let c = ctl::Client::new()?;
    let err = |e: String| io::Error::new(io::ErrorKind::Other, e);
    let r = c.post("/api/send", &serde_json::json!({"paths": paths, "to": to})).map_err(err)?;
    println!("Sending to {}...", r["to"].as_str().unwrap_or("?"));
    // follow the progress until it is done
    loop {
        std::thread::sleep(Duration::from_millis(300));
        let s = c.get("/api/state").map_err(err)?;
        let t = &s["transfer"];
        if t["finished"].as_bool().unwrap_or(false) {
            let detail = t["detail"].as_str().unwrap_or("");
            if t["ok"].as_bool().unwrap_or(false) {
                println!("Done: {detail}");
                return Ok(());
            }
            return Err(err(detail.to_string()));
        }
    }
}

/// `SmoothKvm update`: check for a new version and install it through the running app.
fn update_cmd() -> io::Result<()> {
    if !ctl::alive() {
        return Err(io::Error::new(io::ErrorKind::NotConnected, "SmoothKvm is not running (start it first)"));
    }
    let c = ctl::Client::new()?;
    let err = |e: String| io::Error::new(io::ErrorKind::Other, e);
    let s = c.post("/api/update/check", &serde_json::json!({})).map_err(err)?;
    if let Some(e) = s["error"].as_str() {
        return Err(err(format!("could not check for updates: {e}")));
    }
    if !s["available"].as_bool().unwrap_or(false) {
        println!("You have the newest version ({VERSION}).");
        return Ok(());
    }
    println!("Updating SmoothKvm {VERSION} -> {}...", s["latest"].as_str().unwrap_or("?"));
    c.post("/api/update/apply", &serde_json::json!({})).map_err(err)?;
    let mut last = String::new();
    for _ in 0..1200 {
        std::thread::sleep(Duration::from_millis(500));
        let Ok(st) = c.get("/api/state") else {
            println!("SmoothKvm is restarting with the new version.");
            return Ok(());
        };
        let u = &st["update"];
        if let Some(b) = u["busy"].as_str() {
            if b != last {
                println!("{b}");
                last = b.to_string();
            }
        } else if let Some(e) = u["error"].as_str() {
            return Err(err(e.to_string()));
        }
    }
    Ok(())
}

fn usage() {
    eprintln!(
        "SmoothKvm {VERSION}\n  SmoothKvm                 interactive terminal UI (type /help)\n  SmoothKvm app             open the app window\n  Windows:  start | stop | status | install [--autostart] | uninstall\n  Linux:    service | listen [--pair] | display start [WxH]|stop|status\n  Both:     send <files...> [--to <screen>] | update | view <addr> | test <addr> [--pin PIN] | --version"
    );
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let pos: Vec<&String> = args.iter().skip(1).filter(|a| !a.starts_with("--")).collect();
    let cfg = store::load_config();
    match args.first().map(String::as_str) {
        None | Some("tui") => tui::run(),
        Some("--version") | Some("-V") | Some("version") => {
            println!("SmoothKvm {VERSION}");
            Ok(())
        }
        Some("--help") | Some("-h") | Some("help") => {
            usage();
            Ok(())
        }
        #[cfg(windows)]
        Some("open") | Some("panel") | Some("app") => app::open_panel(),
        #[cfg(windows)]
        Some("daemon") => gui::run(flag("--show")),
        #[cfg(windows)]
        Some("start") => app::start(false).map(|started| println!("{}", if started { "SmoothKvm started." } else { "SmoothKvm is already running." })),
        #[cfg(windows)]
        Some("stop") => app::stop(),
        #[cfg(windows)]
        Some("status") => app::status(),
        #[cfg(windows)]
        Some("install") => app::install(flag("--autostart")),
        #[cfg(windows)]
        Some("uninstall") => app::uninstall(),
        #[cfg(unix)]
        Some("open") | Some("panel") | Some("app") | Some("gui") => linux_app::run(),
        // run by the smoothkvm-helper systemd unit (root) to install a downloaded, signed update
        #[cfg(unix)]
        Some("helper") => update::helper_main(),
        Some("update") => update_cmd(),
        // show an app window on another computer: SmoothKvm stream "<title>" --to <host:port> [--pin PIN]
        #[cfg(windows)]
        Some("stream") => {
            let to = args.iter().position(|a| a == "--to").and_then(|i| args.get(i + 1)).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "usage: SmoothKvm stream \"<window title>\" --to <host:port> [--pin PIN]"))?;
            let pin = args.iter().position(|a| a == "--pin").and_then(|i| args.get(i + 1)).map(String::as_str);
            let title = pos.first().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "which window? give part of its title"))?;
            stream_win::run_cli(title, to, pin)
        }
        Some("service") => serve(&format!("0.0.0.0:{}", cfg.port), false, true),
        Some("listen") => {
            let addr = pos.first().map(|s| s.to_string()).unwrap_or_else(|| format!("0.0.0.0:{}", cfg.port));
            serve(&addr, flag("--pair"), false)
        }
        Some("display") => {
            #[cfg(unix)]
            {
                display::run(args.get(1).map(String::as_str), args.get(2).map(String::as_str))
            }
            #[cfg(not(unix))]
            {
                Err(io::Error::new(io::ErrorKind::Unsupported, "virtual displays are created on the Linux machine; use the app"))
            }
        }
        Some("view") => view(pos.first().map(|s| s.to_string())),
        // Send files to another computer through the running app: SmoothKvm send <files...> [--to <name>]
        Some("send") => send_cmd(&args[1..]).or_else(|e| {
            eprintln!("Could not send: {e}");
            std::process::exit(1)
        }),
        Some("test") => match pos.first() {
            Some(a) => test(a, args.iter().position(|x| x == "--pin").and_then(|i| args.get(i + 1)).map(String::as_str)),
            None => Err(io::Error::new(io::ErrorKind::InvalidInput, "usage: SmoothKvm test <addr> [--pin 12345678]")),
        },
        _ => {
            usage();
            Ok(())
        }
    }
}

/// Linux: run the whole backend (receiver + controller) inside this process, for the terminal app.
/// It exists only while the app is open; quitting the app stops everything.
#[cfg(unix)]
pub fn start_embedded() -> io::Result<()> {
    shared::QUIET.store(true, Ordering::SeqCst);
    let cfg = store::load_config();
    let addr = format!("0.0.0.0:{}", cfg.port);
    let (tx, rx) = std::sync::mpsc::channel::<io::Error>();
    std::thread::spawn(move || {
        if let Err(e) = serve(&addr, false, true) {
            let _ = tx.send(e);
        }
    });
    for _ in 0..50 {
        if let Ok(e) = rx.try_recv() {
            return Err(e);
        }
        if ctl::alive() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "the backend did not start"))
}

/// Stop what start_embedded started: hand every key back, tell the other computers, then end.
#[cfg(unix)]
pub fn stop_embedded() {
    #[cfg(target_os = "linux")]
    {
        capture_linux::Controller.emergency_stop();
        release_everything();
    }
    shared::info("SmoothKvm closed: all connections stopped, all keys released");
    std::thread::sleep(Duration::from_millis(250));
}

/// The keyboard/mouse devices need the `input` group. Right after installing, an open terminal does not
/// have it yet, so restart this program through `sg input` (only when the user really is a member).
#[cfg(target_os = "linux")]
pub fn ensure_input_group() {
    use std::os::unix::process::CommandExt;
    if std::env::var_os("SMOOTHKVM_SG").is_some() {
        return;
    }
    let Ok(groups) = std::fs::read_to_string("/etc/group") else { return };
    let Some(line) = groups.lines().find(|l| l.starts_with("input:")) else { return };
    let mut f = line.split(':');
    let gid: u32 = f.nth(2).and_then(|g| g.parse().ok()).unwrap_or(u32::MAX);
    let members = line.rsplit(':').next().unwrap_or("");
    let me = unsafe { libc::getuid() };
    let name = std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_default();
    let mut gs = [0 as libc::gid_t; 256];
    let n = unsafe { libc::getgroups(256, gs.as_mut_ptr()) }.max(0) as usize;
    if gs[..n].contains(&gid) || me == 0 || !members.split(',').any(|m| m == name) {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        let args: Vec<String> = std::env::args().skip(1).map(|a| format!("'{}'", a.replace('\'', ""))).collect();
        let cmd = format!("'{}' {}", exe.display().to_string().replace('\'', ""), args.join(" "));
        let _ = std::process::Command::new("sg").args(["input", "-c", &cmd]).env("SMOOTHKVM_SG", "1").exec();
    }
}
