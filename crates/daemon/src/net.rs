//! Connection helpers shared by every mode: authorization/pairing, connect, tunnel pump.
use crate::store::{Identity, Peers};
use kvm_protocol::secure::{self, SecureConn};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;


/// Decide whether to talk to this peer. Returns Ok(true) if it was newly paired.
///
/// * `pin_paired`: both sides just proved knowledge of the single-use pairing PIN inside the handshake,
///   so the peer's key is saved as trusted (on both devices).
/// * otherwise the peer's key must already be trusted.
pub fn authorize(conn: &mut SecureConn, remote: &[u8], pin_paired: bool, name: &str) -> io::Result<bool> {
    let mut peers = Peers::load();
    if pin_paired {
        peers.add(name, remote);
        peers.save()?;
        // make sure the other side saved us too before we go on
        conn.send_raw(b"P")?;
        if conn.recv_raw()? != b"P" {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "pairing was not completed on the other device"));
        }
        println!("Paired with {name}.");
        return Ok(true);
    }
    let mine = if peers.is_trusted(remote) { b'T' } else { b'R' };
    conn.send_raw(&[mine])?;
    let theirs = conn.recv_raw()?.first().copied().unwrap_or(b'R');
    if mine == b'T' && theirs == b'T' {
        Ok(false)
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "not paired: pair the devices first (open pairing on the screen, then pair with its PIN)",
        ))
    }
}

/// Connect to a receiver. `pin = Some(..)` pairs with it (the PIN is shown on the receiver).
pub fn connect(addr: &str, ident: &Identity, pin: Option<&str>, name: &str) -> io::Result<SecureConn> {
    let sock = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "cannot resolve address"))?;
    let stream = TcpStream::connect_timeout(&sock, Duration::from_secs(5))?;
    let est = secure::connect(stream, &ident.private, pin)?;
    let mut conn = est.conn;
    let label = if name.is_empty() { sock.ip().to_string() } else { name.to_string() };
    authorize(&mut conn, &est.remote_key, est.pin_paired, &label)?;
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
