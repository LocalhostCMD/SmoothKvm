//! Local config panel. Serves a single page on 127.0.0.1 only.
//!
//! Hardening: loopback bind, Host-header check (anti DNS-rebinding), a random secret
//! token required in a custom header on every API call (anti CSRF: browsers can't send
//! custom headers cross-origin without a preflight, which we never approve), strict
//! CSP, no CORS headers, request bodies capped at 64 KiB.
use crate::store::{self, Peers};
use crate::httpd::{json_reply, reply};
use crate::{capture_win, daemon, gui, shared};
use kvm_protocol::secure::to_hex;
use kvm_protocol::{Config, DisplayAction};
use serde_json::{json, Value};
use std::io::{self, Read};
use std::sync::Arc;
use tiny_http::{Method, Request, Server};


const HTML: &str = include_str!("panel.html");

pub fn bind() -> io::Result<Server> {
    Server::http(("127.0.0.1", crate::httpd::port())).map_err(|e| io::Error::new(io::ErrorKind::AddrInUse, e.to_string()))
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
    if !crate::httpd::host_ok(&req) {
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
    if !crate::httpd::authed(&req, token) {
        return json_reply(req, 401, json!({"error": "unauthorized"}));
    }
    let mut body = String::new();
    if method == Method::Post {
        let _ = req.as_reader().take(64 * 1024).read_to_string(&mut body);
    }
    let data: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    match (path.as_str(), method) {
        ("/api/events", Method::Get) => {
            let after = req.url().split("after=").nth(1).and_then(|s| s.split('&').next()).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            json_reply(req, 200, json!({"events": shared::events(after), "alerts": shared::alert_count()}))
        }
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
                    "mode": "sender",
                    "version": crate::VERSION,
                    "config": store::load_config(),
                    "status": shared::statuses(),
                    "pairing_pin": shared::pairing_pin(),
                    "game_active": capture_win::game_active_now(),
                    "paused": capture_win::is_paused(),
                    "sessions": shared::sessions(),
                    "pairing_open_secs": shared::pairing_remaining(),
                    "peers": peers,
                    "me": me,
                }),
            );
        }
        ("/api/config", Method::Post) => match serde_json::from_value::<Config>(data) {
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
        // Pair with screen `name` using the PIN shown on that device.
        ("/api/pair", Method::Post) => {
            let pin = str_field(&data, "pin");
            if pin.len() != 8 || !pin.chars().all(|c| c.is_ascii_digit()) {
                return json_reply(req, 400, json!({"error": "the PIN is 8 digits"}));
            }
            shared::request_pair(&str_field(&data, "name"), &pin);
            shared::info(&format!("Pairing with \"{}\" requested", str_field(&data, "name")));
            json_reply(req, 200, json!({"ok": true}));
        }
        // Let a new computer pair with THIS one (so it can control this PC): returns the PIN to type there.
        ("/api/pair/open", Method::Post) => {
            let secs = data.get("secs").and_then(|v| v.as_u64()).unwrap_or(120).clamp(10, 600);
            let pin = shared::open_pairing(secs);
            json_reply(req, 200, json!({"secs": secs, "pin": pin}));
        }
        ("/api/display", Method::Post) => {
            let name = str_field(&data, "name");
            shared::info(&format!("Remote display on \"{name}\": {}", str_field(&data, "action")));
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
        // Play the effect on screen exactly as configured (used by "Preview on screen" and /effect test).
        ("/api/effect/test", Method::Post) => {
            let anim = data
                .get("animation")
                .and_then(|a| serde_json::from_value::<kvm_protocol::Animation>(a.clone()).ok())
                .unwrap_or_else(|| store::load_config().animation);
            let mut cfg = store::load_config();
            cfg.animation = anim;
            match cfg.validate() {
                Ok(()) => {
                    crate::fx_win::preview(anim);
                    json_reply(req, 200, json!({"ok": true}))
                }
                Err(e) => json_reply(req, 400, json!({"error": e})),
            }
        }
        ("/api/autostart", Method::Get) => json_reply(req, 200, json!({"on": crate::app::autostart_enabled()})),
        ("/api/autostart", Method::Post) => {
            let on = data.get("on").and_then(|v| v.as_bool()).unwrap_or(false);
            match crate::app::set_autostart(on) {
                Ok(()) => {
                    shared::info(if on { "Start with Windows: on" } else { "Start with Windows: off" });
                    json_reply(req, 200, json!({"on": on}))
                }
                Err(e) => json_reply(req, 500, json!({"error": e.to_string()})),
            }
        }
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
