//! A very small D-Bus client (session bus, little-endian, EXTERNAL auth): just enough to own a bus name,
//! receive method calls whose arguments are strings, answer them, and call other services with string
//! arguments. Used to talk to KWin, which is the only thing on KDE Wayland that knows where the pointer is.
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub const METHOD_CALL: u8 = 1;
pub const METHOD_RETURN: u8 = 2;
pub const ERROR: u8 = 3;
const NO_REPLY_EXPECTED: u8 = 1;
const MAX_MSG: usize = 1 << 20;

#[derive(Debug, Default, Clone)]
pub struct Msg {
    pub kind: u8,
    pub flags: u8,
    pub serial: u32,
    pub path: String,
    pub interface: String,
    pub member: String,
    pub sender: String,
    pub destination: String,
    pub reply_serial: u32,
    pub signature: String,
    /// String arguments (only messages whose signature is all 's' are decoded).
    pub args: Vec<String>,
}

pub struct Bus {
    sock: BufReader<UnixStream>,
    serial: u32,
}

fn pad(buf: &mut Vec<u8>, align: usize) {
    while buf.len() % align != 0 {
        buf.push(0);
    }
}

fn put_str(buf: &mut Vec<u8>, s: &str) {
    pad(buf, 4);
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

fn put_sig(buf: &mut Vec<u8>, s: &str) {
    buf.push(s.len() as u8);
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

enum Field<'a> {
    Obj(u8, &'a str),
    Str(u8, &'a str),
    Sig(u8, &'a str),
    U32(u8, u32),
}

/// Encode a whole message. `body` must already be marshalled for `sig` (starting at an 8-aligned offset).
fn encode(kind: u8, flags: u8, serial: u32, fields: &[Field], sig: &str, body: &[u8]) -> Vec<u8> {
    let mut f: Vec<u8> = Vec::new();
    // field offsets are relative to the message start; the array data begins at offset 16 (8-aligned)
    for field in fields {
        pad(&mut f, 8);
        match field {
            Field::Obj(code, v) => {
                f.push(*code);
                put_sig(&mut f, "o");
                put_str(&mut f, v);
            }
            Field::Str(code, v) => {
                f.push(*code);
                put_sig(&mut f, "s");
                put_str(&mut f, v);
            }
            Field::Sig(code, v) => {
                f.push(*code);
                put_sig(&mut f, "g");
                put_sig(&mut f, v);
            }
            Field::U32(code, v) => {
                f.push(*code);
                put_sig(&mut f, "u");
                pad(&mut f, 4);
                f.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    let _ = sig;
    let mut m = vec![b'l', kind, flags, 1];
    m.extend_from_slice(&(body.len() as u32).to_le_bytes());
    m.extend_from_slice(&serial.to_le_bytes());
    m.extend_from_slice(&(f.len() as u32).to_le_bytes());
    m.extend_from_slice(&f);
    pad(&mut m, 8);
    m.extend_from_slice(body);
    m
}

/// Marshal string arguments (signature "s...s").
pub fn strings_body(args: &[&str]) -> Vec<u8> {
    let mut b = Vec::new();
    for a in args {
        put_str(&mut b, a);
    }
    b
}

struct Rd<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Rd<'_> {
    fn align(&mut self, a: usize) {
        self.pos = (self.pos + a - 1) / a * a;
    }
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }
    fn u32(&mut self) -> Option<u32> {
        self.align(4);
        let v = u32::from_le_bytes(self.b.get(self.pos..self.pos + 4)?.try_into().ok()?);
        self.pos += 4;
        Some(v)
    }
    fn string(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        let s = String::from_utf8_lossy(self.b.get(self.pos..self.pos.checked_add(n)?)?).to_string();
        self.pos += n + 1;
        Some(s)
    }
    fn sig(&mut self) -> Option<String> {
        let n = self.u8()? as usize;
        let s = String::from_utf8_lossy(self.b.get(self.pos..self.pos + n)?).to_string();
        self.pos += n + 1;
        Some(s)
    }
}

/// Decode one complete message (`raw` = header + body).
pub fn decode(raw: &[u8]) -> Option<Msg> {
    if raw.len() < 16 || raw[0] != b'l' {
        return None;
    }
    let mut m = Msg { kind: raw[1], flags: raw[2], ..Default::default() };
    let body_len = u32::from_le_bytes(raw[4..8].try_into().ok()?) as usize;
    m.serial = u32::from_le_bytes(raw[8..12].try_into().ok()?);
    let fields_len = u32::from_le_bytes(raw[12..16].try_into().ok()?) as usize;
    let fields_end = 16usize.checked_add(fields_len)?;
    let mut r = Rd { b: raw.get(..fields_end)?, pos: 16 };
    while r.pos < fields_end {
        r.align(8);
        if r.pos >= fields_end {
            break;
        }
        let code = r.u8()?;
        let sig = r.sig()?;
        match sig.as_str() {
            "o" | "s" => {
                let v = r.string()?;
                match code {
                    1 => m.path = v,
                    2 => m.interface = v,
                    3 => m.member = v,
                    6 => m.destination = v,
                    7 => m.sender = v,
                    _ => {}
                }
            }
            "g" => {
                let v = r.sig()?;
                if code == 8 {
                    m.signature = v;
                }
            }
            "u" => {
                let v = r.u32()?;
                if code == 5 {
                    m.reply_serial = v;
                }
            }
            _ => return None,
        }
    }
    let body_start = (fields_end + 7) / 8 * 8;
    let body = raw.get(body_start..body_start.checked_add(body_len)?)?;
    if !m.signature.is_empty() && m.signature.chars().all(|c| c == 's') {
        let mut br = Rd { b: body, pos: 0 };
        for _ in m.signature.chars() {
            m.args.push(br.string()?);
        }
    }
    Some(m)
}

fn session_bus_path() -> String {
    if let Ok(a) = std::env::var("DBUS_SESSION_BUS_ADDRESS") {
        if let Some(p) = a.split(',').next().and_then(|x| x.strip_prefix("unix:path=")) {
            return p.to_string();
        }
    }
    let uid = unsafe { libc::getuid() };
    let run = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{uid}"));
    format!("{run}/bus")
}

impl Bus {
    /// Connect to the session bus of the user this program runs as.
    pub fn session() -> io::Result<Bus> {
        let mut s = UnixStream::connect(session_bus_path())?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        let uid = unsafe { libc::getuid() }.to_string();
        let hex: String = uid.bytes().map(|b| format!("{b:02x}")).collect();
        s.write_all(format!("\0AUTH EXTERNAL {hex}\r\n").as_bytes())?;
        let mut rd = BufReader::new(s);
        let mut line = String::new();
        rd.read_line(&mut line)?;
        if !line.starts_with("OK") {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("D-Bus refused: {}", line.trim())));
        }
        rd.get_mut().write_all(b"BEGIN\r\n")?;
        let mut bus = Bus { sock: rd, serial: 0 };
        bus.call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "Hello", &[])?;
        Ok(bus)
    }

    fn next_serial(&mut self) -> u32 {
        self.serial += 1;
        self.serial
    }

    fn send_call(&mut self, dest: &str, path: &str, iface: &str, member: &str, args: &[&str], flags: u8) -> io::Result<u32> {
        let serial = self.next_serial();
        let sig = "s".repeat(args.len());
        let mut fields = vec![Field::Obj(1, path), Field::Str(2, iface), Field::Str(3, member), Field::Str(6, dest)];
        if !args.is_empty() {
            fields.push(Field::Sig(8, &sig));
        }
        let msg = encode(METHOD_CALL, flags, serial, &fields, &sig, &strings_body(args));
        self.sock.get_mut().write_all(&msg)?;
        Ok(serial)
    }

    /// Call a method with string arguments and wait for its answer (other messages arriving meanwhile are dropped).
    pub fn call(&mut self, dest: &str, path: &str, iface: &str, member: &str, args: &[&str]) -> io::Result<Msg> {
        let serial = self.send_call(dest, path, iface, member, args, 0)?;
        loop {
            let m = self.read()?;
            if (m.kind == METHOD_RETURN || m.kind == ERROR) && m.reply_serial == serial {
                if m.kind == ERROR {
                    return Err(io::Error::new(io::ErrorKind::Other, format!("{member}: {}", m.args.first().cloned().unwrap_or_default())));
                }
                return Ok(m);
            }
        }
    }

    /// RequestName with DO_NOT_QUEUE | REPLACE_EXISTING; true if we own the name.
    pub fn own_name(&mut self, name: &str) -> io::Result<bool> {
        let serial = self.next_serial();
        let mut body = strings_body(&[name]);
        pad(&mut body, 4);
        body.extend_from_slice(&6u32.to_le_bytes());
        let fields = [Field::Obj(1, "/org/freedesktop/DBus"), Field::Str(2, "org.freedesktop.DBus"), Field::Str(3, "RequestName"), Field::Str(6, "org.freedesktop.DBus"), Field::Sig(8, "su")];
        let msg = encode(METHOD_CALL, 0, serial, &fields, "su", &body);
        self.sock.get_mut().write_all(&msg)?;
        loop {
            let m = self.read_raw()?;
            if let Some(d) = decode(&m) {
                if d.reply_serial == serial {
                    // reply body is one uint32: 1 = primary owner, 4 = already owner
                    let code = m.get(m.len().saturating_sub(4)..).and_then(|b| b.try_into().ok()).map(u32::from_le_bytes).unwrap_or(0);
                    return Ok(d.kind == METHOD_RETURN && (code == 1 || code == 4));
                }
            }
        }
    }

    /// Answer a method call we received (empty reply), unless the caller asked for no reply.
    pub fn reply(&mut self, to: &Msg) -> io::Result<()> {
        if to.flags & NO_REPLY_EXPECTED != 0 || to.sender.is_empty() {
            return Ok(());
        }
        let serial = self.next_serial();
        let fields = [Field::U32(5, to.serial), Field::Str(6, &to.sender)];
        let msg = encode(METHOD_RETURN, 0, serial, &fields, "", &[]);
        self.sock.get_mut().write_all(&msg)
    }

    pub fn set_timeout(&mut self, t: Option<Duration>) -> io::Result<()> {
        self.sock.get_ref().set_read_timeout(t)
    }

    fn read_raw(&mut self) -> io::Result<Vec<u8>> {
        let mut head = [0u8; 16];
        self.sock.read_exact(&mut head)?;
        if head[0] != b'l' {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "big-endian D-Bus messages are not supported"));
        }
        let body_len = u32::from_le_bytes(head[4..8].try_into().unwrap()) as usize;
        let fields_len = u32::from_le_bytes(head[12..16].try_into().unwrap()) as usize;
        let total = (16 + fields_len + 7) / 8 * 8 + body_len;
        if total > MAX_MSG {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "D-Bus message too large"));
        }
        let mut raw = head.to_vec();
        raw.resize(total, 0);
        self.sock.read_exact(&mut raw[16..])?;
        Ok(raw)
    }

    /// Read the next message.
    pub fn read(&mut self) -> io::Result<Msg> {
        let raw = self.read_raw()?;
        decode(&raw).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed D-Bus message"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let sig = "ss";
        let fields = [Field::Obj(1, "/Edge"), Field::Str(2, "org.smoothkvm.Edge"), Field::Str(3, "Edge"), Field::Str(7, ":1.5"), Field::Sig(8, sig)];
        let raw = encode(METHOD_CALL, 0, 9, &fields, sig, &strings_body(&["1 500", "x"]));
        let m = decode(&raw).unwrap();
        assert_eq!((m.kind, m.serial), (METHOD_CALL, 9));
        assert_eq!(m.path, "/Edge");
        assert_eq!(m.interface, "org.smoothkvm.Edge");
        assert_eq!(m.member, "Edge");
        assert_eq!(m.sender, ":1.5");
        assert_eq!(m.args, vec!["1 500", "x"]);
    }

    #[test]
    fn garbage_is_rejected_not_panicking() {
        for n in 0..64 {
            let junk: Vec<u8> = (0..n).map(|i| (i * 37 % 251) as u8).collect();
            let _ = decode(&junk);
        }
        let mut raw = encode(METHOD_CALL, 0, 1, &[Field::Str(3, "x")], "", &[]);
        raw[12] = 0xff; // absurd field length
        assert!(decode(&raw).is_none());
    }

    /// Talks to a real dbus-daemon (skipped when there is none).
    #[test]
    fn real_bus_receives_a_call() {
        let Ok(mut child) = std::process::Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            return;
        };
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let addr = line.trim().to_string();
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &addr);
        let mut bus = Bus::session().expect("connect");
        assert!(bus.own_name("org.smoothkvm.Test").unwrap());
        let out = std::process::Command::new("dbus-send")
            .args([&format!("--bus={addr}"), "--print-reply", "--dest=org.smoothkvm.Test", "/Edge", "org.smoothkvm.Edge.Edge", "string:1 250"])
            .spawn()
            .unwrap();
        let m = loop {
            let m = bus.read().unwrap();
            if m.kind == METHOD_CALL && m.member == "Edge" {
                break m;
            }
        };
        assert_eq!(m.args, vec!["1 250"]);
        bus.reply(&m).unwrap();
        let st = out.wait_with_output().unwrap();
        assert!(st.status.success(), "dbus-send got our reply");
        let _ = child.kill();
    }
}
