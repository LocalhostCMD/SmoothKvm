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

#[cfg(target_os = "linux")]
use crate::capture_linux as cap;
/// Placeholder so this file also builds on platforms without the Linux controller.
#[cfg(not(target_os = "linux"))]
mod cap {
    pub struct Controller;
    impl Controller {
        pub fn emergency_stop(&self) {}
        pub fn resume(&self) {}
    }
    pub fn is_paused() -> bool {
        false
    }
}

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
    // The app page (the same one the Windows app shows); it needs the token to do anything.
    if path == "/" && method == Method::Get {
        return reply(req, 200, "text/html; charset=utf-8", include_str!("panel.html").to_string());
    }
    if !authed(&req, token) {
        return json_reply(req, 401, json!({"error": "unauthorized"}));
    }
    if path == "/api/stage" && method == Method::Post {
        let (code, v) = crate::api::stage(&mut req);
        return json_reply(req, code, v);
    }
    let data = if method == Method::Post { read_json(&mut req) } else { serde_json::Value::Null };
    if let Some((code, v)) = crate::api::route(&path, &method, &data, ident) {
        return json_reply(req, code, v);
    }
    // The app page polls with ?page=1 while it is open: when the window is closed (and SmoothKvm runs inside
    // the app rather than as a service), the app stops.
    #[cfg(unix)]
    if req.url().contains("page=1") {
        crate::linux_app::touch();
    }
    #[cfg(unix)]
    if path == "/api/bye" {
        crate::linux_app::bye();
        return json_reply(req, 200, json!({"ok": true}));
    }
    match (path.as_str(), method) {
        ("/api/events", Method::Get) => {
            let after = req.url().split("after=").nth(1).and_then(|s| s.split('&').next()).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            json_reply(req, 200, json!({"events": shared::events(after), "alerts": shared::alert_count()}))
        }
        ("/api/state", Method::Get) => {
            let peers: Vec<_> = Peers::load()
                .peers
                .iter()
                .map(|p| json!({"name": p.name, "key": p.key, "short": store::short(&p.key)}))
                .collect();
            let (running, detail) = display_do(DisplayAction::Status);
            let mut v = json!({
                    "mode": "receiver",
                    "version": VERSION,
                    "me": store::short(&to_hex(&ident.public)),
                    "peers": peers,
                    "sessions": shared::sessions(),
                    "pairing_open_secs": shared::pairing_remaining(),
                    "pairing_pin": shared::pairing_pin(),
                    "config": store::load_config(),
                    "status": shared::statuses(),
                    "paused": cap::is_paused(),
                    "transfer": crate::xfer::progress(),
                    "targets": crate::xfer::targets(),
                    "inbox": crate::xfer::inbox(),
                    "display": {"running": running, "detail": detail},
            });
            crate::api::extra_state(&mut v);
            json_reply(req, 200, v);
        }
        ("/api/peers/remove", Method::Post) => {
            let key = str_field(&data, "key");
            let mut peers = Peers::load();
            peers.peers.retain(|p| p.key != key);
            shared::warn(&format!("Device removed from the trusted list (id {})", store::short(&key)));
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
        ("/api/config", Method::Post) => match serde_json::from_value::<kvm_protocol::Config>(data) {
            Ok(cfg) => match cfg.validate() {
                Ok(()) => match store::save_config(&cfg) {
                    Ok(()) => {
                        shared::info("Settings changed");
                        json_reply(req, 200, json!({"ok": true}))
                    }
                    Err(e) => json_reply(req, 500, json!({"error": e.to_string()})),
                },
                Err(e) => json_reply(req, 400, json!({"error": e})),
            },
            Err(e) => json_reply(req, 400, json!({"error": e.to_string()})),
        },
        ("/api/pair", Method::Post) => {
            let pin = str_field(&data, "pin");
            if pin.len() != 8 || !pin.chars().all(|c| c.is_ascii_digit()) {
                return json_reply(req, 400, json!({"error": "the PIN is 8 digits"}));
            }
            shared::request_pair(&str_field(&data, "name"), &pin);
            shared::info(&format!("Pairing with \"{}\" requested", str_field(&data, "name")));
            json_reply(req, 200, json!({"ok": true}))
        }
        ("/api/emergency", Method::Post) => {
            if data.get("on").and_then(|v| v.as_bool()).unwrap_or(true) {
                cap::Controller.emergency_stop();
                crate::release_everything();
            } else {
                cap::Controller.resume();
            }
            json_reply(req, 200, json!({"paused": cap::is_paused()}))
        }
        // "Preview on screen" for the screen-change effect
        ("/api/effect/test", Method::Post) => {
            let mut cfg = store::load_config();
            if let Some(a) = data.get("animation").and_then(|a| serde_json::from_value::<kvm_protocol::Animation>(a.clone()).ok()) {
                cfg.animation = a;
            }
            match cfg.validate() {
                Ok(()) => {
                    #[cfg(target_os = "linux")]
                    crate::fx_x11::preview(cfg.animation);
                    json_reply(req, 200, json!({"ok": true}))
                }
                Err(e) => json_reply(req, 400, json!({"error": e})),
            }
        }
        ("/api/release", Method::Post) => {
            crate::release_everything();
            json_reply(req, 200, json!({"ok": true}));
        }
        // Send local files/folders to another computer (runs in the background; progress is in /api/state).
        ("/api/send", Method::Post) => {
            let paths: Vec<std::path::PathBuf> = data
                .get("paths")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|p| p.as_str()).map(std::path::PathBuf::from).collect())
                .unwrap_or_default();
            if paths.is_empty() {
                return json_reply(req, 400, json!({"error": "no files given"}));
            }
            let to = data.get("to").and_then(|v| v.as_str()).map(String::from);
            match crate::xfer::send_async(to, paths, ident.clone()) {
                Ok(t) => json_reply(req, 200, json!({"to": t.name})),
                Err(e) => json_reply(req, 400, json!({"error": e.to_string()})),
            }
        }
        _ => json_reply(req, 404, json!({"error": "not found"})),
    }
}
