//! Local config panel. Serves a single page on 127.0.0.1 only.
//!
//! Hardening: loopback bind, Host-header check (anti DNS-rebinding), a random secret
//! token required in a custom header on every API call (anti CSRF: browsers can't send
//! custom headers cross-origin without a preflight, which we never approve), strict
//! CSP, no CORS headers, request bodies capped at 64 KiB.
use crate::store::{self, Peers};
use crate::{capture_win, daemon, gui, shared};
use kvm_protocol::secure::to_hex;
use kvm_protocol::{Config, DisplayAction};
use serde_json::{json, Value};
use std::io::{self, Read};
use std::sync::Arc;
use tiny_http::{Header, Method, Request, Response, Server};

pub const PORT: u16 = 24801;
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

fn json_reply(req: Request, code: u16, v: Value) {
    reply(req, code, "application/json", v.to_string());
}

pub fn bind() -> io::Result<Server> {
    Server::http(("127.0.0.1", PORT)).map_err(|e| io::Error::new(io::ErrorKind::AddrInUse, e.to_string()))
}

pub fn serve(server: Server, token: String, ident: Arc<store::Identity>) {
    let token = Arc::new(token);
    for req in server.incoming_requests() {
        let (token, ident) = (token.clone(), ident.clone());
        // One thread per request: starting a display can take several seconds.
        std::thread::spawn(move || handle(req, &token, &ident));
    }
}

fn str_field(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn handle(mut req: Request, token: &str, ident: &Arc<store::Identity>) {
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
    if path == "/ping" {
        return json_reply(req, 200, json!({"ok": true}));
    }
    let authed = req.headers().iter().any(|h| h.field.equiv("X-Token") && h.value.as_str() == token);
    if !authed {
        return json_reply(req, 401, json!({"error": "unauthorized"}));
    }
    let mut body = String::new();
    if method == Method::Post {
        let _ = req.as_reader().take(64 * 1024).read_to_string(&mut body);
    }
    let data: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    match (path.as_str(), method) {
        ("/api/state", Method::Get) => {
            let me = store::short(&to_hex(&ident.public));
            let peers: Vec<_> = Peers::load()
                .peers
                .iter()
                .map(|p| json!({"name": p.name, "key": p.key, "short": store::short(&p.key)}))
                .collect();
            json_reply(
                req,
                200,
                json!({
                    "config": store::load_config(),
                    "status": shared::statuses(),
                    "pairing": shared::pending_pair(),
                    "game_active": capture_win::game_active_now(),
                    "paused": capture_win::is_paused(),
                    "peers": peers,
                    "me": me,
                }),
            );
        }
        ("/api/config", Method::Post) => match serde_json::from_value::<Config>(data) {
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
            let key = str_field(&data, "key");
            let mut peers = Peers::load();
            peers.peers.retain(|p| p.key != key);
            match peers.save() {
                Ok(()) => json_reply(req, 200, json!({"ok": true})),
                Err(e) => json_reply(req, 500, json!({"error": e.to_string()})),
            }
        }
        ("/api/pair", Method::Post) => {
            shared::request_pair(&str_field(&data, "name"));
            json_reply(req, 200, json!({"ok": true}));
        }
        ("/api/pair/answer", Method::Post) => {
            shared::answer_pair(data.get("accept").and_then(|v| v.as_bool()).unwrap_or(false));
            json_reply(req, 200, json!({"ok": true}));
        }
        ("/api/display", Method::Post) => {
            let name = str_field(&data, "name");
            let (w, h) = (
                data.get("width").and_then(|v| v.as_u64()).unwrap_or(1920) as u32,
                data.get("height").and_then(|v| v.as_u64()).unwrap_or(1080) as u32,
            );
            let action = match str_field(&data, "action").as_str() {
                "start" => DisplayAction::Start { width: w, height: h },
                "stop" => DisplayAction::Stop,
                _ => DisplayAction::Status,
            };
            match daemon::control_display(&name, ident, action) {
                Ok((running, detail)) => json_reply(req, 200, json!({"running": running, "detail": detail})),
                Err(e) => json_reply(req, 502, json!({"error": e.to_string()})),
            }
        }
        ("/api/viewer", Method::Post) => match daemon::ensure_tunnel(&str_field(&data, "name"), ident.clone()) {
            Ok(port) => json_reply(req, 200, json!({"port": port, "launched": daemon::launch_viewer(port)})),
            Err(e) => json_reply(req, 502, json!({"error": e.to_string()})),
        },
        ("/api/show", Method::Post) => {
            gui::show_window();
            json_reply(req, 200, json!({"ok": true}));
        }
        ("/api/emergency", Method::Post) => {
            let on = data.get("on").and_then(|v| v.as_bool()).unwrap_or(true);
            if on {
                capture_win::Controller.emergency_stop();
            } else {
                capture_win::Controller.resume();
            }
            json_reply(req, 200, json!({"paused": capture_win::is_paused()}));
        }
        ("/api/quit", Method::Post) => {
            json_reply(req, 200, json!({"ok": true}));
            gui::quit_app();
            // Backstop in case the window event loop is wedged.
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_secs(2));
                std::process::exit(0);
            });
        }
        _ => json_reply(req, 404, json!({"error": "not found"})),
    }
}
