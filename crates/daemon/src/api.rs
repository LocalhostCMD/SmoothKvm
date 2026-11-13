//! API routes shared by the Windows app panel and the Linux control server (both serve the same app page).
use crate::store::{self, Identity};
use crate::{discover, shared, update};
use kvm_protocol::secure::to_hex;
use kvm_protocol::Edge;
use serde_json::{json, Value};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tiny_http::{Method, Request};

pub fn my_id(ident: &Identity) -> String {
    store::short(&to_hex(&ident.public))
}

/// Fields every /api/state answer carries in addition to the platform-specific ones.
pub fn extra_state(v: &mut Value) {
    v["os"] = json!(if cfg!(windows) { "windows" } else { "linux" });
    v["host"] = json!(discover::hostname());
    v["addrs"] = json!(discover::local_addrs());
    v["port"] = json!(store::load_config().port);
    v["update"] = update::state();
}

fn edge_of(s: &str) -> Option<Edge> {
    serde_json::from_value(json!(s)).ok()
}

/// Handle a shared route. Returns None when the path is not one of ours.
pub fn route(path: &str, method: &Method, data: &Value, ident: &Arc<Identity>) -> Option<(u16, Value)> {
    let ok = |v: Value| Some((200, v));
    let bad = |code: u16, e: String| Some((code, json!({"error": e})));
    match (path, method) {
        // Open pairing on THIS computer (so another one can pair with it), making sure it is listening.
        ("/api/pair/open", Method::Post) => {
            let secs = data.get("secs").and_then(|v| v.as_u64()).unwrap_or(120).clamp(10, 600);
            let mut cfg = store::load_config();
            if cfg!(windows) && !cfg.receive {
                cfg.receive = true;
                if let Err(e) = store::save_config(&cfg) {
                    return bad(500, e.to_string());
                }
                shared::info("Let paired computers control this PC: on (needed for pairing)");
            }
            let pin = shared::open_pairing(secs);
            ok(json!({"secs": secs, "pin": pin, "host": discover::hostname(), "addrs": discover::local_addrs(), "port": cfg.port}))
        }
        ("/api/pair/close", Method::Post) => {
            shared::burn_pairing();
            ok(json!({"ok": true}))
        }
        // Which computers are open for pairing on the network right now.
        ("/api/discover", Method::Post) => {
            let found = discover::scan(store::load_config().port, &my_id(ident));
            ok(json!({"found": found}))
        }
        // Add a screen (or reuse the one with this address) and pair with it, in one step.
        ("/api/pair/add", Method::Post) => {
            let pin = data["pin"].as_str().unwrap_or("").trim().to_string();
            if pin.len() != 8 || !pin.chars().all(|c| c.is_ascii_digit()) {
                return bad(400, "the code is 8 digits".into());
            }
            let mut addr = data["addr"].as_str().unwrap_or("").trim().to_string();
            if addr.is_empty() || addr.len() > 100 || addr.chars().any(|c| c.is_whitespace() || c == '/') {
                return bad(400, "enter the other computer's address, e.g. 192.168.1.20".into());
            }
            let has_port = addr.rsplit_once(':').map(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok()).unwrap_or(false);
            if !has_port {
                addr = format!("{addr}:24800");
            }
            let mut cfg = store::load_config();
            // Already paired and connected to that computer: nothing to do (re-pairing with a used code would
            // only knock the working connection offline).
            if let Some(r) = cfg.remotes.iter().find(|r| r.enabled && r.addr.eq_ignore_ascii_case(&addr)) {
                if shared::statuses().get(&r.name).map(|s| s.state == "connected").unwrap_or(false) {
                    return ok(json!({"name": r.name, "addr": addr, "already": true}));
                }
            }
            let want = data["edge"].as_str().and_then(edge_of).unwrap_or(Edge::Right);
            let name = if let Some(r) = cfg.remotes.iter_mut().find(|r| r.addr.eq_ignore_ascii_case(&addr)) {
                r.enabled = true;
                r.name.clone()
            } else {
                let base: String = shared::clean(data["name"].as_str().unwrap_or("")).trim().chars().take(32).collect();
                let base = if base.is_empty() { "Computer".to_string() } else { base };
                let mut name = base.clone();
                let mut n = 2;
                while cfg.remotes.iter().any(|r| r.name.eq_ignore_ascii_case(&name)) {
                    name = format!("{base} {n}");
                    n += 1;
                }
                // one screen per side: if that side is taken, use the first free one
                let taken = |e: &Edge| cfg.remotes.iter().any(|r| r.enabled && r.edge == *e);
                let edge = if !taken(&want) { want } else { [Edge::Right, Edge::Left, Edge::Top, Edge::Bottom].into_iter().find(|e| !taken(e)).unwrap_or(want) };
                cfg.remotes.push(kvm_protocol::Remote { name: name.clone(), addr: addr.clone(), edge, ..Default::default() });
                name
            };
            if let Err(e) = cfg.validate() {
                return bad(400, e);
            }
            if let Err(e) = store::save_config(&cfg) {
                return bad(500, e.to_string());
            }
            shared::request_pair(&name, &pin);
            shared::info(&format!("Pairing with \"{name}\" ({addr})"));
            ok(json!({"name": name, "addr": addr}))
        }
        ("/api/update/check", Method::Post) => ok(update::check_now()),
        ("/api/update/apply", Method::Post) => match update::apply_async() {
            Ok(()) => ok(update::state()),
            Err(e) => bad(409, e),
        },
        ("/api/uninstall", Method::Post) => {
            if data["confirm"].as_str() != Some("remove") {
                return bad(400, "not confirmed".into());
            }
            match update::uninstall(data["wipe"].as_bool().unwrap_or(false)) {
                Ok(m) => ok(json!({"ok": true, "detail": m})),
                Err(e) => bad(400, e),
            }
        }
        _ => None,
    }
}

// ---- files dropped on the app page in a normal browser (Linux): the page can upload their contents, but
// cannot tell us where they are on disk, so they are staged in a private folder and then sent from there.

fn outbox() -> PathBuf {
    #[cfg(windows)]
    let base = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("SmoothKvm");
    #[cfg(unix)]
    let base = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp")).join(".cache").join("smoothkvm");
    base.join("outbox")
}

fn query<'a>(url: &'a str, key: &str) -> Option<&'a str> {
    url.split_once('?')?.1.split('&').find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// POST /api/stage?batch=<id>&name=<file name> with the raw file as the body. Returns the staged path.
pub fn stage(req: &mut Request) -> (u16, Value) {
    let url = req.url().to_string();
    let batch: String = query(&url, "batch").unwrap_or("").chars().filter(|c| c.is_ascii_alphanumeric()).take(32).collect();
    let name = pct_decode(query(&url, "name").unwrap_or(""));
    let Some(parts) = crate::xfer::safe_rel(&name) else { return (400, json!({"error": "bad file name"})) };
    let Some(file) = parts.last().cloned() else { return (400, json!({"error": "bad file name"})) };
    if batch.is_empty() {
        return (400, json!({"error": "missing batch"}));
    }
    let root = outbox();
    // old batches (sent or abandoned) are cleaned up as new ones arrive
    if let Ok(rd) = std::fs::read_dir(&root) {
        for e in rd.flatten() {
            let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map(|d| d.as_secs() > 6 * 3600).unwrap_or(false);
            if old {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    let dir = root.join(&batch);
    let res = (|| -> io::Result<PathBuf> {
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(&file);
        let mut out = std::fs::File::create(&path)?;
        let mut body = req.as_reader().take(64 * 1024 * 1024 * 1024);
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = body.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
        }
        Ok(path)
    })();
    match res {
        Ok(p) => (200, json!({"path": p})),
        Err(e) => (500, json!({"error": e.to_string()})),
    }
}
