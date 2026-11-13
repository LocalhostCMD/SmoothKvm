//! Sending files between paired computers.
//!
//! A transfer is its own encrypted session (never the keyboard/mouse one), so a 10 GB copy cannot make
//! the mouse lag. Wire format after the normal handshake:
//!
//!   sender:   SendFiles{count,total}
//!   receiver: FilesResult{ok,..}               (refuses up front if file drop is off: nothing is sent)
//!   sender:   FileStart{path,size}  raw frames of exactly `size` bytes   (repeated)
//!   sender:   FilesEnd
//!   receiver: FilesResult{ok,detail}
//!
//! Every frame is authenticated by the transport (ChaCha20-Poly1305 with strict nonces), so a file that
//! arrives complete is byte-for-byte what was sent. Files are written as `name.part` and renamed only
//! once complete, land in Downloads/SmoothKvm, never overwrite anything and are never opened or run.
use crate::store::{self, Identity};
use crate::{net, shared};
use kvm_protocol::secure::{SecureConn, MAX_PLAINTEXT};
use kvm_protocol::Message;
use serde::Serialize;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::sync_channel;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_FILES: u32 = 100_000;
/// Raw data frames: as large as one Noise frame allows.
const CHUNK: usize = MAX_PLAINTEXT;

// ---- progress, shown by the app and the terminal UI ----

#[derive(Clone, Serialize)]
pub struct Progress {
    /// "send" | "receive"
    pub dir: &'static str,
    pub peer: String,
    pub file: String,
    pub files: u32,
    pub done_bytes: u64,
    pub total_bytes: u64,
    /// Bytes per second, averaged over the whole transfer.
    pub rate: u64,
    pub finished: bool,
    pub ok: bool,
    pub detail: String,
}

static PROGRESS: Mutex<Option<Progress>> = Mutex::new(None);

pub fn progress() -> Option<Progress> {
    PROGRESS.lock().ok().and_then(|g| g.clone())
}

struct Tracker {
    started: Instant,
    last: Instant,
}

impl Tracker {
    fn begin(dir: &'static str, peer: &str, files: u32, total: u64) -> Self {
        set(Progress { dir, peer: peer.into(), file: String::new(), files, done_bytes: 0, total_bytes: total, rate: 0, finished: false, ok: false, detail: String::new() });
        let now = Instant::now();
        Tracker { started: now, last: now }
    }
    fn file(&self, name: &str) {
        update(|p| p.file = shared::clean(name));
    }
    /// Cheap enough to call per chunk: the shared state is touched at most 10 times a second.
    fn bytes(&mut self, done: u64) {
        if self.last.elapsed() < Duration::from_millis(100) {
            return;
        }
        self.last = Instant::now();
        let rate = (done as f64 / self.started.elapsed().as_secs_f64().max(0.001)) as u64;
        update(|p| {
            p.done_bytes = done;
            p.rate = rate;
        });
    }
    fn end(&self, done: u64, ok: bool, detail: &str) {
        let rate = (done as f64 / self.started.elapsed().as_secs_f64().max(0.001)) as u64;
        update(|p| {
            p.done_bytes = done;
            p.rate = rate;
            p.finished = true;
            p.ok = ok;
            p.detail = shared::clean(detail);
        });
    }
}

fn set(p: Progress) {
    if let Ok(mut g) = PROGRESS.lock() {
        *g = Some(p);
    }
}

fn update(f: impl FnOnce(&mut Progress)) {
    if let Ok(mut g) = PROGRESS.lock() {
        if let Some(p) = g.as_mut() {
            f(p);
        }
    }
}

pub fn human(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.2} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} KB", b / 1e3)
    } else {
        format!("{bytes} B")
    }
}

// ---- where files can go ----

#[derive(Clone, Serialize, Debug)]
pub struct Target {
    pub name: String,
    pub addr: String,
    /// "screen" (one of our screens) or "controller" (a computer controlling this one).
    pub kind: &'static str,
}

/// Every computer we can send to right now: our connected screens, and computers controlling this one.
pub fn targets() -> Vec<Target> {
    let cfg = store::load_config();
    let st = shared::statuses();
    let mut out: Vec<Target> = cfg
        .remotes
        .iter()
        .filter(|r| r.enabled && !r.addr.trim().is_empty())
        .filter(|r| st.get(&r.name).map(|s| s.state == "connected").unwrap_or(false))
        .map(|r| Target { name: r.name.clone(), addr: r.addr.clone(), kind: "screen" })
        .collect();
    for (peer, addr) in shared::controllers() {
        let ip = addr.rsplit_once(':').map(|x| x.0.trim_matches(['[', ']'])).unwrap_or("").to_string();
        // the same machine may be both one of our screens and controlling us: list it once
        if out.iter().any(|t| t.addr.split(':').next() == Some(ip.as_str())) {
            continue;
        }
        let name = screen_name_for_ip(&ip).unwrap_or(peer);
        out.push(Target { name, addr, kind: "controller" });
    }
    out
}

/// A friendlier label than the bare address when one of our screens points at the same machine.
fn screen_name_for_ip(ip: &str) -> Option<String> {
    store::load_config().remotes.into_iter().find(|r| r.addr.split(':').next() == Some(ip)).map(|r| r.name)
}

/// `name` = None picks the only/first target.
pub fn find_target(name: Option<&str>) -> io::Result<Target> {
    let all = targets();
    let pick = match name.map(|n| n.trim().to_lowercase()).filter(|n| !n.is_empty()) {
        Some(n) => all.into_iter().find(|t| t.name.to_lowercase() == n || t.addr.to_lowercase().starts_with(&n)),
        None => all.into_iter().next(),
    };
    pick.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no computer to send to: connect to a screen, or have the other computer control this one (both need SmoothKvm 0.8+)",
        )
    })
}

// ---- sending ----

/// (absolute path, relative '/' path, size) for every regular file under `paths` (folders are walked;
/// symbolic links are skipped so a transfer can never wander outside what you dropped).
fn collect(paths: &[PathBuf]) -> io::Result<Vec<(PathBuf, String, u64)>> {
    fn walk(abs: &Path, rel: String, out: &mut Vec<(PathBuf, String, u64)>) -> io::Result<()> {
        let md = fs::symlink_metadata(abs)?;
        if md.file_type().is_symlink() {
            return Ok(());
        }
        if md.is_file() {
            out.push((abs.to_path_buf(), rel, md.len()));
        } else if md.is_dir() {
            let mut entries: Vec<_> = fs::read_dir(abs)?.filter_map(|e| e.ok()).collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                walk(&e.path(), format!("{rel}/{}", e.file_name().to_string_lossy()), out)?;
            }
        }
        if out.len() > MAX_FILES as usize {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("more than {MAX_FILES} files")));
        }
        Ok(())
    }
    let mut out = Vec::new();
    for p in paths {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("cannot send {}", p.display())))?;
        walk(p, name, &mut out)?;
    }
    if out.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "nothing to send (no regular files)"));
    }
    Ok(out)
}

/// Send files/folders to `target`. Blocks until the other side confirms; returns a summary.
pub fn send(target: &Target, paths: &[PathBuf], ident: &Identity) -> io::Result<String> {
    let files = collect(paths)?;
    let total: u64 = files.iter().map(|f| f.2).sum();
    let count = files.len() as u32;
    let mut tr = Tracker::begin("send", &target.name, count, total);
    let res = send_inner(target, &files, total, ident, &mut tr);
    match &res {
        Ok(d) => {
            tr.end(total, true, d);
            shared::info(&format!("Sent {count} file(s), {} to {}", human(total), target.name));
        }
        Err(e) => {
            tr.end(0, false, &e.to_string());
            shared::warn(&format!("Sending files to {} failed: {e}", target.name));
        }
    }
    res
}

fn send_inner(target: &Target, files: &[(PathBuf, String, u64)], total: u64, ident: &Identity, tr: &mut Tracker) -> io::Result<String> {
    let mut conn = net::connect(&target.addr, ident, None, &target.name)?;
    conn.set_timeouts(Some(Duration::from_secs(60)), Some(Duration::from_secs(30)))?;
    conn.send(&Message::SendFiles { count: files.len() as u32, total })?;
    match conn.recv()? {
        Message::FilesResult { ok: true, .. } => {}
        Message::FilesResult { ok: false, detail } => return Err(io::Error::new(io::ErrorKind::PermissionDenied, shared::clean(&detail))),
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected reply (is the other computer on SmoothKvm 0.8+?)")),
    }
    push_files(&mut conn, files, tr)?;
    conn.send(&Message::FilesEnd)?;
    // the receiver may still be flushing a large file to disk
    conn.set_timeouts(Some(Duration::from_secs(300)), None)?;
    match conn.recv()? {
        Message::FilesResult { ok: true, detail } => Ok(shared::clean(&detail)),
        Message::FilesResult { ok: false, detail } => Err(io::Error::new(io::ErrorKind::Other, shared::clean(&detail))),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected reply")),
    }
}

/// Stream every file (FileStart + raw chunks); FilesEnd is sent by the caller.
fn push_files(conn: &mut SecureConn, files: &[(PathBuf, String, u64)], tr: &mut Tracker) -> io::Result<()> {
    // Disk reads run on their own thread, a few chunks ahead of the network, so neither waits for the other.
    let (tx, rx) = sync_channel::<io::Result<Vec<u8>>>(16);
    let list: Vec<(PathBuf, u64)> = files.iter().map(|f| (f.0.clone(), f.2)).collect();
    let reader = std::thread::spawn(move || {
        for (path, size) in list {
            let res = (|| -> io::Result<()> {
                let mut f = File::open(&path)?.take(size);
                let mut left = size;
                while left > 0 {
                    let mut buf = vec![0u8; CHUNK.min(left as usize)];
                    f.read_exact(&mut buf).map_err(|_| io::Error::new(io::ErrorKind::UnexpectedEof, format!("{} changed while sending", path.display())))?;
                    left -= buf.len() as u64;
                    if tx.send(Ok(buf)).is_err() {
                        return Ok(());
                    }
                }
                Ok(())
            })();
            if let Err(e) = res {
                let _ = tx.send(Err(e));
                return;
            }
        }
    });
    let mut done = 0u64;
    let mut result = Ok(());
    'files: for (_, rel, size) in files {
        tr.file(rel);
        if let Err(e) = conn.send(&Message::FileStart { path: rel.clone(), size: *size }) {
            result = Err(e);
            break;
        }
        let mut left = *size;
        while left > 0 {
            let chunk = match rx.recv() {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    result = Err(e);
                    break 'files;
                }
                Err(_) => {
                    result = Err(io::Error::new(io::ErrorKind::Other, "file reader stopped"));
                    break 'files;
                }
            };
            if let Err(e) = conn.send_raw(&chunk) {
                result = Err(e);
                break 'files;
            }
            left -= chunk.len() as u64;
            done += chunk.len() as u64;
            tr.bytes(done);
        }
    }
    drop(rx); // lets the reader thread finish if we stopped early
    let _ = reader.join();
    result
}

/// Start a transfer on a background thread (used by the app, the terminal UI and the drop strip).
pub fn send_async(target: Option<String>, paths: Vec<PathBuf>, ident: std::sync::Arc<Identity>) -> io::Result<Target> {
    if let Some(p) = progress() {
        if !p.finished {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "a transfer is already running"));
        }
    }
    for p in &paths {
        if !p.exists() {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("not found: {}", p.display())));
        }
    }
    let t = find_target(target.as_deref())?;
    // mark it as running right away, so nobody polling sees the previous (finished) transfer meanwhile
    set(Progress { dir: "send", peer: t.name.clone(), file: "preparing...".into(), files: 0, done_bytes: 0, total_bytes: 0, rate: 0, finished: false, ok: false, detail: String::new() });
    let t2 = t.clone();
    std::thread::spawn(move || {
        let _ = send(&t2, &paths, &ident);
    });
    Ok(t)
}

// ---- receiving ----

pub fn downloads_dir() -> PathBuf {
    #[cfg(windows)]
    {
        use windows::Win32::UI::Shell::{FOLDERID_Downloads, SHGetKnownFolderPath, KF_FLAG_DEFAULT};
        if let Ok(p) = unsafe { SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, None) } {
            let s = unsafe { p.to_string() }.unwrap_or_default();
            unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _)) };
            if !s.is_empty() {
                return PathBuf::from(s);
            }
        }
        std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("Downloads")).unwrap_or_else(|| PathBuf::from("."))
    }
    #[cfg(not(windows))]
    {
        if let Ok(o) = std::process::Command::new("xdg-user-dir").arg("DOWNLOAD").output() {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            // with no user-dirs config it answers $HOME itself: then use ~/Downloads instead
            let home = std::env::var("HOME").unwrap_or_default();
            if o.status.success() && s.starts_with('/') && s.trim_end_matches('/') != home.trim_end_matches('/') && Path::new(&s).is_dir() {
                return PathBuf::from(s);
            }
        }
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Downloads")).unwrap_or_else(|| PathBuf::from("."))
    }
}

/// Where received files are saved: the folder chosen in the settings, or Downloads/SmoothKvm.
pub fn inbox() -> PathBuf {
    let chosen = store::load_config().inbox_dir;
    let chosen = chosen.trim();
    if !chosen.is_empty() && Path::new(chosen).is_absolute() {
        return PathBuf::from(chosen);
    }
    downloads_dir().join("SmoothKvm")
}

/// Turn a path from the network into safe relative components, or refuse it. Nothing may escape the
/// inbox ("..", absolute paths, drive letters, device names) and names must be valid on this OS.
pub fn safe_rel(path: &str) -> Option<Vec<String>> {
    const RESERVED: &[&str] = &["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9"];
    if path.len() > 4096 {
        return None;
    }
    let mut out = Vec::new();
    for raw in path.split('/') {
        if raw.is_empty() || raw == "." || raw == ".." {
            return None;
        }
        let mut c: String = raw
            .chars()
            .map(|ch| if ch.is_control() || matches!(ch, '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*') { '_' } else { ch })
            .collect();
        // Windows silently drops trailing dots/spaces, which could make two names collide
        while c.ends_with('.') || c.ends_with(' ') {
            c.pop();
        }
        if c.is_empty() || c.chars().all(|ch| ch == '.') || c.len() > 255 {
            return None;
        }
        let stem = c.split('.').next().unwrap_or("").to_ascii_uppercase();
        if RESERVED.contains(&stem.trim_end()) {
            c.insert(0, '_');
        }
        out.push(c);
    }
    if out.is_empty() || out.len() > 64 {
        return None;
    }
    Some(out)
}

/// "report.pdf" -> "report (2).pdf" until the name is free.
fn unique(dir: &Path, name: &str) -> String {
    if !dir.join(name).exists() && !dir.join(format!("{name}.part")).exists() {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 2..10_000 {
        let cand = format!("{stem} ({n}){ext}");
        if !dir.join(&cand).exists() && !dir.join(format!("{cand}.part")).exists() {
            return cand;
        }
    }
    format!("{stem} ({}){ext}", std::process::id())
}

/// Receive a transfer (the session's first message was SendFiles).
pub fn receive(mut conn: SecureConn, peer: &str, count: u32, total: u64) -> io::Result<()> {
    if !store::load_config().file_drop {
        shared::info(&format!("{peer} tried to send files; refused because receiving files is off"));
        return conn.send(&Message::FilesResult { ok: false, detail: "that computer does not accept files (file drop is off)".into() });
    }
    if count == 0 || count > MAX_FILES {
        return conn.send(&Message::FilesResult { ok: false, detail: "bad file count".into() });
    }
    let root = inbox();
    if let Err(e) = fs::create_dir_all(&root) {
        return conn.send(&Message::FilesResult { ok: false, detail: format!("cannot create {}: {e}", root.display()) });
    }
    conn.set_timeouts(Some(Duration::from_secs(60)), Some(Duration::from_secs(30)))?;
    conn.send(&Message::FilesResult { ok: true, detail: "ready".into() })?;
    shared::info(&format!("Receiving {count} file(s), {} from {peer}", human(total)));
    let mut tr = Tracker::begin("receive", peer, count, total);
    let mut saved: Vec<PathBuf> = Vec::new();
    let res = receive_inner(&mut conn, &root, count, total, &mut tr, &mut saved);
    match res {
        Ok(done) => {
            let first = saved.first().and_then(|p| p.strip_prefix(&root).ok()).and_then(|p| p.components().next()).map(|c| c.as_os_str().to_string_lossy().to_string()).unwrap_or_default();
            let what = if saved.len() == 1 { first } else { format!("{} files", saved.len()) };
            let detail = format!("saved {what} ({}) to {}", human(done), root.display());
            tr.end(done, true, &detail);
            shared::notify(&format!("Received {what} from {peer} -> {}", root.display()));
            conn.send(&Message::FilesResult { ok: true, detail })?;
        }
        Err(e) => {
            tr.end(0, false, &e.to_string());
            shared::warn(&format!("Receiving files from {peer} failed: {e}"));
            let _ = conn.send(&Message::FilesResult { ok: false, detail: e.to_string() });
        }
    }
    Ok(())
}

fn receive_inner(conn: &mut SecureConn, root: &Path, count: u32, total: u64, tr: &mut Tracker, saved: &mut Vec<PathBuf>) -> io::Result<u64> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    // A dropped folder "Photos" becomes "Photos (2)" if Photos already exists; every file inside follows it.
    let mut top: HashMap<String, String> = HashMap::new();
    let mut done = 0u64;
    let mut n = 0u32;
    loop {
        match conn.recv()? {
            Message::FilesEnd => break,
            Message::FileStart { path, size } => {
                n += 1;
                if n > count {
                    return Err(bad("more files than announced"));
                }
                if done.saturating_add(size) > total {
                    return Err(bad("more data than announced"));
                }
                let parts = safe_rel(&path).ok_or_else(|| bad("refused an unsafe file name"))?;
                let first = top.entry(parts[0].clone()).or_insert_with(|| unique(root, &parts[0])).clone();
                let mut dest = root.join(&first);
                for p in &parts[1..] {
                    dest.push(p);
                }
                if let Some(dir) = dest.parent() {
                    fs::create_dir_all(dir)?;
                }
                // inside a renamed folder names are already free, but stay careful
                let file_name = dest.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
                let dest = dest.with_file_name(unique(dest.parent().unwrap_or(root), &file_name));
                let part = dest.with_file_name(format!("{}.part", dest.file_name().unwrap_or_default().to_string_lossy()));
                tr.file(&path);
                let res = (|| -> io::Result<()> {
                    let mut w = BufWriter::with_capacity(1 << 20, File::create(&part)?);
                    let mut left = size;
                    while left > 0 {
                        let chunk = conn.recv_raw()?;
                        if chunk.len() as u64 > left {
                            return Err(bad("file is longer than announced"));
                        }
                        w.write_all(&chunk)?;
                        left -= chunk.len() as u64;
                        done += chunk.len() as u64;
                        tr.bytes(done);
                    }
                    w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
                    fs::rename(&part, &dest)
                })();
                if let Err(e) = res {
                    let _ = fs::remove_file(&part);
                    return Err(e);
                }
                saved.push(dest);
            }
            _ => return Err(bad("unexpected message during a file transfer")),
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_paths_are_refused() {
        for p in ["", "/etc/passwd", "../x", "a/../../x", "a//b", ".", "a/./b", "...", "a/.."] {
            assert!(safe_rel(p).is_none(), "{p:?} should be refused");
        }
    }

    #[test]
    fn names_are_made_valid() {
        assert_eq!(safe_rel("Photos/img 1.jpg").unwrap(), vec!["Photos", "img 1.jpg"]);
        assert_eq!(safe_rel("C:evil").unwrap(), vec!["C_evil"]);
        assert_eq!(safe_rel("a\\b").unwrap(), vec!["a_b"]);
        assert_eq!(safe_rel("CON.txt").unwrap(), vec!["_CON.txt"]);
        assert_eq!(safe_rel("trail. . ").unwrap(), vec!["trail"]);
        assert_eq!(safe_rel("tab\there").unwrap(), vec!["tab_here"]);
    }

    /// Full transfer over a real encrypted loopback connection: a folder with a big file, an empty file and
    /// a name that already exists on the receiving side.
    #[test]
    fn transfer_roundtrip() {
        use kvm_protocol::secure;
        let base = std::env::temp_dir().join(format!("skvm-xfer-{}", std::process::id()));
        let (src, dst) = (base.join("src"), base.join("dst"));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(src.join("Proj/sub")).unwrap();
        fs::create_dir_all(&dst).unwrap();
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        fs::write(src.join("Proj/sub/big.bin"), &big).unwrap();
        fs::write(src.join("Proj/empty.txt"), b"").unwrap();
        fs::write(src.join("note.txt"), b"hello").unwrap();
        fs::write(dst.join("note.txt"), b"already here").unwrap();

        let files = collect(&[src.join("Proj"), src.join("note.txt")]).unwrap();
        assert_eq!(files.len(), 3);
        let total: u64 = files.iter().map(|f| f.2).sum();

        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let (a_priv, _) = secure::generate_keypair().unwrap();
        let (b_priv, _) = secure::generate_keypair().unwrap();
        let dst2 = dst.clone();
        let rx = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut conn = secure::accept(s, &b_priv, None).unwrap().conn;
            let mut tr = Tracker::begin("receive", "t", 3, total);
            let mut saved = Vec::new();
            receive_inner(&mut conn, &dst2, 3, total, &mut tr, &mut saved).unwrap()
        });
        let mut conn = secure::connect(std::net::TcpStream::connect(addr).unwrap(), &a_priv, None).unwrap().conn;
        let mut tr = Tracker::begin("send", "t", 3, total);
        push_files(&mut conn, &files, &mut tr).unwrap();
        conn.send(&Message::FilesEnd).unwrap();
        assert_eq!(rx.join().unwrap(), total);

        assert_eq!(fs::read(dst.join("Proj/sub/big.bin")).unwrap(), big);
        assert_eq!(fs::read(dst.join("Proj/empty.txt")).unwrap(), b"");
        assert_eq!(fs::read(dst.join("note.txt")).unwrap(), b"already here", "existing file must not be overwritten");
        assert_eq!(fs::read(dst.join("note (2).txt")).unwrap(), b"hello");
        let _ = fs::remove_dir_all(&base);
    }

    /// Throughput over loopback: `cargo test --release -p kvmd xfer_speed -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn xfer_speed() {
        use kvm_protocol::secure;
        let base = std::env::temp_dir().join(format!("skvm-speed-{}", std::process::id()));
        let (src, dst) = (base.join("src"), base.join("dst"));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        let size = 512u64 << 20;
        let blob: Vec<u8> = (0..size as u32).map(|i| (i.wrapping_mul(2654435761) >> 11) as u8).collect();
        fs::write(src.join("big.bin"), &blob).unwrap();
        drop(blob);
        let files = collect(&[src.join("big.bin")]).unwrap();
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let (a_priv, _) = secure::generate_keypair().unwrap();
        let (b_priv, _) = secure::generate_keypair().unwrap();
        let dst2 = dst.clone();
        let rx = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut conn = secure::accept(s, &b_priv, None).unwrap().conn;
            let mut tr = Tracker::begin("receive", "t", 1, size);
            receive_inner(&mut conn, &dst2, 1, size, &mut tr, &mut Vec::new()).unwrap()
        });
        let t0 = Instant::now();
        let mut conn = secure::connect(std::net::TcpStream::connect(addr).unwrap(), &a_priv, None).unwrap().conn;
        let mut tr = Tracker::begin("send", "t", 1, size);
        push_files(&mut conn, &files, &mut tr).unwrap();
        conn.send(&Message::FilesEnd).unwrap();
        rx.join().unwrap();
        let secs = t0.elapsed().as_secs_f64();
        println!("512 MiB in {secs:.2}s = {:.0} MB/s ({:.1} Gbit/s)", size as f64 / secs / 1e6, size as f64 * 8.0 / secs / 1e9);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn unique_names_do_not_overwrite() {
        let dir = std::env::temp_dir().join(format!("skvm-uniq-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.txt"), b"x").unwrap();
        fs::write(dir.join("a (2).txt"), b"x").unwrap();
        assert_eq!(unique(&dir, "a.txt"), "a (3).txt");
        assert_eq!(unique(&dir, "b.txt"), "b.txt");
        let _ = fs::remove_dir_all(&dir);
    }
}
