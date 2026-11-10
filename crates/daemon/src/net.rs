//! Connection helpers shared by every mode: authorization/pairing, connect, tunnel pump.
use crate::store::{Identity, Peers};
use kvm_protocol::secure::{self, SecureConn};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

pub fn ask_yes_no(prompt: &str) -> bool {
    print!("{prompt} [y/N] ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    let ok = io::stdin().read_line(&mut line).is_ok()
        && matches!(line.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}'), "y" | "Y" | "yes");
    println!("{}", if ok { "-> confirmed" } else { "-> not confirmed" });
    ok
}

/// Terminal confirmation of a pairing code.
pub fn cli_confirm(name: &str, sas: &str) -> bool {
    println!("\nPAIRING with {name}\n  security code: {sas}\n  The other device must show exactly the same code.");
    ask_yes_no("Do the codes match?")
}

/// Decide whether to talk to this peer. Returns Ok(true) if it was newly paired.
/// `confirm(name, code)` asks a human whether the codes match.
pub fn authorize(
    conn: &mut SecureConn,
    remote: &[u8],
    sas: &str,
    pairing: bool,
    name: &str,
    confirm: &dyn Fn(&str, &str) -> bool,
) -> io::Result<bool> {
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
            let ok = confirm(name, sas);
            if conn.confirm_pairing(ok)? {
                peers.add(name, remote);
                peers.save()?;
                println!("Paired with {name}.");
                Ok(true)
            } else {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "pairing was not confirmed on both sides"))
            }
        }
        _ => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "not paired: pair the devices first (start the receiver with --pair, then press Pair)",
        )),
    }
}

pub fn connect(
    addr: &str,
    ident: &Identity,
    pairing: bool,
    confirm: &dyn Fn(&str, &str) -> bool,
) -> io::Result<SecureConn> {
    let sock = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "cannot resolve address"))?;
    let stream = TcpStream::connect_timeout(&sock, Duration::from_secs(5))?;
    let est = secure::connect(stream, &ident.private)?;
    let mut conn = est.conn;
    authorize(&mut conn, &est.remote_key, &est.sas, pairing, &sock.ip().to_string(), confirm)?;
    conn.set_timeouts(None, Some(Duration::from_secs(5)))?;
    Ok(conn)
}

/// Copy bytes between a local TCP socket and the encrypted connection, both directions.
pub fn pump(local: TcpStream, conn: SecureConn) -> io::Result<()> {
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
    while let Ok(d) = r.recv_raw() {
        if local_w.write_all(&d).is_err() {
            break;
        }
    }
    let _ = local_w.shutdown(std::net::Shutdown::Both);
    r.shutdown();
    let _ = up.join();
    Ok(())
}
