//! Client for the local control API (Windows panel server / Linux receiver service).
use crate::httpd::port;
use crate::store;
use serde_json::Value;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

#[derive(Clone)]
pub struct Client {
    token: String,
}

fn request(method: &str, path: &str, token: Option<&str>, body: &str, timeout: Duration) -> io::Result<(u16, String)> {
    let mut s = TcpStream::connect_timeout(&([127, 0, 0, 1], port()).into(), Duration::from_millis(800))?;
    s.set_read_timeout(Some(timeout))?;
    let tok = token.map(|t| format!("X-Token: {t}\r\n")).unwrap_or_default();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{tok}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        port(),
        body.len()
    )?;
    let mut out = String::new();
    s.read_to_string(&mut out)?;
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    let code = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    Ok((code, body.to_string()))
}

/// Is a SmoothKvm app/service answering on the control port?
pub fn alive() -> bool {
    matches!(request("GET", "/ping", None, "", Duration::from_secs(2)), Ok((200, _)))
}

impl Client {
    pub fn new() -> io::Result<Self> {
        Ok(Self { token: store::panel_token()? })
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        // Starting a virtual display can take ~15s, so be patient.
        let (code, text) = request(method, path, Some(&self.token), &body, Duration::from_secs(60)).map_err(|e| e.to_string())?;
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if (200..300).contains(&code) {
            Ok(v)
        } else {
            Err(v.get("error").and_then(|e| e.as_str()).unwrap_or("request failed").to_string())
        }
    }

    pub fn get(&self, path: &str) -> Result<Value, String> {
        self.call("GET", path, None)
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.call("POST", path, Some(body))
    }
}
