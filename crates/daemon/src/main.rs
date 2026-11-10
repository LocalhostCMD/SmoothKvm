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

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

mod ctl;
mod httpd;
mod net;
mod rctl;
mod shared;
mod store;
mod tui;
use store::Identity;

#[cfg(unix)]
mod display;
#[cfg(windows)]
mod app;
#[cfg(windows)]
mod capture_win;
#[cfg(windows)]
mod daemon;
#[cfg(windows)]
mod gui;
#[cfg(windows)]
mod inject_win;
#[cfg(target_os = "linux")]
mod inject_linux;
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

/// `service`: running as a background service (pairing prompts go to the terminal UI, not stdin).
fn handle_session(stream: TcpStream, ident: &Identity, service: bool) -> io::Result<bool> {
    let peer = stream.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
    let est = secure::accept(stream, &ident.private)?;
    let mut conn = est.conn;
    let svc = |name: &str, sas: &str| shared::prompt_pair(name, sas);
    let confirm: &dyn Fn(&str, &str) -> bool = if service { &svc } else { &net::cli_confirm };
    let newly_paired = net::authorize(&mut conn, &est.remote_key, &est.sas, shared::pairing_open(), &peer, confirm)?;
    let id = shared::session_start(&peer);
    let result = session_loop(conn, &peer, newly_paired);
    shared::session_end(id);
    result
}

fn session_loop(mut conn: secure::SecureConn, peer: &str, newly_paired: bool) -> io::Result<bool> {
    println!("secure session with {peer}");
    // The sender pings every second; silence for 10s means it's gone.
    conn.set_timeouts(Some(Duration::from_secs(10)), Some(Duration::from_secs(5)))?;
    let mut held = Held::default();
    let mut first = true;
    let end = loop {
        match conn.recv() {
            Ok(Message::OpenDisplay) if first => {
                // Paired peers may reach the local VNC server (127.0.0.1:5900 only, never anything else).
                println!("display tunnel opened by {peer}");
                conn.set_timeouts(None, Some(Duration::from_secs(30)))?;
                let local = TcpStream::connect_timeout(&"127.0.0.1:5900".parse().unwrap(), Duration::from_secs(3))?;
                net::pump(local, conn)?;
                println!("display tunnel closed");
                return Ok(newly_paired);
            }
            Ok(Message::DisplayCmd(action)) if first => {
                println!("display command from {peer}: {action:?}");
                let (running, detail) = display_command(action);
                conn.send(&Message::DisplayState { running, detail })?;
                return Ok(newly_paired);
            }
            Ok(Message::Input(ev)) => {
                held.track(&ev);
                inject(&ev);
            }
            Ok(Message::Leave) | Ok(Message::ReleaseAll) => held.release_all(),
            Ok(_) => {}
            Err(e) => break e,
        }
        first = false;
    };
    held.release_all();
    println!("session ended: {end}");
    Ok(newly_paired)
}

const MAX_SESSIONS: usize = 16;

/// Whether this machine currently accepts incoming control (always on for the Linux service;
/// on Windows it follows the "let other computers control this PC" setting).
pub static RECEIVE_ENABLED: AtomicBool = AtomicBool::new(true);

/// `ctl_server`: also run the receiver control API (Linux service). `ui_confirm`: pairing codes are
/// confirmed in the app/terminal UI instead of on stdin.
fn serve(addr: &str, pair: bool, ctl_server: bool, ui_confirm: bool) -> io::Result<()> {
    let ident = Arc::new(Identity::load_or_create()?);
    let listener = TcpListener::bind(addr)?;
    println!("SmoothKvm receiver {VERSION} listening on {addr} (device id {})", store::short(&secure::to_hex(&ident.public)));
    if ctl_server {
        let token = store::panel_token()?;
        let server = rctl::bind()?;
        let id = ident.clone();
        std::thread::spawn(move || rctl::serve(server, token, id));
    }
    if pair {
        shared::open_pairing(120);
        println!("PAIRING MODE on for 120 seconds.");
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
            match handle_session(stream, &ident, ui_confirm) {
                Ok(true) => shared::close_pairing(),
                Ok(false) => {}
                Err(e) => println!("rejected/ended: {e}"),
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
            if let Err(e) = serve(&format!("0.0.0.0:{port}"), false, false, true) {
                eprintln!("cannot start the receiver on port {port}: {e}");
                STARTED.store(false, Ordering::SeqCst);
            }
        });
    }
}

fn test(addr: &str, pairing: bool) -> io::Result<()> {
    let ident = Identity::load_or_create()?;
    let mut conn = net::connect(addr, &ident, pairing, &net::cli_confirm)?;
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
                let mut conn = net::connect(&addr, &ident, false, &|_, _| false)?;
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

fn usage() {
    eprintln!(
        "SmoothKvm {VERSION}\n  SmoothKvm                 interactive terminal UI (type /help)\n  Windows:  open | start | stop | status | install [--autostart] | uninstall\n  Linux:    service | listen [--pair] | display start [WxH]|stop|status\n  Both:     view <addr> | test <addr> [--pair] | --version"
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
        Some("service") => serve(&format!("0.0.0.0:{}", cfg.port), false, true, true),
        Some("listen") => {
            let addr = pos.first().map(|s| s.to_string()).unwrap_or_else(|| format!("0.0.0.0:{}", cfg.port));
            serve(&addr, flag("--pair"), false, false)
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
        Some("test") => match pos.first() {
            Some(a) => test(a, flag("--pair")),
            None => Err(io::Error::new(io::ErrorKind::InvalidInput, "usage: SmoothKvm test <addr> [--pair]")),
        },
        _ => {
            usage();
            Ok(())
        }
    }
}
