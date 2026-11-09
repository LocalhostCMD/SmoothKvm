//! Local config panel. Serves a single page on 127.0.0.1 only.
//!
//! Hardening: loopback bind, Host-header check (anti DNS-rebinding), a random secret
//! token required in a custom header on every API call (anti CSRF: browsers can't send
//! custom headers cross-origin without a preflight, which we never approve), strict
//! CSP, no CORS headers, request bodies capped at 64 KiB.
use crate::store::{self, Peers};
use kvm_protocol::secure::to_hex;
use kvm_protocol::Config;
use serde_json::json;
use std::io::{self, Read};
use tiny_http::{Header, Method, Request, Response, Server};

const PORT: u16 = 24801;
const HTML: &str = include_str!("panel.html");

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).unwrap()
}

fn reply(req: Request, code: u16, ctype: &str, body: String) {
    let resp = Response::from_string(body)
        .with_status_code(code)
        .with_header(header("Content-Type", ctype))
        .with_header(header("Cache-Control", "no-store"))
        .with_header(header("X-Frame-Options", "DENY"))
        .with_header(header("X-Content-Type-Options", "nosniff"))
        .with_header(header("Referrer-Policy", "no-referrer"))
        .with_header(header(
            "Content-Security-Policy",
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'",
        ));
    let _ = req.respond(resp);
}

fn json_reply(req: Request, code: u16, v: serde_json::Value) {
    reply(req, code, "application/json", v.to_string());
}

pub fn run(open: bool) -> io::Result<()> {
    let token = store::panel_token()?;
    let server =
        Server::http(("127.0.0.1", PORT)).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    let url = format!("http://127.0.0.1:{PORT}/#{token}");
    println!("config panel: {url}\n(keep this link private; press Ctrl+C to close the panel)");
    if open {
        open_browser(&url);
    }
    for req in server.incoming_requests() {
        handle(req, &token);
    }
    Ok(())
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    let _ = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(not(windows))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

fn handle(mut req: Request, token: &str) {
    let host_ok = req.headers().iter().any(|h| {
        h.field.equiv("Host") && {
            let v = h.value.as_str();
            v == format!("127.0.0.1:{PORT}") || v == format!("localhost:{PORT}")
        }
    });
    if !host_ok {
        return json_reply(req, 403, json!({"error": "forbidden"}));
    }
    let path = req.url().split('?').next().unwrap_or("").to_string();
    let method = req.method().clone();
    if path == "/" && method == Method::Get {
        return reply(req, 200, "text/html; charset=utf-8", HTML.to_string());
    }
    let authed = req.headers().iter().any(|h| h.field.equiv("X-Token") && h.value.as_str() == token);
    if !authed {
        return json_reply(req, 401, json!({"error": "unauthorized"}));
    }
    let mut body = String::new();
    if method == Method::Post {
        let _ = req.as_reader().take(64 * 1024).read_to_string(&mut body);
    }
    match (path.as_str(), method) {
        ("/api/state", Method::Get) => {
            let me = store::Identity::load_or_create().map(|i| store::short(&to_hex(&i.public))).unwrap_or_default();
            let peers: Vec<_> = Peers::load()
                .peers
                .iter()
                .map(|p| json!({"name": p.name, "key": p.key, "short": store::short(&p.key)}))
                .collect();
            json_reply(req, 200, json!({"config": store::load_config(), "peers": peers, "me": me}));
        }
        ("/api/config", Method::Post) => match serde_json::from_str::<Config>(&body) {
            Ok(cfg) => match cfg.validate() {
                Ok(()) => match store::save_config(&cfg) {
                    Ok(()) => json_reply(req, 200, json!({"ok": true})),
                    Err(e) => json_reply(req, 500, json!({"error": e.to_string()})),
                },
                Err(e) => json_reply(req, 400, json!({"error": e})),
            },
            Err(e) => json_reply(req, 400, json!({"error": e.to_string()})),
        },
        ("/api/peers/remove", Method::Post) => {
            let key = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("key").and_then(|k| k.as_str().map(String::from)))
                .unwrap_or_default();
            let mut peers = Peers::load();
            peers.peers.retain(|p| p.key != key);
            match peers.save() {
                Ok(()) => json_reply(req, 200, json!({"ok": true})),
                Err(e) => json_reply(req, 500, json!({"error": e.to_string()})),
            }
        }
        _ => json_reply(req, 404, json!({"error": "not found"})),
    }
}
