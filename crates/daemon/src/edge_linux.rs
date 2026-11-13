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

/// side: 0 = left, 1 = right, 2 = top, 3 = bottom, -1 = not on an edge. Sent only when it changes.
const SCRIPT: &str = r#"// SmoothKvm: tells SmoothKvm when the pointer touches a screen edge, so it can move the mouse to the
// computer on that side (KDE does not let other programs see the pointer). It only reports the edge.
var last = -2;
function report() {
    var p = workspace.cursorPos;
    var g = workspace.virtualScreenGeometry;
    var side = -1, frac = 0;
    if (p.x <= g.x) { side = 0; frac = (p.y - g.y) / Math.max(1, g.height - 1); }
    else if (p.x >= g.x + g.width - 1) { side = 1; frac = (p.y - g.y) / Math.max(1, g.height - 1); }
    else if (p.y <= g.y) { side = 2; frac = (p.x - g.x) / Math.max(1, g.width - 1); }
    else if (p.y >= g.y + g.height - 1) { side = 3; frac = (p.x - g.x) / Math.max(1, g.width - 1); }
    if (side === last) return;
    last = side;
    callDBus("org.smoothkvm.Edge", "/Edge", "org.smoothkvm.Edge", "Edge", side + " " + Math.round(frac * 1000));
}
workspace.cursorPosChanged.connect(report);
"#;

/// Parse "1 500" into (Right, 0.5); "-1 0" into None (away from the edges).
pub fn parse(arg: &str) -> Result<Option<(Edge, f32)>, ()> {
    let mut it = arg.split_whitespace();
    let side: i32 = it.next().and_then(|s| s.parse().ok()).ok_or(())?;
    let frac = it.next().and_then(|s| s.parse::<f32>().ok()).unwrap_or(500.0) / 1000.0;
    let edge = match side {
        -1 => return Ok(None),
        0 => Edge::Left,
        1 => Edge::Right,
        2 => Edge::Top,
        3 => Edge::Bottom,
        _ => return Err(()),
    };
    Ok(Some((edge, frac.clamp(0.0, 1.0))))
}

fn script_path() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".local").join("share").join("smoothkvm").join("kwin-edge.js")
}

fn load_script(bus: &mut Bus) -> io::Result<()> {
    let path = script_path();
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&path, SCRIPT)?;
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
    load_script(&mut bus).map_err(|e| io::Error::new(e.kind(), format!("KWin (KDE Plasma) is needed for edge switching: {e}")))?;
    if !*announced {
        *announced = true;
        shared::info("Edge switching is on: push the mouse off the side of the screen where another computer is (Scroll Lock still works too)");
    }
    bus.set_timeout(None)?;
    loop {
        let m = bus.read()?;
        if m.kind != METHOD_CALL {
            continue;
        }
        if m.interface == IFACE && m.member == "Edge" {
            match m.args.first().map(|a| parse(a)) {
                Some(Ok(Some((edge, frac)))) => crate::capture_linux::edge_hit(edge, frac),
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
        assert_eq!(parse("1 500"), Ok(Some((Edge::Right, 0.5))));
        assert_eq!(parse("0 0"), Ok(Some((Edge::Left, 0.0))));
        assert_eq!(parse("3 1500"), Ok(Some((Edge::Bottom, 1.0))));
        assert_eq!(parse("-1 0"), Ok(None));
        assert!(parse("7 1").is_err());
        assert!(parse("").is_err());
    }
}
