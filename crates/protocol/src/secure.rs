//! Encrypted, mutually authenticated transport.
//!
//! Noise_XX_25519_ChaChaPoly_BLAKE2s: both sides prove possession of a static
//! X25519 key, all traffic is encrypted + authenticated (ChaCha20-Poly1305),
//! sessions have forward secrecy, and nonces are counters so replays fail.
//! Which keys are *trusted* is decided by the caller (pairing / pinned keys).
use crate::Message;
use snow::{Builder, TransportState};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

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

pub struct SecureConn {
    stream: TcpStream,
    ts: TransportState,
    buf: Vec<u8>,
}

pub struct Established {
    pub conn: SecureConn,
    /// The peer's static public key (authenticated by the handshake).
    pub remote_key: Vec<u8>,
    /// Short code both sides can show to a human to rule out a man-in-the-middle.
    pub sas: String,
}

fn handshake(mut stream: TcpStream, private: &[u8], initiator: bool) -> io::Result<Established> {
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_nodelay(true)?;
    let builder = Builder::new(PARAMS.parse().map_err(err)?).local_private_key(private);
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
    let hash = hs.get_handshake_hash().to_vec();
    let ts = hs.into_transport_mode().map_err(err)?;
    let n = u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]) % 1_000_000;
    let sas = format!("{:03} {:03}", n / 1000, n % 1000);
    Ok(Established { conn: SecureConn { stream, ts, buf }, remote_key, sas })
}

pub fn connect(stream: TcpStream, private: &[u8]) -> io::Result<Established> {
    handshake(stream, private, true)
}

pub fn accept(stream: TcpStream, private: &[u8]) -> io::Result<Established> {
    handshake(stream, private, false)
}

impl SecureConn {
    pub fn send_raw(&mut self, data: &[u8]) -> io::Result<()> {
        let n = self.ts.write_message(data, &mut self.buf).map_err(err)?;
        write_frame(&mut self.stream, &self.buf[..n])
    }

    pub fn recv_raw(&mut self) -> io::Result<Vec<u8>> {
        let ct = read_frame(&mut self.stream)?;
        let n = self.ts.read_message(&ct, &mut self.buf).map_err(err)?;
        Ok(self.buf[..n].to_vec())
    }

    pub fn send(&mut self, msg: &Message) -> io::Result<()> {
        let body = bincode::serialize(msg).map_err(err)?;
        self.send_raw(&body)
    }

    pub fn recv(&mut self) -> io::Result<Message> {
        let body = self.recv_raw()?;
        bincode::deserialize(&body).map_err(err)
    }

    /// Both sides send their verdict; pairing succeeds only if both said yes.
    pub fn confirm_pairing(&mut self, local_ok: bool) -> io::Result<bool> {
        self.send_raw(if local_ok { b"Y" } else { b"N" })?;
        let theirs = self.recv_raw()?;
        Ok(local_ok && theirs == b"Y")
    }

    pub fn set_timeouts(&self, read: Option<Duration>, write: Option<Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(read)?;
        self.stream.set_write_timeout(write)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InputEvent;
    use std::net::TcpListener;

    #[test]
    fn encrypted_roundtrip_and_matching_sas() {
        let (a_priv, a_pub) = generate_keypair().unwrap();
        let (b_priv, b_pub) = generate_keypair().unwrap();
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut e = accept(s, &b_priv).unwrap();
            let m = e.conn.recv().unwrap();
            (e.remote_key, e.sas, matches!(m, Message::Input(InputEvent::Key { code: 4, pressed: true })))
        });
        let mut e = connect(TcpStream::connect(addr).unwrap(), &a_priv).unwrap();
        e.conn.send(&Message::Input(InputEvent::Key { code: 4, pressed: true })).unwrap();
        let (seen_key, sas_b, got) = t.join().unwrap();
        assert!(got);
        assert_eq!(seen_key, a_pub);
        assert_eq!(e.remote_key, b_pub);
        assert_eq!(e.sas, sas_b);
    }

    #[test]
    fn hex_roundtrip() {
        assert_eq!(from_hex(&to_hex(&[0, 1, 254, 255])).unwrap(), vec![0, 1, 254, 255]);
        assert!(from_hex("abc").is_none());
    }
}
