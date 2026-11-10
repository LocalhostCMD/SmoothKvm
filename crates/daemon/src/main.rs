// SmoothKvm: software KVM.
//
// Windows (the PC with the keyboard and mouse):
//   SmoothKvm                 open the control panel (starts the background app if needed)
//   SmoothKvm start | stop | status
//   SmoothKvm install [--autostart]   put SmoothKvm on your PATH (and start with Windows)
//   SmoothKvm uninstall
// Linux (the machine being controlled):
//   SmoothKvm listen [--pair]          receive input; --pair accepts a new device for 2 minutes
//   SmoothKvm display start [WxH] | stop | status     on-demand virtual screen
// Both:
//   SmoothKvm view <addr>              tunnel a remote display to localhost:5901 for a VNC viewer
//   SmoothKvm test <addr> [--pair]     send a small mouse-square
//
// All traffic is encrypted and mutually authenticated (see kvm_protocol::secure).
use kvm_protocol::secure;
use kvm_protocol::{DisplayAction, InputEvent, Message, MouseButton};
use std::collections::HashSet;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod net;
mod store;
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
mod inject_win;
#[cfg(target_os = "linux")]
mod inject_linux;
#[cfg(windows)]
mod panel;
#[cfg(windows)]
mod shared;

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

const PAIRING_WINDOW: Duration = Duration::from_secs(120);

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

fn handle_session(stream: TcpStream, ident: &Identity, pairing: bool) -> io::Result<bool> {
    let peer = stream.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
    let est = secure::accept(stream, &ident.private)?;
    let mut conn = est.conn;
    let newly_paired = net::authorize(&mut conn, &est.remote_key, &est.sas, pairing, &peer, &net::cli_confirm)?;
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
            Ok(Message::Leave) => held.release_all(),
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

fn serve(addr: &str, pairing: bool) -> io::Result<()> {
    let ident = Arc::new(Identity::load_or_create()?);
    let listener = TcpListener::bind(addr)?;
    println!("SmoothKvm receiver listening on {addr} (device id {})", store::short(&secure::to_hex(&ident.public)));
    let pairing_until = Instant::now() + PAIRING_WINDOW;
    if pairing {
        println!("PAIRING MODE on for {} seconds. Press 'Pair this device' in the SmoothKvm panel now.", PAIRING_WINDOW.as_secs());
    }
    let pairing = Arc::new(AtomicBool::new(pairing));
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if pairing.load(Ordering::SeqCst) && Instant::now() > pairing_until {
            pairing.store(false, Ordering::SeqCst);
            println!("pairing window closed");
        }
        // Input, control and display sessions run side by side; cap them so strangers can't flood us.
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_SESSIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let (ident, pairing, active) = (ident.clone(), pairing.clone(), active.clone());
        std::thread::spawn(move || {
            match handle_session(stream, &ident, pairing.load(Ordering::SeqCst)) {
                Ok(true) => pairing.store(false, Ordering::SeqCst),
                Ok(false) => {}
                Err(e) => println!("rejected/ended: {e}"),
            }
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
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
        "SmoothKvm\n  Windows:  SmoothKvm | start | stop | status | install [--autostart] | uninstall\n  Linux:    SmoothKvm listen [--pair] | display start [WxH]|stop|status\n  Both:     SmoothKvm view <addr> | test <addr> [--pair]"
    );
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let pos: Vec<&String> = args.iter().skip(1).filter(|a| !a.starts_with("--")).collect();
    let cfg = store::load_config();
    match args.first().map(String::as_str) {
        #[cfg(windows)]
        None | Some("panel") | Some("open") => app::open_panel(),
        #[cfg(windows)]
        Some("daemon") => daemon::run(),
        #[cfg(windows)]
        Some("start") => app::start().map(|started| println!("{}", if started { "SmoothKvm started." } else { "SmoothKvm is already running." })),
        #[cfg(windows)]
        Some("stop") => app::stop(),
        #[cfg(windows)]
        Some("status") => app::status(),
        #[cfg(windows)]
        Some("install") => app::install(flag("--autostart")),
        #[cfg(windows)]
        Some("uninstall") => app::uninstall(),
        Some("listen") => {
            let addr = pos.first().map(|s| s.to_string()).unwrap_or_else(|| format!("0.0.0.0:{}", cfg.port));
            serve(&addr, flag("--pair"))
        }
        Some("display") => {
            #[cfg(unix)]
            {
                display::run(args.get(1).map(String::as_str), args.get(2).map(String::as_str))
            }
            #[cfg(not(unix))]
            {
                Err(io::Error::new(io::ErrorKind::Unsupported, "virtual displays are created on the Linux machine; use the panel"))
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
