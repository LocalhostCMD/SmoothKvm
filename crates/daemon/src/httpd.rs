//! Small helpers shared by the Windows panel server and the receiver control server.
//! Hardening: loopback bind (done by the caller), Host-header check (anti DNS-rebinding),
//! secret token in a custom header (anti CSRF), no CORS headers, strict CSP, bodies capped at 64 KiB.
use serde_json::Value;
use std::io::Read;
use tiny_http::{Header, Request, Response};

pub const PORT: u16 = 24801;

pub fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).unwrap()
}

pub fn reply(req: Request, code: u16, ctype: &str, body: String) {
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

pub fn json_reply(req: Request, code: u16, v: Value) {
    reply(req, code, "application/json", v.to_string());
}

pub fn host_ok(req: &Request) -> bool {
    req.headers().iter().any(|h| {
        h.field.equiv("Host") && {
            let v = h.value.as_str();
            v == format!("127.0.0.1:{PORT}") || v == format!("localhost:{PORT}")
        }
    })
}

pub fn authed(req: &Request, token: &str) -> bool {
    req.headers().iter().any(|h| h.field.equiv("X-Token") && h.value.as_str() == token)
}

pub fn read_json(req: &mut Request) -> Value {
    let mut body = String::new();
    let _ = req.as_reader().take(64 * 1024).read_to_string(&mut body);
    serde_json::from_str(&body).unwrap_or(Value::Null)
}

pub fn str_field(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}
