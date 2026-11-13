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
use std::time::{Duration, Instant};

const ACCENT: Color = Color::Rgb(0, 229, 255);
const MAGENTA: Color = Color::Rgb(255, 45, 149);
const GOOD: Color = Color::Rgb(157, 255, 74);
const BAD: Color = Color::Rgb(255, 82, 82);
const WARN: Color = Color::Rgb(255, 211, 78);
const DIM: Color = Color::Rgb(122, 130, 150);

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
    ("/clipboard", "/clipboard on|off   share copied text with paired computers"),
    ("/send", "/send <file or folder>... [to <screen>]   send files (\"quote paths\" with spaces)"),
    ("/files", "/files on|off   accept files from other computers;  /files folder <path>|default   where they are saved"),
    ("/log", "/log [alerts]   recent activity and security alerts"),
    ("/status", "show screens, connections and settings"),
    ("/screens", "list your screens"),
    ("/add", "/add <name> <left|right|top|bottom> <host:port> [WxH]"),
    ("/move", "/move <name> <left|right|top|bottom>   put a screen on another side"),
    ("/remove", "/remove <name>"),
    ("/pair", "/pair <name> <PIN>   pair with a screen; the 8-digit PIN is shown on that device (run pair there)"),
    ("/display", "/display <name> start|stop|status [WxH]"),
    ("/view", "/view <name>   open that screen's virtual display"),
    ("/effect", "/effect <ripple|glow|sparks|comet|none> [rainbow|ocean|sunset|neon|mono] [700ms] [220px] [test]"),
    ("/motion", "/motion <bounce|elastic|slide|overshoot|spring|custom>   how the effect moves"),
    ("/curve", "/curve <x1> <y1> <x2> <y2>   your own motion curve (cubic-bezier)"),
    ("/game", "/game on|off   never switch screens while gaming"),
    ("/receive", "/receive on|off   let paired computers control THIS PC too"),
    ("/allow", "/allow [seconds]   let a new device pair with this PC (default 120)"),
    ("/stop", "EMERGENCY STOP: release every key/button and pause switching"),
    ("/resume", "resume screen switching"),
    ("/devices", "list paired devices"),
    ("/unpair", "/unpair <name>"),
    ("/open", "open the app window"),
    ("/update", "check for a new version and install it"),
    ("/uninstall", "/uninstall yes [wipe]   remove SmoothKvm from this computer"),
    ("/shutdown", "quit the background app"),
    ("/clear", "clear this screen"),
    ("/help", "show commands"),
    ("/quit", "leave the terminal UI"),
];

const RECEIVER_CMDS: &[(&str, &str)] = &[
    ("/clipboard", "/clipboard on|off   share copied text with paired computers"),
    ("/send", "/send <file or folder>... [to <screen>]   send files (\"quote paths\" with spaces)"),
    ("/files", "/files on|off   accept files from other computers;  /files folder <path>|default   where they are saved"),
    ("/screens", "list the screens this computer can control (switch with the Scroll Lock key)"),
    ("/add", "/add <name> <left|right|top|bottom> <host:port>   a screen to control from this computer"),
    ("/move", "/move <name> <left|right|top|bottom>   put a screen on another side"),
    ("/remove", "/remove <name>"),
    ("/resume", "resume after an emergency stop"),
    ("/log", "/log [alerts]   recent activity and security alerts"),
    ("/status", "show connections and settings"),
    ("/pair", "/pair  shows a big pairing code here;  /pair <name> <code> pairs with a screen you added"),
    ("/devices", "list paired devices"),
    ("/unpair", "/unpair <name>"),
    ("/display", "/display start|stop|status [WxH]"),
    ("/app", "open the SmoothKvm window (the easy way to pair and change settings)"),
    ("/update", "check for a new version and install it"),
    ("/uninstall", "/uninstall yes [wipe]   remove SmoothKvm from this computer"),
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

/// Split a command line into words, keeping "quoted parts" (or 'single') together.
fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for ch in s.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(ch);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            (None, c) => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn expand_home(p: &str) -> String {
    match p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
        Some(rest) => {
            let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_default();
            format!("{home}/{rest}")
        }
        None => p.to_string(),
    }
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
            out.push((Kind::Dim, format!("effect: {} {}ms {}px   game mode: {}{}", an["visual"].as_str().unwrap_or("?"), an["duration_ms"], an["size_px"], if s["config"]["game_guard"].as_bool().unwrap_or(true) { "on" } else { "off" }, if s["game_active"].as_bool().unwrap_or(false) { " (a full-screen app is running now)" } else { "" })));
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
        ("/log", _) => {
            let only_alerts = a.first().map(|s| s.starts_with("alert")).unwrap_or(false);
            let r = c.get("/api/events?after=0")?;
            let mut out = vec![(Kind::Head, format!("Activity log ({} security alert(s) so far)", r["alerts"]))];
            let evs = r["events"].as_array().cloned().unwrap_or_default();
            let shown: Vec<_> = evs.iter().filter(|e| !only_alerts || e["level"] == "alert").collect();
            for e in shown.iter().rev().take(15).rev() {
                let kind = match e["level"].as_str() { Some("alert") => Kind::Bad, Some("warn") => Kind::Info, _ => Kind::Dim };
                let t = e["t"].as_u64().unwrap_or(0) % 86_400;
                out.push((kind, format!("{:02}:{:02}:{:02} UTC  {}", t / 3600, t % 3600 / 60, t % 60, e["text"].as_str().unwrap_or(""))));
            }
            if shown.is_empty() {
                out.push((Kind::Dim, "nothing logged yet".into()));
            }
            Ok(out)
        }
        ("/screens", _) => Ok(remote_lines(&state_of(c)?)),
        ("/add", _) => {
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
        ("/move", _) => {
            let (name, side) = match (a.first(), a.get(1)) {
                (Some(n), Some(s)) => (*n, *s),
                _ => return Err("usage: /move <name> <left|right|top|bottom>".into()),
            };
            let edge = edge_name(side).ok_or("side must be left, right, top or bottom")?;
            let mut cfg = state_of(c)?["config"].clone();
            let i = remote_index(&cfg, name).ok_or("no such screen (see /screens)")?;
            let remotes = cfg["remotes"].as_array_mut().ok_or("bad config")?;
            let from = remotes[i]["edge"].clone();
            // a screen already on that side swaps places with it
            for (j, r) in remotes.iter_mut().enumerate() {
                if j != i && r["edge"] == edge {
                    r["edge"] = from.clone();
                }
            }
            remotes[i]["edge"] = json!(edge);
            let real = remotes[i]["name"].as_str().unwrap_or(name).to_string();
            set_config(c, &cfg)?;
            Ok(vec![(Kind::Good, format!("{real} is now on the {}.", edge.to_lowercase()))])
        }
        ("/remove", _) => {
            let name = a.first().ok_or("usage: /remove <name>")?;
            let mut cfg = state_of(c)?["config"].clone();
            let i = remote_index(&cfg, name).ok_or("no such screen")?;
            cfg["remotes"].as_array_mut().ok_or("bad config")?.remove(i);
            set_config(c, &cfg)?;
            Ok(vec![(Kind::Good, format!("Removed {name}."))])
        }
        ("/pair", _) if sender || a.len() >= 2 => {
            let (name, pin) = match (a.first(), a.get(1)) {
                (Some(n), Some(p)) => (*n, *p),
                _ => return Err("usage: pair <name> <8-digit PIN shown on that device>   (on that device run: pair)".into()),
            };
            if pin.len() != 8 || !pin.chars().all(|ch| ch.is_ascii_digit()) {
                return Err("the PIN is 8 digits".into());
            }
            let cfg = state_of(c)?["config"].clone();
            let i = remote_index(&cfg, name).ok_or("no such screen")?;
            let real = cfg["remotes"][i]["name"].as_str().unwrap_or(name).to_string();
            c.post("/api/pair", &json!({"name": real, "pin": pin}))?;
            Ok(vec![(Kind::Info, format!("Pairing with {real}... watch the screens panel."))])
        }
        ("/pair", false) => {
            let secs = a.first().and_then(|s| s.parse::<u64>().ok()).unwrap_or(300);
            let r = c.post("/api/pair/open", &json!({"secs": secs}))?;
            Ok(vec![
                (Kind::Head, format!("PAIRING CODE: {}  (shown big on the left, valid {}s)", r["pin"].as_str().unwrap_or("?"), r["secs"])),
                (Kind::Dim, "On the other computer: + Add a computer -> Control another computer, pick this one, type the code. It works once.".into()),
            ])
        }
        ("/update", _) => {
            let s = c.post("/api/update/check", &json!({}))?;
            if let Some(e) = s["error"].as_str() {
                return Err(format!("Could not check for updates: {e}"));
            }
            if !s["available"].as_bool().unwrap_or(false) {
                return Ok(vec![(Kind::Good, format!("You have the newest version ({VERSION})."))]);
            }
            c.post("/api/update/apply", &json!({}))?;
            Ok(vec![
                (Kind::Head, format!("Updating to SmoothKvm {}...", s["latest"].as_str().unwrap_or("?"))),
                (Kind::Dim, "It is downloaded, checked against the SmoothKvm signing key, installed, and SmoothKvm restarts by itself. Progress is shown at the top.".into()),
            ])
        }
        ("/uninstall", _) => {
            if a.first().map(|s| s.to_lowercase()) != Some("yes".into()) {
                return Ok(vec![
                    (Kind::Head, "Uninstall SmoothKvm from this computer?".into()),
                    (Kind::Dim, "Type  uninstall yes  to remove it (settings are kept), or  uninstall yes wipe  to delete your settings and paired devices too.".into()),
                ]);
            }
            let wipe = a.get(1).map(|s| s.eq_ignore_ascii_case("wipe")).unwrap_or(false);
            let r = c.post("/api/uninstall", &json!({"confirm": "remove", "wipe": wipe}))?;
            Ok(vec![(Kind::Good, r["detail"].as_str().unwrap_or("Removed.").to_string())])
        }
        #[cfg(unix)]
        ("/app", _) => {
            crate::linux_app::open_window().map_err(|e| e.to_string())?;
            Ok(vec![(Kind::Good, "Opening the SmoothKvm window...".into())])
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
            const VISUALS: [&str; 5] = ["None", "Ripple", "Glow", "Sparks", "Comet"];
            const PALETTES: [&str; 5] = ["Rainbow", "Ocean", "Sunset", "Neon", "Mono"];
            let pick = |list: &[&'static str], w: &str| list.iter().find(|x| x.eq_ignore_ascii_case(w)).copied();
            let mut cfg = state_of(c)?["config"].clone();
            let mut test = false;
            for w in a {
                if let Some(v) = pick(&VISUALS, w) {
                    cfg["animation"]["visual"] = json!(v);
                } else if let Some(p) = pick(&PALETTES, w) {
                    cfg["animation"]["palette"] = json!(p);
                } else if let Some(ms) = w.strip_suffix("ms").and_then(|n| n.parse::<u32>().ok()) {
                    cfg["animation"]["duration_ms"] = json!(ms);
                } else if let Some(px) = w.strip_suffix("px").and_then(|n| n.parse::<u32>().ok()) {
                    cfg["animation"]["size_px"] = json!(px);
                } else if w.eq_ignore_ascii_case("test") {
                    test = true;
                } else {
                    return Err(format!("don't know \"{w}\". Try: effect ripple|glow|sparks|comet|none  rainbow|ocean|sunset|neon|mono  700ms  220px  test"));
                }
            }
            if a.is_empty() {
                return Err("usage: effect <ripple|glow|sparks|comet|none> [rainbow|ocean|sunset|neon|mono] [700ms] [220px] [test]".into());
            }
            set_config(c, &cfg)?;
            if test {
                c.post("/api/effect/test", &json!({"animation": cfg["animation"]}))?;
            }
            let an = &cfg["animation"];
            Ok(vec![(Kind::Good, format!("Effect: {} / {} / {}ms / {}px", an["visual"].as_str().unwrap_or("?"), an["palette"].as_str().unwrap_or("?"), an["duration_ms"], an["size_px"]))])
        }
        ("/motion", true) => {
            let style = style_name(a.first().ok_or("usage: motion <bounce|elastic|slide|overshoot|spring|custom>")?).ok_or("unknown motion (bounce, elastic, slide, overshoot, spring, custom)")?;
            let mut cfg = state_of(c)?["config"].clone();
            cfg["animation"]["style"] = json!(style);
            set_config(c, &cfg)?;
            Ok(vec![(Kind::Good, format!("Motion: {style}"))])
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
        ("/send", _) => {
            let args = split_args(line.splitn(2, char::is_whitespace).nth(1).unwrap_or(""));
            let (paths, to) = match args.iter().rposition(|a| a.eq_ignore_ascii_case("to")) {
                Some(i) if i + 1 < args.len() && i > 0 => (args[..i].to_vec(), Some(args[i + 1..].join(" "))),
                _ => (args.clone(), None),
            };
            if paths.is_empty() {
                return Err("usage: /send <file or folder>... [to <screen>]".into());
            }
            // the app runs in another folder: send absolute paths
            let mut abs = Vec::new();
            for p in &paths {
                let p = expand_home(p);
                abs.push(std::fs::canonicalize(&p).map_err(|e| format!("{p}: {e}"))?.to_string_lossy().trim_start_matches(r"\\?\").to_string());
            }
            let r = c.post("/api/send", &json!({"paths": abs, "to": to}))?;
            Ok(vec![(Kind::Info, format!("Sending to {}... (progress on the left)", r["to"].as_str().unwrap_or("?")))])
        }
        ("/files", _) if a.first().map(|s| s.eq_ignore_ascii_case("folder")).unwrap_or(false) => {
            let mut cfg = state_of(c)?["config"].clone();
            let rest: Vec<String> = split_args(&a[1..].join(" "));
            let dir = match rest.first().map(|s| s.as_str()) {
                None => return Err("usage: /files folder <full path>   or   /files folder default".into()),
                Some(d) if d.eq_ignore_ascii_case("default") => String::new(),
                Some(d) => {
                    let p = expand_home(d);
                    std::fs::create_dir_all(&p).map_err(|e| format!("cannot use {p}: {e}"))?;
                    std::fs::canonicalize(&p).map(|x| x.to_string_lossy().trim_start_matches(r"\\?\").to_string()).unwrap_or(p)
                }
            };
            cfg["inbox_dir"] = json!(dir);
            set_config(c, &cfg)?;
            let s = state_of(c)?;
            Ok(vec![(Kind::Good, format!("Received files now go to {}", s["inbox"].as_str().unwrap_or("?")))])
        }
        ("/files", _) => {
            let s = state_of(c)?;
            let on = match a.first().map(|s| s.to_lowercase()).as_deref() {
                Some("on") => true,
                Some("off") => false,
                None => {
                    let on = s["config"]["file_drop"].as_bool().unwrap_or(true);
                    return Ok(vec![(Kind::Info, format!("Receiving files is {}. Files go to {}", if on { "ON" } else { "off" }, s["inbox"].as_str().unwrap_or("?")))]);
                }
                _ => return Err("usage: /files on|off".into()),
            };
            let mut cfg = s["config"].clone();
            cfg["file_drop"] = json!(on);
            set_config(c, &cfg)?;
            Ok(vec![(Kind::Good, format!("Receiving files is {}.", if on { "ON" } else { "off" })), (Kind::Dim, format!("Received files go to {} and are never opened or run.", s["inbox"].as_str().unwrap_or("?")))])
        }
        ("/clipboard", _) => {
            let on = match a.first().map(|s| s.to_lowercase()).as_deref() {
                Some("on") => true,
                Some("off") => false,
                _ => return Err("usage: /clipboard on|off".into()),
            };
            let mut cfg = state_of(c)?["config"].clone();
            cfg["clipboard"] = json!(on);
            set_config(c, &cfg)?;
            Ok(vec![
                (Kind::Good, format!("Clipboard sharing is {}.", if on { "ON" } else { "off" })),
                (Kind::Dim, "Only plain text copied after connecting is shared, up to 32 KB, with paired computers that also have it on. Text from password managers is skipped.".into()),
            ])
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
            Ok(vec![
                (Kind::Head, format!("PAIRING OPEN for {}s  -  PIN: {}", r["secs"], r["pin"].as_str().unwrap_or("?"))),
                (Kind::Dim, "Type this PIN on the other computer (needs: receive on). One try only.".into()),
            ])
        }
        ("/stop", true) => {
            c.post("/api/emergency", &json!({"on": true}))?;
            Ok(vec![(Kind::Bad, "EMERGENCY STOP: control returned, every held key/button released, switching paused. /resume to continue.".into())])
        }
        ("/stop", false) => {
            c.post("/api/release", &json!({}))?;
            Ok(vec![(Kind::Bad, "Released every key and mouse button on this machine.".into())])
        }
        ("/resume", _) => {
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
    /// Cursor position in the input line, counted in characters (not bytes).
    cur: usize,
    history: Vec<String>,
    hist: Option<usize>,
    log: Vec<(Kind, String, u64)>,
    state: Option<Value>,
    offline: bool,
    tx: Sender<Msg>,
    client: Client,
    quit: bool,
    scroll: usize,
    busy: u32,
    started: Instant,
}

/// "/pair" -> "pair" in messages: the UI accepts commands with or without the slash.
fn unslash(s: &str) -> String {
    let cs: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut at_word_start = true;
    for (i, &ch) in cs.iter().enumerate() {
        if ch == '/' && at_word_start && cs.get(i + 1).map(|c| c.is_ascii_alphabetic()).unwrap_or(false) {
            continue;
        }
        out.push(ch);
        at_word_start = ch.is_whitespace() || ch == '(';
    }
    out
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

    fn say(&mut self, kind: Kind, s: impl AsRef<str>) {
        // text can come from other machines: strip anything that could drive the terminal
        let text = unslash(&crate::shared::clean(s.as_ref()));
        let at = self.started.elapsed().as_secs();
        self.log.push((kind, text, at));
        if self.log.len() > 500 {
            self.log.remove(0);
        }
        self.scroll = 0;
    }

    fn help(&mut self) {
        self.say(Kind::Head, "COMMANDS (the leading / is optional)");
        for (c, d) in self.cmds().to_vec() {
            self.say(Kind::Info, format!("{:<10} {}", c.trim_start_matches('/'), d));
        }
        self.say(Kind::Dim, "keys: F2 = emergency stop, F3 = resume, Tab completes, PgUp/PgDn scroll");
    }

    fn run_line(&mut self, line: String) {
        let (client, tx, mode) = (self.client.clone(), self.tx.clone(), self.mode().to_string());
        self.busy += 1;
        std::thread::spawn(move || {
            let out = exec(&client, &mode, &line).unwrap_or_else(|e| vec![(Kind::Bad, e)]);
            let _ = tx.send(Msg::Out(out));
        });
    }

    /// Replace the whole input line and put the cursor at its end.
    fn set_input(&mut self, s: String) {
        self.cur = s.chars().count();
        self.input = s;
    }

    fn byte_at(&self, chars: usize) -> usize {
        self.input.char_indices().nth(chars).map(|(b, _)| b).unwrap_or(self.input.len())
    }

    /// Start of the previous / next word from the cursor (for Ctrl+Left / Ctrl+Right).
    fn word_left(&self) -> usize {
        let cs: Vec<char> = self.input.chars().collect();
        let mut i = self.cur.min(cs.len());
        while i > 0 && cs[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !cs[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    fn word_right(&self) -> usize {
        let cs: Vec<char> = self.input.chars().collect();
        let mut i = self.cur.min(cs.len());
        while i < cs.len() && !cs[i].is_whitespace() {
            i += 1;
        }
        while i < cs.len() && cs[i].is_whitespace() {
            i += 1;
        }
        i
    }

    fn submit(&mut self) {
        let typed = self.input.trim().to_string();
        self.set_input(String::new());
        self.hist = None;
        if typed.is_empty() {
            return;
        }
        if self.history.last() != Some(&typed) {
            self.history.push(typed.clone());
        }
        self.say(Kind::User, format!("❯ {typed}"));
        let line = if typed.starts_with('/') { typed } else { format!("/{typed}") };
        match line.split_whitespace().next().unwrap_or("").to_lowercase().as_str() {
            "/quit" | "/exit" | "/q" => self.quit = true,
            "/help" | "/?" | "/h" => self.help(),
            "/clear" | "/cls" => self.log.clear(),
            _ => self.run_line(line),
        }
    }

    fn complete(&mut self) {
        if self.input.contains(' ') {
            return;
        }
        let typed = format!("/{}", self.input.trim_start_matches('/').to_lowercase());
        if let Some((c, _)) = self.cmds().iter().find(|(c, _)| c.starts_with(&typed)) {
            self.set_input(format!("{} ", c.trim_start_matches('/')));
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('c') | KeyCode::Char('d') if ctrl => self.quit = true,
            KeyCode::F(1) => self.help(),
            KeyCode::F(2) => {
                self.say(Kind::Bad, "EMERGENCY STOP (F2)");
                self.run_line("/stop".into());
            }
            KeyCode::F(3) => self.run_line("/resume".into()),
            // Ctrl+A / Ctrl+E / Ctrl+U / Ctrl+W like a shell
            KeyCode::Char('a') if ctrl => self.cur = 0,
            KeyCode::Char('e') if ctrl => self.cur = self.input.chars().count(),
            KeyCode::Char('u') if ctrl => {
                let b = self.byte_at(self.cur);
                self.input.replace_range(..b, "");
                self.cur = 0;
            }
            KeyCode::Char('w') if ctrl => {
                let from = self.word_left();
                let (a, b) = (self.byte_at(from), self.byte_at(self.cur));
                self.input.replace_range(a..b, "");
                self.cur = from;
            }
            KeyCode::Char(c) if !ctrl => {
                let b = self.byte_at(self.cur);
                self.input.insert(b, c);
                self.cur += 1;
            }
            KeyCode::Backspace => {
                if self.cur > 0 {
                    let (a, b) = (self.byte_at(self.cur - 1), self.byte_at(self.cur));
                    self.input.replace_range(a..b, "");
                    self.cur -= 1;
                }
            }
            KeyCode::Delete => {
                if self.cur < self.input.chars().count() {
                    let (a, b) = (self.byte_at(self.cur), self.byte_at(self.cur + 1));
                    self.input.replace_range(a..b, "");
                }
            }
            KeyCode::Left if ctrl => self.cur = self.word_left(),
            KeyCode::Right if ctrl => self.cur = self.word_right(),
            KeyCode::Left => self.cur = self.cur.saturating_sub(1),
            KeyCode::Right => self.cur = (self.cur + 1).min(self.input.chars().count()),
            KeyCode::Home => self.cur = 0,
            KeyCode::End => self.cur = self.input.chars().count(),
            KeyCode::Enter => self.submit(),
            KeyCode::Esc => self.set_input(String::new()),
            KeyCode::Tab => self.complete(),
            KeyCode::Up => {
                if !self.history.is_empty() {
                    let i = self.hist.map(|i| i.saturating_sub(1)).unwrap_or(self.history.len() - 1);
                    self.hist = Some(i);
                    self.set_input(self.history[i].clone());
                }
            }
            KeyCode::Down => {
                if let Some(i) = self.hist {
                    if i + 1 < self.history.len() {
                        self.hist = Some(i + 1);
                        self.set_input(self.history[i + 1].clone());
                    } else {
                        self.hist = None;
                        self.set_input(String::new());
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
        Kind::Head => Style::new().fg(MAGENTA).add_modifier(Modifier::BOLD),
        Kind::Info => Style::new().fg(Color::Rgb(220, 224, 235)),
        Kind::Good => Style::new().fg(GOOD),
        Kind::Bad => Style::new().fg(BAD),
        Kind::Dim => Style::new().fg(DIM),
        Kind::User => Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
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

fn chip(text: &str, bg: Color) -> Span<'static> {
    Span::styled(format!(" {text} "), Style::new().fg(Color::Black).bg(bg).add_modifier(Modifier::BOLD))
}

fn heading(text: &str) -> Line<'static> {
    Line::from(Span::styled(format!(" {text}"), Style::new().fg(MAGENTA).add_modifier(Modifier::BOLD)))
}

fn kv(k: &str, v: String, vc: Color) -> Line<'static> {
    Line::from(vec![Span::styled(format!(" {k:<11}"), Style::new().fg(DIM)), Span::styled(v, Style::new().fg(vc))])
}

fn arrow(edge: &str) -> &'static str {
    match edge {
        "Left" => "←",
        "Right" => "→",
        "Top" => "↑",
        _ => "↓",
    }
}

fn status_chips(app: &App) -> Line<'static> {
    let mut v: Vec<Span> = vec![Span::raw(" ")];
    let s = match (&app.state, app.offline) {
        (Some(s), false) => s,
        _ => {
            v.push(chip("OFFLINE", BAD));
            v.push(Span::styled("  the SmoothKvm app/service is not running", Style::new().fg(DIM)));
            return Line::from(v);
        }
    };
    if s["mode"] == "receiver" {
        let n = s["sessions"].as_array().map(|a| a.len()).unwrap_or(0);
        v.push(chip("SCREEN MODE", ACCENT));
        v.push(Span::raw(" "));
        v.push(chip(&format!("{n} CONNECTED"), if n > 0 { GOOD } else { WARN }));
        if s["pairing_open_secs"].as_u64().unwrap_or(0) > 0 {
            v.push(Span::raw(" "));
            v.push(chip("PAIRING OPEN", WARN));
        }
    } else {
        let remotes = s["config"]["remotes"].as_array().cloned().unwrap_or_default();
        let enabled = remotes.iter().filter(|r| r["enabled"].as_bool().unwrap_or(true)).count();
        let up = remotes.iter().filter(|r| s["status"][r["name"].as_str().unwrap_or("")]["state"] == "connected").count();
        v.push(chip("CONTROLLER", ACCENT));
        v.push(Span::raw(" "));
        v.push(chip(&format!("{up}/{enabled} ONLINE"), if up > 0 { GOOD } else { WARN }));
        if s["paused"].as_bool().unwrap_or(false) {
            v.push(Span::raw(" "));
            v.push(chip("■ STOPPED", BAD));
        }
        if s["game_active"].as_bool().unwrap_or(false) {
            v.push(Span::raw(" "));
            v.push(chip("GAME MODE ACTIVE", WARN));
        }
    }
    let u = &s["update"];
    if let Some(b) = u["busy"].as_str() {
        v.push(Span::raw(" "));
        v.push(chip(&b.to_uppercase(), ACCENT));
    } else if u["available"].as_bool().unwrap_or(false) {
        v.push(Span::raw(" "));
        v.push(chip(&format!("UPDATE {} AVAILABLE - type update", u["latest"].as_str().unwrap_or("")), GOOD));
    }
    Line::from(v)
}

/// 3x5 pixel digits, drawn with half blocks so each digit is 3 columns by 3 rows: easy to read from a distance.
const DIGITS: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111],
    [0b010, 0b110, 0b010, 0b010, 0b111],
    [0b111, 0b001, 0b111, 0b100, 0b111],
    [0b111, 0b001, 0b111, 0b001, 0b111],
    [0b101, 0b101, 0b111, 0b001, 0b001],
    [0b111, 0b100, 0b111, 0b001, 0b111],
    [0b111, 0b100, 0b111, 0b101, 0b111],
    [0b111, 0b001, 0b001, 0b001, 0b001],
    [0b111, 0b101, 0b111, 0b101, 0b111],
    [0b111, 0b101, 0b111, 0b001, 0b111],
];

/// The pairing code in big digits ("1234 5678"), 3 lines tall.
fn big_code(pin: &str) -> Vec<Line<'static>> {
    let mut rows = vec![String::from("  "), String::from("  "), String::from("  ")];
    for (i, ch) in pin.chars().enumerate() {
        let Some(d) = ch.to_digit(10) else { continue };
        let g = DIGITS[d as usize];
        if i == 4 {
            for r in rows.iter_mut() {
                r.push_str("  ");
            }
        }
        for (line, r) in rows.iter_mut().enumerate() {
            for col in (0..3).rev() {
                let top = g[line * 2] >> col & 1 == 1;
                let bottom = line * 2 + 1 < 5 && g[line * 2 + 1] >> col & 1 == 1;
                r.push(match (top, bottom) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    _ => ' ',
                });
            }
            r.push(' ');
        }
    }
    rows.into_iter().map(|r| Line::styled(r, Style::new().fg(WARN).add_modifier(Modifier::BOLD))).collect()
}

/// "THIS PC  name" and its addresses, so you know what to type on the other computer.
fn whoami_lines(s: &Value) -> Vec<Line<'static>> {
    let mut out = vec![kv("THIS PC", s["host"].as_str().unwrap_or("?").to_string(), Color::White)];
    let port = s["port"].as_u64().unwrap_or(24800);
    for a in s["addrs"].as_array().cloned().unwrap_or_default() {
        let a = a.as_str().unwrap_or("").to_string();
        let label = if crate::discover::is_tailscale(&a) { "TAILSCALE" } else { "IP" };
        let shown = if port != 24800 { format!("{a}:{port}") } else { a };
        out.push(kv(label, shown, ACCENT));
    }
    out.push(Line::raw(""));
    out
}

fn transfer_lines(s: &Value) -> Vec<Line<'static>> {
    let t = &s["transfer"];
    if t.is_null() {
        return vec![];
    }
    let (done, total) = (t["done_bytes"].as_u64().unwrap_or(0), t["total_bytes"].as_u64().unwrap_or(0).max(1));
    let pct = (done as f64 / total as f64 * 100.0).min(100.0);
    let dir = if t["dir"] == "send" { "→" } else { "←" };
    let mut out = vec![heading(&format!("FILES {dir} {}", t["peer"].as_str().unwrap_or("?")))];
    if t["finished"].as_bool().unwrap_or(false) {
        let ok = t["ok"].as_bool().unwrap_or(false);
        out.push(Line::styled(format!("  {}", t["detail"].as_str().unwrap_or("")), Style::new().fg(if ok { GOOD } else { BAD })));
    } else {
        let w = 20usize;
        let fill = (pct / 100.0 * w as f64) as usize;
        out.push(Line::from(vec![
            Span::styled(format!("  {}", "█".repeat(fill)), Style::new().fg(ACCENT)),
            Span::styled("░".repeat(w - fill), Style::new().fg(DIM)),
            Span::raw(format!(" {pct:.0}%")),
        ]));
        out.push(Line::styled(
            format!("  {} / {}  {}/s", crate::xfer::human(done), crate::xfer::human(total), crate::xfer::human(t["rate"].as_u64().unwrap_or(0))),
            Style::new().fg(DIM),
        ));
        out.push(Line::styled(format!("  {}", t["file"].as_str().unwrap_or("").chars().rev().take(34).collect::<Vec<_>>().into_iter().rev().collect::<String>()), Style::new().fg(DIM)));
    }
    out.push(Line::raw(""));
    out
}

fn left_lines(app: &App) -> Vec<Line<'static>> {
    let mut out: Vec<Line> = Vec::new();
    let Some(s) = app.state.as_ref().filter(|_| !app.offline) else {
        out.push(Line::styled(" waiting for the app...", Style::new().fg(DIM)));
        return out;
    };
    let peers = s["peers"].as_array().map(|a| a.len()).unwrap_or(0);
    out.extend(transfer_lines(s));
    let open = s["pairing_open_secs"].as_u64().unwrap_or(0);
    if open > 0 {
        out.push(heading(&format!("PAIRING CODE  ({}:{:02} left, works once)", open / 60, open % 60)));
        out.extend(big_code(s["pairing_pin"].as_str().unwrap_or("")));
        out.push(Line::styled("  type it on the other computer", Style::new().fg(DIM)));
        out.push(Line::raw(""));
    }
    out.extend(whoami_lines(s));
    if s["mode"] == "receiver" {
        out.push(heading("CONNECTIONS"));
        let sessions = s["sessions"].as_array().cloned().unwrap_or_default();
        if sessions.is_empty() {
            out.push(Line::styled("  nobody connected", Style::new().fg(DIM)));
        }
        for x in sessions {
            out.push(Line::from(vec![
                Span::styled("  ▌ ", Style::new().fg(GOOD)),
                Span::raw(format!("{:<16}", x["peer"].as_str().unwrap_or("?"))),
                Span::styled(format!("{}s", x["secs"]), Style::new().fg(DIM)),
            ]));
        }
        out.push(Line::raw(""));
        if open == 0 {
            out.push(heading("PAIRING"));
            out.push(Line::styled("  closed - type  pair  to show a code", Style::new().fg(DIM)));
            out.push(Line::raw(""));
        }
        out.push(kv("DISPLAY", s["display"]["detail"].as_str().unwrap_or("?").to_string(), Color::White));
        out.push(kv("PAIRED", format!("{peers} device(s)"), Color::White));
        return out;
    }
    out.push(heading("SCREENS"));
    let remotes = s["config"]["remotes"].as_array().cloned().unwrap_or_default();
    if remotes.is_empty() {
        out.push(Line::styled("  none yet", Style::new().fg(DIM)));
        out.push(Line::styled("  add Server right 100.64.0.5:24800", Style::new().fg(DIM)));
    }
    for r in remotes {
        let name = r["name"].as_str().unwrap_or("?");
        let state = s["status"][name]["state"].as_str().unwrap_or("connecting");
        let (col, label) = match state {
            "connected" => (GOOD, "ONLINE"),
            "not_paired" => (WARN, "NOT PAIRED"),
            "disabled" => (DIM, "OFF"),
            "offline" => (BAD, "OFFLINE"),
            _ => (WARN, "..."),
        };
        out.push(Line::from(vec![
            Span::styled("  ▌ ", Style::new().fg(col)),
            Span::raw(format!("{:<12}", name.chars().take(11).collect::<String>())),
            Span::styled(format!("{} {:<7}", arrow(r["edge"].as_str().unwrap_or("")), r["edge"].as_str().unwrap_or("").to_lowercase()), Style::new().fg(DIM)),
            Span::styled(label, Style::new().fg(col).add_modifier(Modifier::BOLD)),
        ]));
    }
    out.push(Line::raw(""));
    let an = &s["config"]["animation"];
    out.push(heading("EFFECT"));
    out.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(an["visual"].as_str().unwrap_or("?").to_lowercase(), Style::new().fg(ACCENT)),
        Span::styled(format!(" · {} · {}ms · {}px", an["palette"].as_str().unwrap_or("?").to_lowercase(), an["duration_ms"], an["size_px"]), Style::new().fg(DIM)),
    ]));
    out.push(kv("MOTION", an["style"].as_str().unwrap_or("?").to_lowercase(), Color::White));
    out.push(Line::raw(""));
    let guard = s["config"]["game_guard"].as_bool().unwrap_or(true);
    out.push(kv("GAME MODE", if guard { "on".into() } else { "off".into() }, if guard { GOOD } else { DIM }));
    out.push(kv("EMERGENCY", "F2 stop · F3 resume".into(), WARN));
    out.push(kv("", "or Scroll Lock x3".into(), DIM));
    out.push(Line::raw(""));
    let recv = s["config"]["receive"].as_bool().unwrap_or(false);
    out.push(kv("THIS PC", if recv { "can be controlled".into() } else { "controls only".into() }, if recv { ACCENT } else { DIM }));
    if recv {
        let n = s["sessions"].as_array().map(|a| a.len()).unwrap_or(0);
        out.push(kv("", format!("{n} connected"), Color::White));
    }
    out.push(kv("PAIRED", format!("{peers} device(s)"), Color::White));
    out
}

fn suggestions(app: &App) -> Vec<Span<'static>> {
    let typed = app.input.to_lowercase();
    let typed = typed.trim_start_matches('/');
    match typed.split_once(' ') {
        Some((cmd, _)) => app
            .cmds()
            .iter()
            .find(|(c, _)| c.trim_start_matches('/') == cmd)
            .map(|(_, d)| vec![Span::styled(format!(" {}", unslash(d)), Style::new().fg(WARN))])
            .unwrap_or_default(),
        None => {
            let mut v = vec![Span::raw(" ")];
            for (c, _) in app.cmds().iter().filter(|(c, _)| c.trim_start_matches('/').starts_with(typed)) {
                v.push(Span::styled(format!("[{}]", c.trim_start_matches('/')), Style::new().fg(ACCENT)));
                v.push(Span::raw(" "));
            }
            v
        }
    }
}

fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let [title, body, chips, input, foot]: [Rect; 5] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    // title bar
    let tb = Block::bordered()
        .border_type(BorderType::Double)
        .border_style(Style::new().fg(ACCENT))
        .title(Span::styled(" ◢◤ S M O O T H K V M ◢◤ ", Style::new().fg(MAGENTA).add_modifier(Modifier::BOLD)))
        .title(Line::from(Span::styled(format!(" v{VERSION} "), Style::new().fg(DIM))).right_aligned());
    f.render_widget(Paragraph::new(status_chips(app)).block(tb), title);

    // two panels
    let [left, right]: [Rect; 2] = Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(body);
    let lb = Block::bordered().border_type(BorderType::Double).border_style(Style::new().fg(ACCENT)).title(Span::styled(" STATUS ", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)));
    f.render_widget(Paragraph::new(left_lines(app)).block(lb), left);

    let rb = Block::bordered().border_type(BorderType::Double).border_style(Style::new().fg(MAGENTA)).title(Span::styled(" ACTIVITY ", Style::new().fg(MAGENTA).add_modifier(Modifier::BOLD)));
    let inner_w = right.width.saturating_sub(2) as usize;
    let inner_h = right.height.saturating_sub(2) as usize;
    let mut rows: Vec<Line> = Vec::new();
    if app.log.is_empty() {
        rows.push(Line::styled(" ready.  type  help  or press F1", Style::new().fg(DIM)));
    }
    for (k, text, at) in &app.log {
        let stamp = format!("{:02}:{:02} ", at / 60 % 100, at % 60);
        for (i, chunk) in wrap(text, inner_w.saturating_sub(7)).into_iter().enumerate() {
            let lead = if i == 0 { stamp.clone() } else { "      ".to_string() };
            rows.push(Line::from(vec![Span::styled(lead, Style::new().fg(DIM)), Span::styled(chunk, style_of(*k))]));
        }
    }
    if app.busy > 0 {
        rows.push(Line::styled("      working...", Style::new().fg(WARN)));
    }
    let total = rows.len();
    let end = total.saturating_sub(app.scroll.min(total.saturating_sub(1)));
    let start = end.saturating_sub(inner_h);
    f.render_widget(Paragraph::new(rows[start..end].to_vec()).block(rb), right);

    // command hints, prompt, footer
    f.render_widget(Paragraph::new(Line::from(suggestions(app))), chips);
    let bg = Color::Rgb(22, 24, 38);
    // A line longer than the box scrolls sideways so the cursor always stays visible.
    let room = (input.width as usize).saturating_sub(14).max(1);
    let skip = app.cur.saturating_sub(room);
    let shown: String = app.input.chars().skip(skip).take(room).collect();
    let prompt = Line::from(vec![
        Span::styled(" smoothkvm", Style::new().fg(MAGENTA).bg(bg).add_modifier(Modifier::BOLD)),
        Span::styled(" ❯ ", Style::new().fg(ACCENT).bg(bg)),
        Span::styled(shown, Style::new().fg(Color::White).bg(bg)),
    ]);
    f.render_widget(Paragraph::new(prompt).style(Style::new().bg(bg)), input);
    let cx = input.x + 13 + (app.cur - skip) as u16;
    f.set_cursor_position((cx.min(input.right().saturating_sub(1)), input.y));
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" F1", Style::new().fg(ACCENT)),
            Span::styled(" help  ", Style::new().fg(DIM)),
            Span::styled("F2", Style::new().fg(BAD).add_modifier(Modifier::BOLD)),
            Span::styled(" STOP  ", Style::new().fg(DIM)),
            Span::styled("F3", Style::new().fg(GOOD)),
            Span::styled(" resume  ", Style::new().fg(DIM)),
            Span::styled("Tab", Style::new().fg(ACCENT)),
            Span::styled(" complete  ", Style::new().fg(DIM)),
            Span::styled("PgUp/PgDn", Style::new().fg(ACCENT)),
            Span::styled(" scroll  ", Style::new().fg(DIM)),
            Span::styled("Ctrl+C", Style::new().fg(ACCENT)),
            Span::styled(" quit", Style::new().fg(DIM)),
        ])),
        foot,
    );
}

fn app_loop(term: &mut DefaultTerminal, client: Client, embedded: bool) -> io::Result<()> {
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
    let mut app = App { input: String::new(), cur: 0, history: vec![], hist: None, log: vec![], state: None, offline: true, tx, client, quit: false, scroll: 0, busy: 0, started: Instant::now() };
    app.say(
        Kind::Dim,
        if embedded {
            "SmoothKvm runs only while this window is open. Closing it (or /quit) stops everything."
        } else {
            "Connected to the SmoothKvm background service. Closing this window leaves it running; stop it with:  sudo systemctl stop smoothkvm"
        },
    );
    while !app.quit {
        #[cfg(unix)]
        if HANGUP.load(std::sync::atomic::Ordering::SeqCst) {
            app.quit = true;
        }
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

/// Set by SIGHUP/SIGTERM (terminal window closed): the app quits instead of lingering invisibly.
#[cfg(unix)]
pub static HANGUP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_hangup(_: libc::c_int) {
    HANGUP.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub fn run() -> io::Result<()> {
    #[cfg_attr(windows, allow(unused_mut))]
    let mut embedded = false;
    if !ctl::alive() {
        #[cfg(windows)]
        {
            eprintln!("Starting SmoothKvm...");
            let _ = crate::app::start(false);
        }
        #[cfg(unix)]
        {
            // No background service: run SmoothKvm inside this window. Closing it stops everything.
            #[cfg(target_os = "linux")]
            crate::ensure_input_group();
            unsafe {
                libc::signal(libc::SIGHUP, on_hangup as *const () as usize);
                libc::signal(libc::SIGTERM, on_hangup as *const () as usize);
            }
            crate::start_embedded()?;
            embedded = true;
        }
    }
    let client = Client::new()?;
    let mut term = ratatui::try_init()?;
    let res = app_loop(&mut term, client, embedded);
    ratatui::restore();
    #[cfg(unix)]
    if embedded {
        crate::stop_embedded();
    }
    res
}

#[cfg(test)]
mod tests {
    use super::{big_code, split_args};

    #[test]
    fn big_code_is_three_lines_and_readable() {
        let rows: Vec<String> = big_code("10").iter().map(|l| l.spans.iter().map(|s| s.content.to_string()).collect()).collect();
        assert_eq!(rows.len(), 3);
        // "1" then "0" in half-block pixels
        assert_eq!(rows[0], "  ▄█  █▀█ ");
        assert_eq!(rows[1], "   █  █ █ ");
        assert_eq!(rows[2], "  ▀▀▀ ▀▀▀ ");
        assert_eq!(big_code("12345678")[0].spans[0].content.chars().count(), 2 + 8 * 4 + 2);
    }

    #[test]
    fn quoted_paths_stay_together() {
        assert_eq!(split_args(r#"a "b c" 'd e' f"#), vec!["a", "b c", "d e", "f"]);
        assert_eq!(split_args(r#""C:\My Files\x.step" to Ubuntu"#), vec![r"C:\My Files\x.step", "to", "Ubuntu"]);
        assert_eq!(split_args("  "), Vec::<String>::new());
    }
}
