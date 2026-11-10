//! Encrypted, mutually authenticated transport.
//!
//! Normal connections: Noise_XX_25519_ChaChaPoly_BLAKE2s (both sides prove possession of a static
//! X25519 key; ChaCha20-Poly1305; forward secrecy; strict nonce counters so replayed, dropped or
//! reordered frames fail authentication). Which keys are *trusted* is decided by the caller.
//!
//! Pairing connections: Noise_XXpsk3, where the PSK is derived from a single-use PIN shown on the
//! receiving device. Anyone who doesn't know the PIN cannot complete the handshake with either
//! side, so a man-in-the-middle can't slip in (this replaces comparing a short code by eye, which
//! an attacker can defeat by grinding keys until the short codes collide).
use crate::Message;
use blake2::{Blake2s256, Digest};
use snow::{Builder, StatelessTransportState};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;
use std::time::Duration;

const PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const PARAMS_PSK: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";
/// Unauthenticated peers get very little time: this is the main handle for slow-connection abuse.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest plaintext that fits in one Noise frame (65535 - 16 byte tag).
pub const MAX_PLAINTEXT: usize = 65519;

const MODE_NORMAL: u8 = 0x01;
const MODE_PAIRING: u8 = 0x02;

fn err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

pub fn generate_keypair() -> io::Result<(Vec<u8>, Vec<u8>)> {
    let kp = Builder::new(PARAMS.parse().map_err(err)?).generate_keypair().map_err(err)?;
    Ok((kp.private, kp.public))
}

pub fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// Stretch a short PIN into a 32-byte pre-shared key. The stretching only makes offline guessing
/// slower; the real protection is that a PIN is single-use and burned after any failed attempt.
pub fn pin_to_psk(pin: &str) -> [u8; 32] {
    let mut h = Blake2s256::new();
    h.update(b"SmoothKvm pairing v1");
    h.update(pin.as_bytes());
    let mut key: [u8; 32] = h.finalize().into();
    for _ in 0..200_000 {
        let mut h = Blake2s256::new();
        h.update(key);
        h.update(pin.as_bytes());
        key = h.finalize().into();
    }
    key
}

/// Constant-time equality for secrets (tokens, MACs, PINs).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

fn write_frame(s: &mut TcpStream, data: &[u8]) -> io::Result<()> {
    if data.len() > u16::MAX as usize {
        return Err(err("frame too large"));
    }
    let mut v = Vec::with_capacity(2 + data.len());
    v.extend_from_slice(&(data.len() as u16).to_be_bytes());
    v.extend_from_slice(data);
    s.write_all(&v)
}

fn read_frame(s: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut l = [0u8; 2];
    s.read_exact(&mut l)?;
    let mut v = vec![0u8; u16::from_be_bytes(l) as usize];
    s.read_exact(&mut v)?;
    Ok(v)
}

fn seal(ts: &StatelessTransportState, nonce: &mut u64, buf: &mut [u8], stream: &mut TcpStream, data: &[u8]) -> io::Result<()> {
    if data.len() > MAX_PLAINTEXT {
        return Err(err("message too large"));
    }
    let n = ts.write_message(*nonce, data, buf).map_err(err)?;
    *nonce += 1;
    write_frame(stream, &buf[..n])
}

fn open(ts: &StatelessTransportState, nonce: &mut u64, buf: &mut [u8], stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let ct = read_frame(stream)?;
    let n = ts.read_message(*nonce, &ct, buf).map_err(err)?;
    *nonce += 1;
    Ok(buf[..n].to_vec())
}

pub struct SecureConn {
    stream: TcpStream,
    ts: Arc<StatelessTransportState>,
    send_n: u64,
    recv_n: u64,
    buf: Vec<u8>,
}

pub struct Established {
    pub conn: SecureConn,
    /// The peer's static public key (authenticated by the handshake).
    pub remote_key: Vec<u8>,
    /// True when both sides proved knowledge of the pairing PIN (so the keys may be trusted).
    pub pin_paired: bool,
}

fn handshake(mut stream: TcpStream, private: &[u8], initiator: bool, psk: Option<[u8; 32]>) -> io::Result<Established> {
    let params = if psk.is_some() { PARAMS_PSK } else { PARAMS };
    let mut builder = Builder::new(params.parse().map_err(err)?).local_private_key(private);
    if let Some(k) = psk.as_ref() {
        builder = builder.psk(3, k);
    }
    let built = if initiator { builder.build_initiator() } else { builder.build_responder() };
    let mut hs = built.map_err(err)?;
    let mut buf = vec![0u8; 65535];
    while !hs.is_handshake_finished() {
        if hs.is_my_turn() {
            let n = hs.write_message(&[], &mut buf).map_err(err)?;
            write_frame(&mut stream, &buf[..n])?;
        } else {
            let m = read_frame(&mut stream)?;
            hs.read_message(&m, &mut buf).map_err(err)?;
        }
    }
    let remote_key = hs.get_remote_static().ok_or_else(|| err("no remote key"))?.to_vec();
    let ts = hs.into_stateless_transport_mode().map_err(err)?;
    Ok(Established {
        conn: SecureConn { stream, ts: Arc::new(ts), send_n: 0, recv_n: 0, buf },
        remote_key,
        pin_paired: psk.is_some(),
    })
}

/// Connect side. `pin = Some(..)` starts a pairing handshake, otherwise a normal one.
pub fn connect(mut stream: TcpStream, private: &[u8], pin: Option<&str>) -> io::Result<Established> {
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_nodelay(true)?;
    stream.write_all(&[if pin.is_some() { MODE_PAIRING } else { MODE_NORMAL }])?;
    handshake(stream, private, true, pin.map(pin_to_psk))
}

/// Accept side. `open_pin` is the currently valid pairing PIN, or None when pairing is closed (in
/// which case pairing handshakes are refused before any key exchange).
pub fn accept(mut stream: TcpStream, private: &[u8], open_pin: Option<&str>) -> io::Result<Established> {
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_nodelay(true)?;
    let mut mode = [0u8; 1];
    stream.read_exact(&mut mode)?;
    match mode[0] {
        MODE_NORMAL => handshake(stream, private, false, None),
        MODE_PAIRING => match open_pin {
            // Any failure here means the caller did not know the PIN; PermissionDenied tells the receiver to burn it.
            Some(pin) => handshake(stream, private, false, Some(pin_to_psk(pin)))
                .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, format!("pairing failed: {e}"))),
            None => Err(io::Error::new(io::ErrorKind::PermissionDenied, "pairing is closed")),
        },
        _ => Err(err("unknown connection mode")),
    }
}

impl SecureConn {
    pub fn send_raw(&mut self, data: &[u8]) -> io::Result<()> {
        seal(&self.ts, &mut self.send_n, &mut self.buf, &mut self.stream, data)
    }

    pub fn recv_raw(&mut self) -> io::Result<Vec<u8>> {
        open(&self.ts, &mut self.recv_n, &mut self.buf, &mut self.stream)
    }

    pub fn send(&mut self, msg: &Message) -> io::Result<()> {
        let body = bincode::serialize(msg).map_err(err)?;
        self.send_raw(&body)
    }

    pub fn recv(&mut self) -> io::Result<Message> {
        let body = self.recv_raw()?;
        bincode::deserialize(&body).map_err(err)
    }

    pub fn set_timeouts(&self, read: Option<Duration>, write: Option<Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(read)?;
        self.stream.set_write_timeout(write)
    }

    /// Split into independent reader/writer halves so two threads can pump data
    /// in both directions (each direction has its own key and nonce counter).
    pub fn split(self) -> io::Result<(SecureReader, SecureWriter)> {
        let wstream = self.stream.try_clone()?;
        Ok((
            SecureReader { stream: self.stream, ts: self.ts.clone(), n: self.recv_n, buf: vec![0u8; 65535] },
            SecureWriter { stream: wstream, ts: self.ts, n: self.send_n, buf: vec![0u8; 65535] },
        ))
    }
}

pub struct SecureReader {
    stream: TcpStream,
    ts: Arc<StatelessTransportState>,
    n: u64,
    buf: Vec<u8>,
}

impl SecureReader {
    pub fn recv_raw(&mut self) -> io::Result<Vec<u8>> {
        open(&self.ts, &mut self.n, &mut self.buf, &mut self.stream)
    }
    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

pub struct SecureWriter {
    stream: TcpStream,
    ts: Arc<StatelessTransportState>,
    n: u64,
    buf: Vec<u8>,
}

impl SecureWriter {
    pub fn send_raw(&mut self, data: &[u8]) -> io::Result<()> {
        seal(&self.ts, &mut self.n, &mut self.buf, &mut self.stream, data)
    }
    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InputEvent;
    use std::net::TcpListener;

    fn pair() -> (TcpStream, TcpListener) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        (TcpStream::connect(l.local_addr().unwrap()).unwrap(), l)
    }

    #[test]
    fn encrypted_roundtrip_and_key_authentication() {
        let (a_priv, a_pub) = generate_keypair().unwrap();
        let (b_priv, b_pub) = generate_keypair().unwrap();
        let (c, l) = pair();
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut e = accept(s, &b_priv, None).unwrap();
            let m = e.conn.recv().unwrap();
            (e.remote_key, e.pin_paired, matches!(m, Message::Input(InputEvent::Key { code: 4, pressed: true })))
        });
        let mut e = connect(c, &a_priv, None).unwrap();
        e.conn.send(&Message::Input(InputEvent::Key { code: 4, pressed: true })).unwrap();
        let (seen_key, paired, got) = t.join().unwrap();
        assert!(got && !paired);
        assert_eq!(seen_key, a_pub);
        assert_eq!(e.remote_key, b_pub);
    }

    #[test]
    fn pairing_with_the_right_pin_works() {
        let (a_priv, a_pub) = generate_keypair().unwrap();
        let (b_priv, b_pub) = generate_keypair().unwrap();
        let (c, l) = pair();
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let e = accept(s, &b_priv, Some("12345678")).unwrap();
            (e.remote_key, e.pin_paired)
        });
        let e = connect(c, &a_priv, Some("12345678")).unwrap();
        let (seen, paired) = t.join().unwrap();
        assert!(paired && e.pin_paired);
        assert_eq!(seen, a_pub);
        assert_eq!(e.remote_key, b_pub);
    }

    #[test]
    fn wrong_pin_cannot_pair() {
        let (a_priv, _) = generate_keypair().unwrap();
        let (b_priv, _) = generate_keypair().unwrap();
        let (c, l) = pair();
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            accept(s, &b_priv, Some("12345678")).is_err()
        });
        // the attacker guessed wrong; the receiver must reject it (the client side may or may not notice yet)
        let _ = connect(c, &a_priv, Some("00000000")).and_then(|mut e| e.conn.send(&Message::Ping));
        assert!(t.join().unwrap(), "receiver accepted a wrong PIN");
    }

    #[test]
    fn pairing_is_refused_when_closed() {
        let (a_priv, _) = generate_keypair().unwrap();
        let (b_priv, _) = generate_keypair().unwrap();
        let (c, l) = pair();
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            accept(s, &b_priv, None).is_err()
        });
        let _ = connect(c, &a_priv, Some("12345678"));
        assert!(t.join().unwrap());
    }

    #[test]
    fn split_halves_work_both_ways() {
        let (a_priv, _) = generate_keypair().unwrap();
        let (b_priv, _) = generate_keypair().unwrap();
        let (c, l) = pair();
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let e = accept(s, &b_priv, None).unwrap();
            let (mut r, mut w) = e.conn.split().unwrap();
            let got = r.recv_raw().unwrap();
            w.send_raw(&got).unwrap();
            w.send_raw(b"second").unwrap();
        });
        let e = connect(c, &a_priv, None).unwrap();
        let (mut r, mut w) = e.conn.split().unwrap();
        w.send_raw(b"hello").unwrap();
        assert_eq!(r.recv_raw().unwrap(), b"hello");
        assert_eq!(r.recv_raw().unwrap(), b"second");
        t.join().unwrap();
    }

    #[test]
    fn tampered_frame_is_rejected() {
        let (a_priv, _) = generate_keypair().unwrap();
        let (b_priv, _) = generate_keypair().unwrap();
        let (c, l) = pair();
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut e = accept(s, &b_priv, None).unwrap();
            e.conn.recv_raw().is_err()
        });
        let e = connect(c, &a_priv, None).unwrap();
        // write a garbage but well-framed message straight onto the socket
        let mut raw = e.conn;
        raw.stream.write_all(&[0, 20]).unwrap();
        raw.stream.write_all(&[7u8; 20]).unwrap();
        assert!(t.join().unwrap(), "forged frame was accepted");
    }

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    #[test]
    fn hex_roundtrip() {
        assert_eq!(from_hex(&to_hex(&[0, 1, 254, 255])).unwrap(), vec![0, 1, 254, 255]);
        assert!(from_hex("abc").is_none());
    }
}
