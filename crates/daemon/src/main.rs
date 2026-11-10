// kvmd: software KVM daemon.
//
//   kvmd listen [addr] [--pair]   receiver (e.g. the Ubuntu box): inject input from the sender
//   kvmd send [addr] [--pair]     sender (Windows): capture keyboard/mouse and forward them
//   kvmd test <addr> [--pair]     send a small mouse-square to a receiver
//   kvmd panel                    open the config panel in your browser
//
// All traffic is encrypted and mutually authenticated (see kvm_protocol::secure).
// Devices must be paired once: run both sides with --pair and confirm the codes match.
use kvm_protocol::secure::{self, SecureConn};
use kvm_protocol::{InputEvent, Message, MouseButton};
use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

#[cfg(unix)]
mod display;
#[cfg(not(unix))]
mod display {
    pub fn run(_: Option<&str>, _: Option<&str>) -> std::io::Result<()> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "virtual displays are created on the Linux machine"))
    }
}
mod panel;
mod store;
use store::{Identity, Peers};

#[cfg(windows)]
mod capture_win;
#[cfg(windows)]
mod inject_win;
#[cfg(target_os = "linux")]
mod inject_linux;

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

const PAIRING_WINDOW: Duration = Duration::from_secs(120);

fn ask_yes_no(prompt: &str) -> bool {
    print!("{prompt} [y/N] ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    let ok = io::stdin().read_line(&mut line).is_ok() && matches!(line.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}'), "y" | "Y" | "yes");
    println!("{}", if ok { "-> confirmed" } else { "-> not confirmed" });
    ok
}

/// Decide whether to talk to this peer. Returns Ok(true) if it was newly paired.
fn authorize(conn: &mut SecureConn, remote: &[u8], sas: &str, pairing: bool, name: &str) -> io::Result<bool> {
    let mut peers = Peers::load();
    let mine = if peers.is_trusted(remote) {
        b'T'
    } else if pairing {
        b'U'
    } else {
        b'R'
    };
    conn.send_raw(&[mine])?;
    let theirs = conn.recv_raw()?.first().copied().unwrap_or(b'R');
    match (mine, theirs) {
        (b'T', b'T') => Ok(false),
        (b'U', b'U') => {
            println!("\nPAIRING with {name}\n  security code: {sas}\n  The other device must show exactly the same code.");
            let ok = ask_yes_no("Do the codes match?");
            if conn.confirm_pairing(ok)? {
                peers.add(name, remote);
                peers.save()?;
                println!("Paired.");
                Ok(true)
            } else {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "pairing was not confirmed on both sides"))
            }
        }
        _ => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "peer is not trusted; pair the devices first (run both sides with --pair)",
        )),
    }
}

fn connect(addr: &str, ident: &Identity, pairing: bool) -> io::Result<SecureConn> {
    let sock = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "cannot resolve address"))?;
    let stream = TcpStream::connect_timeout(&sock, Duration::from_secs(5))?;
    let est = secure::connect(stream, &ident.private)?;
    let mut conn = est.conn;
    authorize(&mut conn, &est.remote_key, &est.sas, pairing, &sock.ip().to_string())?;
    conn.set_timeouts(None, Some(Duration::from_secs(5)))?;
    Ok(conn)
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

fn handle_session(stream: TcpStream, ident: &Identity, pairing: bool) -> io::Result<bool> {
    let peer = stream.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
    let est = secure::accept(stream, &ident.private)?;
    let mut conn = est.conn;
    let newly_paired = authorize(&mut conn, &est.remote_key, &est.sas, pairing, &peer)?;
    println!("secure session with {peer}");
    // The sender pings every 2s; silence for 10s means it's gone.
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
                pump(local, conn)?;
                println!("display tunnel closed");
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

/// Copy bytes between a local TCP socket and the encrypted connection, both directions.
fn pump(local: TcpStream, conn: SecureConn) -> io::Result<()> {
    local.set_nodelay(true)?;
    let (mut r, mut w) = conn.split()?;
    let mut local_r = local.try_clone()?;
    let mut local_w = local;
    let up = std::thread::spawn(move || {
        let mut buf = vec![0u8; 16384];
        loop {
            match local_r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if w.send_raw(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        w.shutdown();
    });
    loop {
        match r.recv_raw() {
            Ok(d) => {
                if local_w.write_all(&d).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let _ = local_w.shutdown(std::net::Shutdown::Both);
    r.shutdown();
    let _ = up.join();
    Ok(())
}

/// Listen on localhost and tunnel each connection (e.g. a VNC viewer) to the receiver's display.
fn view(addr_override: Option<String>) -> io::Result<()> {
    let ident = std::sync::Arc::new(Identity::load_or_create()?);
    let addr = addr_override.unwrap_or_else(|| store::load_config().peer_addr);
    if addr.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "usage: kvmd view <receiver-addr:24800> (or set it in the panel)"));
    }
    let listener = TcpListener::bind("127.0.0.1:5901")?;
    println!("Point a VNC viewer at  localhost:5901  (encrypted tunnel to {addr}). Ctrl+C to stop.");
    for s in listener.incoming() {
        let Ok(s) = s else { continue };
        let (ident, addr) = (ident.clone(), addr.clone());
        std::thread::spawn(move || {
            let res = (|| -> io::Result<()> {
                let mut conn = connect(&addr, &ident, false)?;
                conn.send(&Message::OpenDisplay)?;
                pump(s, conn)
            })();
            if let Err(e) = res {
                eprintln!("display tunnel: {e}");
            }
        });
    }
    Ok(())
}

fn serve(addr: &str, mut pairing: bool) -> io::Result<()> {
    let ident = Identity::load_or_create()?;
    let listener = TcpListener::bind(addr)?;
    println!("kvmd listening on {addr} (device id {})", store::short(&secure::to_hex(&ident.public)));
    let pairing_until = Instant::now() + PAIRING_WINDOW;
    if pairing {
        println!("PAIRING MODE on for {} seconds. Start the sender with --pair now.", PAIRING_WINDOW.as_secs());
    }
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if pairing && Instant::now() > pairing_until {
            pairing = false;
            println!("pairing window closed");
        }
        match handle_session(stream, &ident, pairing) {
            Ok(true) => pairing = false,
            Ok(false) => {}
            Err(e) => println!("rejected/ended: {e}"),
        }
    }
    Ok(())
}

fn test(addr: &str, pairing: bool) -> io::Result<()> {
    let ident = Identity::load_or_create()?;
    let mut conn = connect(addr, &ident, pairing)?;
    for (dx, dy) in [(8, 0), (0, 8), (-8, 0), (0, -8)] {
        for _ in 0..20 {
            conn.send(&Message::Input(InputEvent::MouseMoveRel { dx, dy }))?;
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    println!("test sent");
    Ok(())
}

#[cfg(windows)]
fn send_loop(addr_override: Option<String>, pairing: bool) -> io::Result<()> {
    use std::sync::mpsc::{channel, RecvTimeoutError};
    let ident = Identity::load_or_create()?;
    let mut cfg = store::load_config();
    let (tx, rx) = channel::<Message>();
    let ctl = capture_win::start(cfg.clone(), tx)?;
    let mut cfg_time = store::config_mtime();
    let mut first = true;
    println!("kvmd sender running. Escape key returns control to this PC.");
    loop {
        let addr = addr_override.clone().unwrap_or_else(|| cfg.peer_addr.clone());
        if addr.is_empty() {
            eprintln!("no receiver address: set it in the panel (kvmd panel) or pass it on the command line");
            std::thread::sleep(Duration::from_secs(3));
            cfg = store::load_config();
            ctl.update_config(cfg.clone());
            continue;
        }
        match connect(&addr, &ident, pairing && first) {
            Ok(mut conn) => {
                first = false;
                println!("connected securely to {addr}");
                while rx.try_recv().is_ok() {}
                ctl.set_connected(true);
                let end = loop {
                    match rx.recv_timeout(Duration::from_secs(2)) {
                        Ok(m) => {
                            if let Err(e) = conn.send(&m) {
                                break e;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            if let Err(e) = conn.send(&Message::Ping) {
                                break e;
                            }
                            let t = store::config_mtime();
                            if t != cfg_time {
                                cfg_time = t;
                                cfg = store::load_config();
                                ctl.update_config(cfg.clone());
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => return Ok(()),
                    }
                };
                ctl.set_connected(false);
                ctl.force_local();
                println!("disconnected: {end}");
            }
            Err(e) => eprintln!("cannot connect to {addr}: {e}"),
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

#[cfg(not(windows))]
fn send_loop(_addr: Option<String>, _pairing: bool) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "the sender is only implemented for Windows so far"))
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let pairing = args.iter().any(|a| a == "--pair");
    let pos: Vec<&String> = args.iter().skip(1).filter(|a| !a.starts_with("--")).collect();
    let cfg = store::load_config();
    match args.first().map(String::as_str) {
        Some("listen") => {
            let addr = pos.first().map(|s| s.to_string()).unwrap_or_else(|| format!("0.0.0.0:{}", cfg.port));
            serve(&addr, pairing)
        }
        Some("send") => send_loop(pos.first().map(|s| s.to_string()), pairing),
        Some("test") => match pos.first() {
            Some(a) => test(a, pairing),
            None => Err(io::Error::new(io::ErrorKind::InvalidInput, "usage: kvmd test <addr> [--pair]")),
        },
        Some("panel") => panel::run(true),
        Some("display") => display::run(args.get(1).map(String::as_str), args.get(2).map(String::as_str)),
        Some("view") => view(pos.first().map(|s| s.to_string())),
        _ => {
            eprintln!("usage: kvmd listen [addr] [--pair] | send [addr] [--pair] | test <addr> [--pair] | panel");
            Ok(())
        }
    }
}
