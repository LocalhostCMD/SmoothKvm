//! Screen-edge switching on KDE Plasma. Wayland does not let ordinary programs see where the pointer is, but
//! KWin (the KDE window manager) does, and it runs small user scripts. We load a 20-line script into KWin that
//! calls us over D-Bus whenever the pointer touches (or leaves) an edge of the desktop. The script only reports
//! which edge and where along it; it cannot do anything else, and it is removed when you uninstall.
use crate::dbus_min::{Bus, METHOD_CALL};
use crate::shared;
use kvm_protocol::Edge;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

pub const BUS_NAME: &str = "org.smoothkvm.Edge";
const IFACE: &str = "org.smoothkvm.Edge";
const SCRIPT_NAME: &str = "smoothkvm-edge";

/// Reports "side permille along length" (side: 0 = left, 1 = right, 2 = top, 3 = bottom, -1 = not on an
/// edge), only when the side changes or the pointer moves between a corner zone and the middle of the edge.
const SCRIPT: &str = r#"// SmoothKvm: tells SmoothKvm when the pointer touches a screen edge, so it can move the mouse to the
// computer on that side (KDE does not let other programs see the pointer). It only reports the edge.
var CORNER = @CORNER@;
var last = "";
function report() {
    var p = workspace.cursorPos;
    var g = workspace.virtualScreenGeometry;
    var side = -1, along = 0, len = 1;
    if (p.x <= g.x) { side = 0; along = p.y - g.y; len = g.height; }
    else if (p.x >= g.x + g.width - 1) { side = 1; along = p.y - g.y; len = g.height; }
    else if (p.y <= g.y) { side = 2; along = p.x - g.x; len = g.width; }
    else if (p.y >= g.y + g.height - 1) { side = 3; along = p.x - g.x; len = g.width; }
    var zone = -1;
    if (side >= 0) { var c = Math.min(CORNER, Math.floor(len / 3)); zone = (along < c || along >= len - c) ? 0 : 1; }
    var key = side + ":" + zone;
    if (key === last) return;
    last = key;
    var frac = along / Math.max(1, len - 1);
    callDBus("org.smoothkvm.Edge", "/Edge", "org.smoothkvm.Edge", "Edge", side + " " + Math.round(frac * 1000) + " " + Math.round(along) + " " + Math.round(len));
}
workspace.cursorPosChanged.connect(report);
"#;

/// Where on an edge the pointer is: the edge, how far along it (0..1), and (pixels along, edge length).
pub type EdgeAt = (Edge, f32, Option<(i64, i64)>);

/// Parse "1 500 540 1080" into (Right, 0.5, (540, 1080)); "-1 0" into None (away from the edges).
/// (The older "side permille" form without pixels is accepted too.)
pub fn parse(arg: &str) -> Result<Option<EdgeAt>, ()> {
    let mut it = arg.split_whitespace();
    let side: i32 = it.next().and_then(|s| s.parse().ok()).ok_or(())?;
    let frac = it.next().and_then(|s| s.parse::<f32>().ok()).unwrap_or(500.0) / 1000.0;
    let px = match (it.next().and_then(|s| s.parse::<i64>().ok()), it.next().and_then(|s| s.parse::<i64>().ok())) {
        (Some(a), Some(l)) if l > 0 => Some((a.clamp(0, l), l)),
        _ => None,
    };
    let edge = match side {
        -1 => return Ok(None),
        0 => Edge::Left,
        1 => Edge::Right,
        2 => Edge::Top,
        3 => Edge::Bottom,
        _ => return Err(()),
    };
    Ok(Some((edge, frac.clamp(0.0, 1.0), px)))
}

fn script_path() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".local").join("share").join("smoothkvm").join("kwin-edge.js")
}

fn load_script(bus: &mut Bus, corner: u32) -> io::Result<()> {
    let path = script_path();
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&path, SCRIPT.replace("@CORNER@", &corner.min(400).to_string()))?;
    let p = path.to_string_lossy().to_string();
    // replace an older copy, then load and start it
    let _ = bus.call("org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting", "unloadScript", &[SCRIPT_NAME]);
    bus.call("org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting", "loadScript", &[&p, SCRIPT_NAME])?;
    bus.call("org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting", "start", &[])?;
    Ok(())
}

fn run_once(announced: &mut bool) -> io::Result<()> {
    let mut bus = Bus::session()?;
    if !bus.own_name(BUS_NAME)? {
        return Err(io::Error::new(io::ErrorKind::AddrInUse, "another SmoothKvm already handles screen edges"));
    }
    let mut corner = crate::store::load_config().corner_px;
    load_script(&mut bus, corner).map_err(|e| io::Error::new(e.kind(), format!("KWin (KDE Plasma) is needed for edge switching: {e}")))?;
    if !*announced {
        *announced = true;
        shared::info("Edge switching is on: push the mouse off the side of the screen where another computer is (Scroll Lock still works too)");
    }
    // wake up now and then to notice a changed corner size (the script has it built in)
    bus.set_timeout(Some(Duration::from_secs(3)))?;
    loop {
        let m = match bus.read() {
            Ok(m) => m,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                let now = crate::store::load_config().corner_px;
                if now != corner {
                    corner = now;
                    load_script(&mut bus, corner)?;
                    bus.set_timeout(Some(Duration::from_secs(3)))?;
                }
                continue;
            }
            Err(e) => return Err(e),
        };
        if m.kind != METHOD_CALL {
            continue;
        }
        if m.interface == IFACE && m.member == "Edge" {
            match m.args.first().map(|a| parse(a)) {
                Some(Ok(Some((edge, frac, px)))) => crate::capture_linux::edge_hit(edge, frac, px),
                Some(Ok(None)) => crate::capture_linux::edge_away(),
                _ => {}
            }
        }
        bus.reply(&m)?;
    }
}

/// Keep the KWin script loaded and listen to it (reconnects after logging out and in again).
pub fn start() {
    std::thread::spawn(|| {
        let mut announced = false;
        let mut last_err = String::new();
        loop {
            if let Err(e) = run_once(&mut announced) {
                let msg = e.to_string();
                if msg != last_err {
                    shared::info(&format!("Edge switching is not available right now ({msg}); Scroll Lock still switches screens"));
                    last_err = msg;
                }
            }
            std::thread::sleep(Duration::from_secs(10));
        }
    });
}

/// Remove the script from KWin (used when uninstalling).
#[allow(dead_code)]
pub fn unload() {
    if let Ok(mut bus) = Bus::session() {
        let _ = bus.call("org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting", "unloadScript", &[SCRIPT_NAME]);
    }
    let _ = std::fs::remove_file(script_path());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_edge_reports() {
        assert_eq!(parse("1 500 540 1080"), Ok(Some((Edge::Right, 0.5, Some((540, 1080))))));
        assert_eq!(parse("1 500"), Ok(Some((Edge::Right, 0.5, None))));
        assert_eq!(parse("0 0"), Ok(Some((Edge::Left, 0.0, None))));
        assert_eq!(parse("3 1500"), Ok(Some((Edge::Bottom, 1.0, None))));
        assert_eq!(parse("-1 0"), Ok(None));
        assert!(parse("7 1").is_err());
        assert!(parse("").is_err());
    }
}
