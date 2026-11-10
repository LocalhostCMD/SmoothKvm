//! Interactive terminal UI: type `SmoothKvm`, then `/commands`.
//! It is a thin client of the local control API, so it works the same on Windows and Linux.
use crate::ctl::{self, Client};
use crate::VERSION;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use serde_json::{json, Value};
use std::io;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

const ACCENT: Color = Color::Rgb(217, 119, 87);
const GOOD: Color = Color::Rgb(94, 190, 130);
const BAD: Color = Color::Rgb(230, 100, 90);
const WARN: Color = Color::Rgb(226, 179, 70);
const DIM: Color = Color::Rgb(140, 146, 160);

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Head,
    Info,
    Good,
    Bad,
    Dim,
    User,
}
type Out = Vec<(Kind, String)>;

enum Msg {
    State(Result<Value, String>),
    Out(Out),
}

const SENDER_CMDS: &[(&str, &str)] = &[
    ("/status", "show screens, connections and settings"),
    ("/screens", "list your screens"),
    ("/add", "/add <name> <left|right|top|bottom> <host:port> [WxH]"),
    ("/remove", "/remove <name>"),
    ("/pair", "/pair <name>   pair with a screen (it must be in pairing mode)"),
    ("/display", "/display <name> start|stop|status [WxH]"),
    ("/view", "/view <name>   open that screen's virtual display"),
    ("/effect", "/effect <bounce|elastic|slide|overshoot|spring|none> [ms] [px]"),
    ("/curve", "/curve <x1> <y1> <x2> <y2>   your own cubic-bezier effect"),
    ("/game", "/game on|off   never switch screens while gaming"),
    ("/receive", "/receive on|off   let paired computers control THIS PC too"),
    ("/allow", "/allow [seconds]   let a new device pair with this PC (default 120)"),
    ("/stop", "EMERGENCY STOP: release every key/button and pause switching"),
    ("/resume", "resume screen switching"),
    ("/devices", "list paired devices"),
    ("/unpair", "/unpair <name>"),
    ("/open", "open the app window"),
    ("/shutdown", "quit the background app"),
    ("/clear", "clear this screen"),
    ("/help", "show commands"),
    ("/quit", "leave the terminal UI"),
];

const RECEIVER_CMDS: &[(&str, &str)] = &[
    ("/status", "show connections and settings"),
    ("/pair", "/pair [seconds]   allow a new device to pair (default 120)"),
    ("/devices", "list paired devices"),
    ("/unpair", "/unpair <name>"),
    ("/display", "/display start|stop|status [WxH]"),
    ("/stop", "EMERGENCY STOP: release every key and mouse button"),
    ("/clear", "clear this screen"),
    ("/help", "show commands"),
    ("/quit", "leave the terminal UI"),
];

fn edge_name(s: &str) -> Option<&'static str> {
    match s.to_lowercase().as_str() {
        "left" => Some("Left"),
        "right" => Some("Right"),
        "top" | "up" => Some("Top"),
        "bottom" | "down" => Some("Bottom"),
        _ => None,
    }
}

fn style_name(s: &str) -> Option<&'static str> {
    match s.to_lowercase().as_str() {
        "none" | "off" => Some("None"),
        "bounce" => Some("Bounce"),
        "elastic" => Some("Elastic"),
        "slide" => Some("Slide"),
        "overshoot" => Some("Overshoot"),
        "spring" => Some("Spring"),
        "custom" => Some("Custom"),
        _ => None,
    }
}

fn remote_index(cfg: &Value, name: &str) -> Option<usize> {
    cfg["remotes"].as_array()?.iter().position(|r| r["name"].as_str().map(|n| n.eq_ignore_ascii_case(name)).unwrap_or(false))
}

fn state_of(c: &Client) -> Result<Value, String> {
    c.get("/api/state")
}

fn remote_lines(s: &Value) -> Out {
    let mut out = Vec::new();
    let remotes = s["config"]["remotes"].as_array().cloned().unwrap_or_default();
    if remotes.is_empty() {
        out.push((Kind::Dim, "No screens yet. Add one:  /add Server right 100.64.0.5:24800".into()));
    }
    for r in remotes {
        let name = r["name"].as_str().unwrap_or("?");
        let st = &s["status"][name];
        let state = st["state"].as_str().unwrap_or("connecting");
        let (kind, label) = match state {
            "connected" => (Kind::Good, "connected"),
            "not_paired" => (Kind::Info, "not paired yet  (use /pair)"),
            "disabled" => (Kind::Dim, "disabled"),
            "offline" => (Kind::Bad, "offline"),
            _ => (Kind::Info, "connecting"),
        };
        let detail = st["detail"].as_str().filter(|d| !d.is_empty() && state != "connected" && state != "not_paired").map(|d| format!(" - {d}")).unwrap_or_default();
        out.push((
            kind,
            format!(
                "{} {:<14} {:<7} {}  {}x{}  {label}{detail}",
                if state == "connected" { "●" } else { "○" },
                name,
                r["edge"].as_str().unwrap_or("?").to_lowercase(),
                r["addr"].as_str().filter(|a| !a.is_empty()).unwrap_or("(no address)"),
                r["width"],
                r["height"],
            ),
        ));
    }
    out
}

fn peer_lines(s: &Value) -> Out {
    let peers = s["peers"].as_array().cloned().unwrap_or_default();
    let mut out = vec![(Kind::Head, format!("Paired devices (this device: {})", s["me"].as_str().unwrap_or("?")))];
    if peers.is_empty() {
        out.push((Kind::Dim, "none yet".into()));
    }
    for p in peers {
        out.push((Kind::Info, format!("  {}  {}", p["name"].as_str().unwrap_or("?"), p["short"].as_str().unwrap_or(""))));
    }
    out
}

fn set_config(c: &Client, cfg: &Value) -> Result<(), String> {
    c.post("/api/config", cfg).map(|_| ())
}

/// Run one slash command against the control API.
fn exec(c: &Client, mode: &str, line: &str) -> Result<Out, String> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    let cmd = parts[0].to_lowercase();
    let a = &parts[1..];
    let sender = mode != "receiver";
    match (cmd.as_str(), sender) {
        ("/status", true) => {
            let s = state_of(c)?;
            let mut out = vec![(Kind::Head, format!("SmoothKvm {VERSION} - this PC controls your screens"))];
            out.extend(remote_lines(&s));
            let an = &s["config"]["animation"];
            out.push((Kind::Dim, format!("effect: {} {}ms {}px   game mode: {}{}", an["style"].as_str().unwrap_or("?"), an["duration_ms"], an["distance_px"], if s["config"]["game_guard"].as_bool().unwrap_or(true) { "on" } else { "off" }, if s["game_active"].as_bool().unwrap_or(false) { " (a full-screen app is running now)" } else { "" })));
            if s["paused"].as_bool().unwrap_or(false) {
                out.push((Kind::Bad, "STOPPED - screen switching is paused. Use /resume.".into()));
            }
            Ok(out)
        }
        ("/status", false) => {
            let s = state_of(c)?;
            let mut out = vec![(Kind::Head, format!("SmoothKvm {VERSION} - this machine is a screen"))];
            let sessions = s["sessions"].as_array().cloned().unwrap_or_default();
            if sessions.is_empty() {
                out.push((Kind::Dim, "no one is connected".into()));
            }
            for x in sessions {
                out.push((Kind::Good, format!("● {} connected for {}s", x["peer"].as_str().unwrap_or("?"), x["secs"])));
            }
            let open = s["pairing_open_secs"].as_u64().unwrap_or(0);
            out.push((if open > 0 { Kind::Info } else { Kind::Dim }, if open > 0 { format!("pairing is OPEN for {open}s") } else { "pairing is closed (use /pair to allow a new device)".into() }));
            out.push((Kind::Dim, format!("display: {}", s["display"]["detail"].as_str().unwrap_or("?"))));
            Ok(out)
        }
        ("/screens", true) => Ok(remote_lines(&state_of(c)?)),
        ("/add", true) => {
            if a.len() < 3 {
                return Err("usage: /add <name> <left|right|top|bottom> <host:port> [WxH]".into());
            }
            let edge = edge_name(a[1]).ok_or("side must be left, right, top or bottom")?;
            let (w, h) = a
                .get(3)
                .and_then(|s| s.split_once('x'))
                .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
                .unwrap_or((1920, 1080));
            let mut cfg = state_of(c)?["config"].clone();
            if remote_index(&cfg, a[0]).is_some() {
                return Err(format!("a screen called {} already exists", a[0]));
            }
            let addr = if a[2].contains(':') { a[2].to_string() } else { format!("{}:24800", a[2]) };
            cfg["remotes"].as_array_mut().ok_or("bad config")?.push(json!({"name": a[0], "addr": addr, "edge": edge, "width": w, "height": h, "enabled": true}));
            set_config(c, &cfg)?;
            Ok(vec![(Kind::Good, format!("Added {} on the {}.", a[0], edge.to_lowercase())), (Kind::Dim, format!("Next: start pairing mode on that device, then  /pair {}", a[0]))])
        }
        ("/remove", true) => {
            let name = a.first().ok_or("usage: /remove <name>")?;
            let mut cfg = state_of(c)?["config"].clone();
            let i = remote_index(&cfg, name).ok_or("no such screen")?;
            cfg["remotes"].as_array_mut().ok_or("bad config")?.remove(i);
            set_config(c, &cfg)?;
            Ok(vec![(Kind::Good, format!("Removed {name}."))])
        }
        ("/pair", true) => {
            let name = a.first().ok_or("usage: /pair <name>")?;
            let cfg = state_of(c)?["config"].clone();
            let i = remote_index(&cfg, name).ok_or("no such screen")?;
            let real = cfg["remotes"][i]["name"].as_str().unwrap_or(name).to_string();
            c.post("/api/pair", &json!({"name": real}))?;
            Ok(vec![(Kind::Info, format!("Pairing with {real}... a code will appear here. (The other device must be in pairing mode: /pair there.)"))])
        }
        ("/pair", false) => {
            let secs = a.first().and_then(|s| s.parse::<u64>().ok()).unwrap_or(120);
            let r = c.post("/api/pair/open", &json!({"secs": secs}))?;
            Ok(vec![(Kind::Info, format!("Pairing is open for {}s. Now start pairing from the controlling PC.", r["secs"]))])
        }
        ("/display", true) => {
            if a.len() < 2 {
                return Err("usage: /display <name> start|stop|status [WxH]".into());
            }
            let cfg = state_of(c)?["config"].clone();
            let i = remote_index(&cfg, a[0]).ok_or("no such screen")?;
            let r = &cfg["remotes"][i];
            let (w, h) = a
                .get(2)
                .and_then(|s| s.split_once('x'))
                .and_then(|(w, h)| Some((w.parse::<u64>().ok()?, h.parse::<u64>().ok()?)))
                .unwrap_or((r["width"].as_u64().unwrap_or(1920), r["height"].as_u64().unwrap_or(1080)));
            let res = c.post("/api/display", &json!({"name": r["name"], "action": a[1].to_lowercase(), "width": w, "height": h}))?;
            Ok(vec![(if res["running"].as_bool().unwrap_or(false) { Kind::Good } else { Kind::Info }, res["detail"].as_str().unwrap_or("done").to_string())])
        }
        ("/display", false) => {
            let action = a.first().ok_or("usage: /display start|stop|status [WxH]")?.to_lowercase();
            let (w, h) = a
                .get(1)
                .and_then(|s| s.split_once('x'))
                .and_then(|(w, h)| Some((w.parse::<u64>().ok()?, h.parse::<u64>().ok()?)))
                .unwrap_or((1920, 1080));
            let res = c.post("/api/display", &json!({"action": action, "width": w, "height": h}))?;
            Ok(vec![(if res["running"].as_bool().unwrap_or(false) { Kind::Good } else { Kind::Info }, res["detail"].as_str().unwrap_or("done").to_string())])
        }
        ("/view", true) => {
            let name = a.first().ok_or("usage: /view <name>")?;
            let cfg = state_of(c)?["config"].clone();
            let i = remote_index(&cfg, name).ok_or("no such screen")?;
            let res = c.post("/api/viewer", &json!({"name": cfg["remotes"][i]["name"]}))?;
            Ok(vec![if res["launched"].as_bool().unwrap_or(false) {
                (Kind::Good, "Viewer opened.".to_string())
            } else {
                (Kind::Info, format!("No VNC viewer found. Connect one to localhost:{}", res["port"]))
            }])
        }
        ("/effect", true) => {
            let style = style_name(a.first().ok_or("usage: /effect <bounce|elastic|slide|overshoot|spring|none> [ms] [px]")?).ok_or("unknown effect (try bounce, elastic, slide, overshoot, spring, none; or /curve)")?;
            let mut cfg = state_of(c)?["config"].clone();
            cfg["animation"]["style"] = json!(style);
            if let Some(ms) = a.get(1).and_then(|s| s.parse::<u32>().ok()) {
                cfg["animation"]["duration_ms"] = json!(ms);
            }
            if let Some(px) = a.get(2).and_then(|s| s.parse::<u32>().ok()) {
                cfg["animation"]["distance_px"] = json!(px);
            }
            set_config(c, &cfg)?;
            Ok(vec![(Kind::Good, format!("Effect: {style} {}ms {}px", cfg["animation"]["duration_ms"], cfg["animation"]["distance_px"]))])
        }
        ("/curve", true) => {
            let v: Vec<f64> = a.iter().filter_map(|s| s.parse().ok()).collect();
            if v.len() != 4 {
                return Err("usage: /curve <x1> <y1> <x2> <y2>   e.g. /curve 0.34 1.56 0.64 1".into());
            }
            let mut cfg = state_of(c)?["config"].clone();
            cfg["animation"]["style"] = json!("Custom");
            cfg["animation"]["curve"] = json!(v);
            set_config(c, &cfg)?;
            Ok(vec![(Kind::Good, format!("Custom curve set: {} {} {} {}", v[0], v[1], v[2], v[3]))])
        }
        ("/game", true) => {
            let on = match a.first().map(|s| s.to_lowercase()).as_deref() {
                Some("on") => true,
                Some("off") => false,
                _ => return Err("usage: /game on|off".into()),
            };
            let mut cfg = state_of(c)?["config"].clone();
            cfg["game_guard"] = json!(on);
            set_config(c, &cfg)?;
            Ok(vec![(Kind::Good, format!("Game mode {}.", if on { "on - no screen switching during full-screen apps" } else { "off" }))])
        }
        ("/receive", true) => {
            let on = match a.first().map(|s| s.to_lowercase()).as_deref() {
                Some("on") => true,
                Some("off") => false,
                _ => return Err("usage: /receive on|off".into()),
            };
            let mut cfg = state_of(c)?["config"].clone();
            cfg["receive"] = json!(on);
            set_config(c, &cfg)?;
            Ok(vec![
                (Kind::Good, format!("This PC {} be controlled by paired computers.", if on { "can now" } else { "can no longer" })),
                (Kind::Dim, if on { "Next: /allow, then pair from the other computer.".into() } else { "Nothing is listening for control any more.".into() }),
            ])
        }
        ("/allow", true) => {
            let secs = a.first().and_then(|s| s.parse::<u64>().ok()).unwrap_or(120);
            let r = c.post("/api/pair/open", &json!({"secs": secs}))?;
            Ok(vec![(Kind::Info, format!("Pairing open for {}s. Start pairing from the other computer now (needs /receive on).", r["secs"]))])
        }
        ("/stop", true) => {
            c.post("/api/emergency", &json!({"on": true}))?;
            Ok(vec![(Kind::Bad, "EMERGENCY STOP: control returned, every held key/button released, switching paused. /resume to continue.".into())])
        }
        ("/stop", false) => {
            c.post("/api/release", &json!({}))?;
            Ok(vec![(Kind::Bad, "Released every key and mouse button on this machine.".into())])
        }
        ("/resume", true) => {
            c.post("/api/emergency", &json!({"on": false}))?;
            Ok(vec![(Kind::Good, "Screen switching resumed.".into())])
        }
        ("/devices", _) => Ok(peer_lines(&state_of(c)?)),
        ("/unpair", _) => {
            let name = a.first().ok_or("usage: /unpair <name>")?.to_lowercase();
            let s = state_of(c)?;
            let key = s["peers"]
                .as_array()
                .and_then(|p| p.iter().find(|p| p["name"].as_str().map(|n| n.to_lowercase() == name).unwrap_or(false) || p["short"].as_str() == Some(name.as_str())))
                .and_then(|p| p["key"].as_str().map(String::from))
                .ok_or("no such paired device (see /devices)")?;
            c.post("/api/peers/remove", &json!({"key": key}))?;
            Ok(vec![(Kind::Good, "Unpaired. It must be paired again to connect.".into())])
        }
        ("/open", true) => {
            c.post("/api/show", &json!({}))?;
            Ok(vec![(Kind::Good, "Opening the app window...".into())])
        }
        ("/shutdown", true) => {
            c.post("/api/quit", &json!({}))?;
            Ok(vec![(Kind::Info, "SmoothKvm stopped. Type SmoothKvm again to start it.".into())])
        }
        _ => Err(format!("unknown command: {}  (try /help)", parts[0])),
    }
}

struct App {
    input: String,
    history: Vec<String>,
    hist: Option<usize>,
    lines: Out,
    state: Option<Value>,
    offline: bool,
    tx: Sender<Msg>,
    client: Client,
    quit: bool,
    scroll: usize,
    busy: u32,
}

impl App {
    fn mode(&self) -> &str {
        self.state.as_ref().and_then(|s| s["mode"].as_str()).unwrap_or(if cfg!(windows) { "sender" } else { "receiver" })
    }

    fn cmds(&self) -> &'static [(&'static str, &'static str)] {
        if self.mode() == "receiver" {
            RECEIVER_CMDS
        } else {
            SENDER_CMDS
        }
    }

    fn pairing(&self) -> Option<(String, String)> {
        let p = &self.state.as_ref()?["pairing"];
        Some((p["name"].as_str()?.to_string(), p["sas"].as_str()?.to_string()))
    }

    fn say(&mut self, kind: Kind, s: impl Into<String>) {
        self.lines.push((kind, s.into()));
        self.scroll = 0;
    }

    fn help(&mut self) {
        self.say(Kind::Head, "Commands");
        for (c, d) in self.cmds().to_vec() {
            self.say(Kind::Info, format!("  {c:<10} {}", if d.starts_with('/') { d.to_string() } else { d.to_string() }));
        }
        self.say(Kind::Dim, "Tip: press Scroll Lock three times quickly for the emergency stop on the controlling PC.");
    }

    fn submit(&mut self) {
        let line = self.input.trim().to_string();
        self.input.clear();
        self.hist = None;
        if line.is_empty() {
            return;
        }
        if self.history.last() != Some(&line) {
            self.history.push(line.clone());
        }
        self.say(Kind::User, format!("> {line}"));
        match line.split_whitespace().next().unwrap_or("").to_lowercase().as_str() {
            "/quit" | "/exit" | "/q" => self.quit = true,
            "/help" | "/?" => self.help(),
            "/clear" => self.lines.clear(),
            c if c.starts_with('/') => {
                let (client, tx, mode) = (self.client.clone(), self.tx.clone(), self.mode().to_string());
                self.busy += 1;
                std::thread::spawn(move || {
                    let out = exec(&client, &mode, &line).unwrap_or_else(|e| vec![(Kind::Bad, e)]);
                    let _ = tx.send(Msg::Out(out));
                });
            }
            _ => self.say(Kind::Dim, "Commands start with /  -  try /help"),
        }
    }

    fn complete(&mut self) {
        if !self.input.starts_with('/') || self.input.contains(' ') {
            return;
        }
        let first = self.cmds().iter().find(|(c, _)| c.starts_with(&self.input.to_lowercase())).map(|(c, _)| c.to_string());
        if let Some(c) = first {
            self.input = format!("{c} ");
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('c') | KeyCode::Char('d') if ctrl => self.quit = true,
            KeyCode::Char(c) if !ctrl && self.input.is_empty() && self.pairing().is_some() && matches!(c, 'y' | 'Y' | 'n' | 'N') => {
                let accept = matches!(c, 'y' | 'Y');
                let (client, tx) = (self.client.clone(), self.tx.clone());
                std::thread::spawn(move || {
                    let _ = client.post("/api/pair/answer", &json!({"accept": accept}));
                    let _ = tx.send(Msg::Out(vec![(if accept { Kind::Good } else { Kind::Info }, if accept { "Confirmed - finishing pairing...".into() } else { "Pairing cancelled.".into() })]));
                });
            }
            KeyCode::Char(c) if !ctrl => self.input.push(c),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Esc => self.input.clear(),
            KeyCode::Tab => self.complete(),
            KeyCode::Up => {
                if !self.history.is_empty() {
                    let i = self.hist.map(|i| i.saturating_sub(1)).unwrap_or(self.history.len() - 1);
                    self.hist = Some(i);
                    self.input = self.history[i].clone();
                }
            }
            KeyCode::Down => {
                if let Some(i) = self.hist {
                    if i + 1 < self.history.len() {
                        self.hist = Some(i + 1);
                        self.input = self.history[i + 1].clone();
                    } else {
                        self.hist = None;
                        self.input.clear();
                    }
                }
            }
            KeyCode::PageUp => self.scroll += 5,
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(5),
            _ => {}
        }
    }

    fn apply(&mut self, m: Msg) {
        match m {
            Msg::State(Ok(s)) => {
                self.offline = false;
                self.state = Some(s);
            }
            Msg::State(Err(_)) => self.offline = true,
            Msg::Out(lines) => {
                self.busy = self.busy.saturating_sub(1);
                for (k, l) in lines {
                    self.say(k, l);
                }
            }
        }
    }
}

fn style_of(k: Kind) -> Style {
    match k {
        Kind::Head => Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        Kind::Info => Style::new(),
        Kind::Good => Style::new().fg(GOOD),
        Kind::Bad => Style::new().fg(BAD),
        Kind::Dim => Style::new().fg(DIM),
        Kind::User => Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
    }
}

fn wrap(s: &str, w: usize) -> Vec<String> {
    if w == 0 {
        return vec![];
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars.chunks(w).map(|c| c.iter().collect()).collect()
}

fn header_line(app: &App) -> Line<'static> {
    let mut spans = vec![
        Span::styled("◆ ", Style::new().fg(ACCENT)),
        Span::styled("SmoothKvm", Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(format!(" v{VERSION}   "), Style::new().fg(DIM)),
    ];
    let s = match (&app.state, app.offline) {
        (None, _) | (_, true) => {
            spans.push(Span::styled("○ not running - waiting for the app/service...", Style::new().fg(BAD)));
            return Line::from(spans);
        }
        (Some(s), false) => s,
    };
    if s["mode"] == "receiver" {
        let n = s["sessions"].as_array().map(|a| a.len()).unwrap_or(0);
        spans.push(Span::styled(format!("● screen mode · {n} connection(s)"), Style::new().fg(if n > 0 { GOOD } else { WARN })));
        let open = s["pairing_open_secs"].as_u64().unwrap_or(0);
        if open > 0 {
            spans.push(Span::styled(format!(" · pairing open {open}s"), Style::new().fg(WARN)));
        }
    } else {
        let remotes = s["config"]["remotes"].as_array().cloned().unwrap_or_default();
        let enabled = remotes.iter().filter(|r| r["enabled"].as_bool().unwrap_or(true)).count();
        let up = remotes.iter().filter(|r| s["status"][r["name"].as_str().unwrap_or("")]["state"] == "connected").count();
        spans.push(Span::styled(format!("● {up}/{enabled} screens connected"), Style::new().fg(if up > 0 { GOOD } else { WARN })));
        if s["paused"].as_bool().unwrap_or(false) {
            spans.push(Span::styled(" · STOPPED", Style::new().fg(BAD).add_modifier(Modifier::BOLD)));
        }
        if s["game_active"].as_bool().unwrap_or(false) {
            spans.push(Span::styled(" · game mode active", Style::new().fg(WARN)));
        }
    }
    Line::from(spans)
}

fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let sugg = suggestions(app);
    let pair = app.pairing();
    let [head, body, sugg_area, pair_area, input_area, foot]: [Rect; 6] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(sugg.len().min(8) as u16),
        Constraint::Length(if pair.is_some() { 3 } else { 0 }),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(area);

    f.render_widget(Paragraph::new(header_line(app)), head);

    // transcript (with a welcome box first)
    let w = body.width.saturating_sub(2) as usize;
    let mut rows: Vec<Line> = Vec::new();
    let box_w = 46.min(w.max(10));
    let pad = |s: &str| format!("│ {:<width$} │", s, width = box_w.saturating_sub(4));
    rows.push(Line::styled(format!("╭{}╮", "─".repeat(box_w.saturating_sub(2))), Style::new().fg(ACCENT)));
    rows.push(Line::styled(pad("✻ Welcome to SmoothKvm"), Style::new().fg(ACCENT)));
    rows.push(Line::styled(pad("One keyboard and mouse, every screen."), Style::new().fg(DIM)));
    rows.push(Line::styled(pad("/help for commands · /stop = emergency stop"), Style::new().fg(DIM)));
    rows.push(Line::styled(format!("╰{}╯", "─".repeat(box_w.saturating_sub(2))), Style::new().fg(ACCENT)));
    rows.push(Line::raw(""));
    for (k, text) in &app.lines {
        for (i, chunk) in wrap(text, w).into_iter().enumerate() {
            let t = if i > 0 && *k == Kind::User { format!("  {chunk}") } else { chunk };
            rows.push(Line::styled(t, style_of(*k)));
        }
    }
    if app.busy > 0 {
        rows.push(Line::styled("✻ working...", Style::new().fg(ACCENT)));
    }
    let h = body.height as usize;
    let total = rows.len();
    let end = total.saturating_sub(app.scroll.min(total.saturating_sub(1)));
    let start = end.saturating_sub(h);
    f.render_widget(Paragraph::new(rows[start..end].to_vec()), Rect { x: body.x + 1, width: body.width.saturating_sub(1), ..body });

    // command suggestions
    if !sugg.is_empty() {
        let lines: Vec<Line> = sugg
            .iter()
            .take(8)
            .enumerate()
            .map(|(i, (c, d))| {
                Line::from(vec![
                    Span::styled(format!(" {c:<10}"), if i == 0 { Style::new().fg(ACCENT).add_modifier(Modifier::BOLD) } else { Style::new().fg(ACCENT) }),
                    Span::styled(format!(" {d}"), Style::new().fg(DIM)),
                ])
            })
            .collect();
        f.render_widget(Paragraph::new(lines), sugg_area);
    }

    // inline pairing prompt
    if let Some((name, sas)) = pair {
        let text = Line::from(vec![
            Span::styled(format!(" Pair with {name}?  code "), Style::new()),
            Span::styled(sas, Style::new().fg(WARN).add_modifier(Modifier::BOLD)),
            Span::styled("  - does the other device show the same code?  ", Style::new()),
            Span::styled("[y] yes  [n] no", Style::new().fg(ACCENT)),
        ]);
        f.render_widget(Paragraph::new(text).block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(WARN))), pair_area);
    }

    // input box
    let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM));
    f.render_widget(Paragraph::new(Line::from(vec![Span::styled("> ", Style::new().fg(ACCENT)), Span::raw(app.input.clone())])).block(block), input_area);
    let cx = input_area.x + 3 + app.input.chars().count() as u16;
    f.set_cursor_position((cx.min(input_area.right().saturating_sub(2)), input_area.y + 1));

    f.render_widget(Paragraph::new(Line::styled("  /help · Tab complete · ↑↓ history · PgUp/PgDn scroll · Ctrl+C quit", Style::new().fg(DIM))), foot);
}

fn suggestions(app: &App) -> Vec<(&'static str, &'static str)> {
    let inp = app.input.to_lowercase();
    if !inp.starts_with('/') {
        return vec![];
    }
    match inp.split_once(' ') {
        // after the command name: show its usage
        Some((cmd, _)) => app.cmds().iter().filter(|(c, _)| *c == cmd).copied().collect(),
        None => app.cmds().iter().filter(|(c, _)| c.starts_with(&inp)).copied().collect(),
    }
}

fn app_loop(term: &mut DefaultTerminal, client: Client) -> io::Result<()> {
    let (tx, rx): (Sender<Msg>, Receiver<Msg>) = channel();
    {
        let (c, tx) = (client.clone(), tx.clone());
        std::thread::spawn(move || loop {
            if tx.send(Msg::State(c.get("/api/state"))).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1000));
        });
    }
    let mut app = App { input: String::new(), history: vec![], hist: None, lines: vec![], state: None, offline: true, tx, client, quit: false, scroll: 0, busy: 0 };
    while !app.quit {
        term.draw(|f| draw(f, &app))?;
        while let Ok(m) = rx.try_recv() {
            app.apply(m);
        }
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(k) = event::read()? {
                app.on_key(k);
            }
        }
    }
    Ok(())
}

pub fn run() -> io::Result<()> {
    if !ctl::alive() {
        #[cfg(windows)]
        {
            eprintln!("Starting SmoothKvm...");
            let _ = crate::app::start(false);
        }
        #[cfg(not(windows))]
        eprintln!("The SmoothKvm service isn't running yet. Start it with:  sudo systemctl start smoothkvm");
    }
    let client = Client::new()?;
    let mut term = ratatui::try_init()?;
    let res = app_loop(&mut term, client);
    ratatui::restore();
    res
}
