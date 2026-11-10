//! Control API of the receiver service (Linux). Same security model as the Windows panel:
//! loopback only, secret token, Host check. The terminal UI (`SmoothKvm`) talks to it.
use crate::httpd::*;
use crate::store::{self, Identity, Peers};
use crate::{shared, VERSION};
use kvm_protocol::secure::to_hex;
use kvm_protocol::DisplayAction;
use serde_json::json;
use std::io;
use std::sync::Arc;
use tiny_http::{Method, Request, Server};

pub fn bind() -> io::Result<Server> {
    Server::http(("127.0.0.1", port())).map_err(|e| io::Error::new(io::ErrorKind::AddrInUse, e.to_string()))
}

pub fn serve(server: Server, token: String, ident: Arc<Identity>) {
    let token = Arc::new(token);
    for req in server.incoming_requests() {
        let (token, ident) = (token.clone(), ident.clone());
        std::thread::spawn(move || handle(req, &token, &ident));
    }
}

#[cfg(unix)]
fn display_do(a: DisplayAction) -> (bool, String) {
    crate::display::command(a)
}
#[cfg(not(unix))]
fn display_do(_a: DisplayAction) -> (bool, String) {
    (false, "virtual displays are created on the Linux machine".into())
}

fn handle(mut req: Request, token: &str, ident: &Arc<Identity>) {
    if !host_ok(&req) {
        return json_reply(req, 403, json!({"error": "forbidden"}));
    }
    let path = req.url().split('?').next().unwrap_or("").to_string();
    let method = req.method().clone();
    if path == "/ping" {
        return json_reply(req, 200, json!({"ok": true}));
    }
    if !authed(&req, token) {
        return json_reply(req, 401, json!({"error": "unauthorized"}));
    }
    let data = if method == Method::Post { read_json(&mut req) } else { serde_json::Value::Null };
    match (path.as_str(), method) {
        ("/api/state", Method::Get) => {
            let peers: Vec<_> = Peers::load()
                .peers
                .iter()
                .map(|p| json!({"name": p.name, "key": p.key, "short": store::short(&p.key)}))
                .collect();
            let (running, detail) = display_do(DisplayAction::Status);
            json_reply(
                req,
                200,
                json!({
                    "mode": "receiver",
                    "version": VERSION,
                    "me": store::short(&to_hex(&ident.public)),
                    "peers": peers,
                    "sessions": shared::sessions(),
                    "pairing_open_secs": shared::pairing_remaining(),
                    "pairing": shared::pending_pair(),
                    "display": {"running": running, "detail": detail},
                }),
            );
        }
        ("/api/pair/open", Method::Post) => {
            let secs = data.get("secs").and_then(|v| v.as_u64()).unwrap_or(120).clamp(10, 600);
            shared::open_pairing(secs);
            json_reply(req, 200, json!({"secs": secs}));
        }
        ("/api/pair/answer", Method::Post) => {
            shared::answer_pair(data.get("accept").and_then(|v| v.as_bool()).unwrap_or(false));
            json_reply(req, 200, json!({"ok": true}));
        }
        ("/api/peers/remove", Method::Post) => {
            let key = str_field(&data, "key");
            let mut peers = Peers::load();
            peers.peers.retain(|p| p.key != key);
            match peers.save() {
                Ok(()) => json_reply(req, 200, json!({"ok": true})),
                Err(e) => json_reply(req, 500, json!({"error": e.to_string()})),
            }
        }
        ("/api/display", Method::Post) => {
            let w = data.get("width").and_then(|v| v.as_u64()).unwrap_or(1920) as u32;
            let h = data.get("height").and_then(|v| v.as_u64()).unwrap_or(1080) as u32;
            let action = match str_field(&data, "action").as_str() {
                "start" => DisplayAction::Start { width: w, height: h },
                "stop" => DisplayAction::Stop,
                _ => DisplayAction::Status,
            };
            let (running, detail) = display_do(action);
            json_reply(req, 200, json!({"running": running, "detail": detail}));
        }
        ("/api/release", Method::Post) => {
            crate::release_everything();
            json_reply(req, 200, json!({"ok": true}));
        }
        _ => json_reply(req, 404, json!({"error": "not found"})),
    }
}
