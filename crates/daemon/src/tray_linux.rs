//! The tray icon on Linux desktops (KDE Plasma, and anything else that speaks StatusNotifierItem).
//!
//! Click it: the small panel. Right-click: the full app window. Middle-click: emergency stop / resume.
//! SmoothKvm runs as a sandboxed service, so the windows are started in your own desktop session
//! (`systemd-run --user`), never as children of the service.
use crate::dbus_min::{pad, put_sig, put_str, Bus, METHOD_CALL};
use crate::shared;
use std::io;
use std::process::{Command, Stdio};
use std::time::Duration;

const ITEM: &str = "org.kde.StatusNotifierItem";
const PROPS: &str = "org.freedesktop.DBus.Properties";
const PATH: &str = "/StatusNotifierItem";
const TOLD: &str = "Tray icon is on: click it for the panel, right-click for the app window, middle-click to stop or resume";

enum Val<'a> {
    S(&'a str),
    B(bool),
    I(i32),
    /// (title, text) of a tooltip without icons
    Tip(&'a str, &'a str),
}

fn variant(b: &mut Vec<u8>, v: &Val) {
    match v {
        Val::S(s) => {
            put_sig(b, "s");
            put_str(b, s);
        }
        Val::B(x) => {
            put_sig(b, "b");
            pad(b, 4);
            b.extend_from_slice(&(*x as u32).to_le_bytes());
        }
        Val::I(x) => {
            put_sig(b, "i");
            pad(b, 4);
            b.extend_from_slice(&x.to_le_bytes());
        }
        Val::Tip(title, text) => {
            put_sig(b, "(sa(iiay)ss)");
            pad(b, 8);
            put_str(b, "");
            pad(b, 4);
            b.extend_from_slice(&0u32.to_le_bytes()); // no pixmaps
            pad(b, 8); // (where the first array element would start)
            put_str(b, title);
            put_str(b, text);
        }
    }
}

fn tip() -> String {
    if crate::capture_linux::is_paused() {
        "Stopped: screen switching is paused".into()
    } else {
        "Click for the panel, right-click for settings".into()
    }
}

fn props(tip: &str) -> Vec<(&'static str, Val<'_>)> {
    vec![
        ("Category", Val::S("ApplicationStatus")),
        ("Id", Val::S("smoothkvm")),
        ("Title", Val::S("SmoothKvm")),
        ("Status", Val::S("Active")),
        ("WindowId", Val::I(0)),
        ("IconName", Val::S("smoothkvm")),
        ("IconThemePath", Val::S("")),
        ("OverlayIconName", Val::S("")),
        ("AttentionIconName", Val::S("")),
        ("ItemIsMenu", Val::B(false)),
        ("ToolTip", Val::Tip("SmoothKvm", tip)),
    ]
}

/// All properties as a{sv}.
fn all_props(tip: &str) -> Vec<u8> {
    let mut b = vec![0u8; 4];
    pad(&mut b, 8);
    let start = b.len();
    for (k, v) in props(tip) {
        pad(&mut b, 8);
        put_str(&mut b, k);
        variant(&mut b, &v);
    }
    let len = (b.len() - start) as u32;
    b[..4].copy_from_slice(&len.to_le_bytes());
    b
}

const INTROSPECT: &str = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node><interface name="org.kde.StatusNotifierItem">
<property name="Category" type="s" access="read"/><property name="Id" type="s" access="read"/>
<property name="Title" type="s" access="read"/><property name="Status" type="s" access="read"/>
<property name="WindowId" type="i" access="read"/><property name="IconName" type="s" access="read"/>
<property name="IconThemePath" type="s" access="read"/><property name="OverlayIconName" type="s" access="read"/>
<property name="AttentionIconName" type="s" access="read"/><property name="ItemIsMenu" type="b" access="read"/>
<property name="ToolTip" type="(sa(iiay)ss)" access="read"/>
<method name="Activate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
<method name="SecondaryActivate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
<method name="ContextMenu"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
<method name="Scroll"><arg type="i" direction="in"/><arg type="s" direction="in"/></method>
</interface></node>"#;

/// Open the app window (or the small tray panel) in the desktop session of the user we run as.
pub fn open(tray: bool) {
    let exe = std::env::current_exe().unwrap_or_else(|_| "/usr/bin/SmoothKvm".into());
    let uid = unsafe { libc::getuid() };
    let run = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{uid}"));
    let mut c = Command::new("systemd-run");
    c.args(["--user", "--quiet", "--collect", "--"]).arg(&exe).arg("app");
    if tray {
        c.arg("--tray");
    }
    c.env("XDG_RUNTIME_DIR", &run).env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={run}/bus"));
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let ok = c.status().map(|s| s.success()).unwrap_or(false);
    if !ok {
        // no user systemd: try directly
        if let Err(e) = crate::linux_app::open_window(tray) {
            shared::warn(&format!("Could not open the SmoothKvm window: {e}"));
        }
    }
}

fn register(bus: &mut Bus, name: &str) -> bool {
    bus.call("org.kde.StatusNotifierWatcher", "/StatusNotifierWatcher", "org.kde.StatusNotifierWatcher", "RegisterStatusNotifierItem", &[name]).is_ok()
}

fn run_once(told: &mut bool) -> io::Result<()> {
    let mut bus = Bus::session()?;
    let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
    if !bus.own_name(&name)? {
        return Err(io::Error::new(io::ErrorKind::AddrInUse, "could not take the tray icon's bus name"));
    }
    // Start by registering, and do it again now and then: the desktop's tray may start after us, or restart.
    bus.set_timeout(Some(Duration::from_millis(50)))?;
    loop {
        let m = match bus.read() {
            Ok(m) => m,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                bus.set_timeout(Some(Duration::from_secs(5)))?;
                let registered = register(&mut bus, &name);
                if registered && !*told {
                    *told = true;
                    shared::info(TOLD);
                }
                bus.set_timeout(Some(Duration::from_secs(if registered { 60 } else { 10 })))?;
                continue;
            }
            Err(e) => return Err(e),
        };
        if m.kind != METHOD_CALL {
            continue;
        }
        let t = tip();
        match (m.interface.as_str(), m.member.as_str()) {
            (PROPS, "GetAll") => bus.reply_body(&m, "a{sv}", &all_props(&t))?,
            (PROPS, "Get") => match props(&t).into_iter().find(|(k, _)| Some(*k) == m.args.get(1).map(|s| s.as_str())) {
                Some((_, v)) => {
                    let mut b = Vec::new();
                    variant(&mut b, &v);
                    bus.reply_body(&m, "v", &b)?
                }
                None => bus.reply_error(&m, "org.freedesktop.DBus.Error.UnknownProperty")?,
            },
            ("org.freedesktop.DBus.Introspectable", "Introspect") => {
                let mut b = Vec::new();
                put_str(&mut b, if m.path == PATH { INTROSPECT } else { "<node><node name=\"StatusNotifierItem\"/></node>" });
                bus.reply_body(&m, "s", &b)?
            }
            ("org.freedesktop.DBus.Peer", "Ping") => bus.reply(&m)?,
            (ITEM, member) => {
                bus.reply(&m)?;
                match member {
                    "Activate" => {
                        std::thread::spawn(|| open(true));
                    }
                    "ContextMenu" => {
                        std::thread::spawn(|| open(false));
                    }
                    "SecondaryActivate" => {
                        if crate::capture_linux::is_paused() {
                            crate::capture_linux::Controller.resume();
                        } else {
                            crate::capture_linux::Controller.emergency_stop();
                            crate::release_everything();
                        }
                    }
                    _ => {}
                }
            }
            _ => bus.reply_error(&m, "org.freedesktop.DBus.Error.UnknownMethod")?,
        }
    }
}

/// Show the tray icon for as long as SmoothKvm runs (comes back after logging out and in again).
pub fn start() {
    std::thread::spawn(|| {
        let mut told = false;
        loop {
            let _ = run_once(&mut told);
            std::thread::sleep(Duration::from_secs(10));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn properties_are_well_formed() {
        let b = all_props("hello");
        let len = u32::from_le_bytes(b[..4].try_into().unwrap()) as usize;
        assert_eq!(len, b.len() - 8, "array length covers everything after the padding");
        // first entry: key "Category" at offset 8
        assert_eq!(u32::from_le_bytes(b[8..12].try_into().unwrap()), 8);
        assert_eq!(&b[12..20], b"Category");
        let mut v = Vec::new();
        variant(&mut v, &Val::B(true));
        assert_eq!(v, vec![1, b'b', 0, 0, 1, 0, 0, 0]);
    }

    /// Run by hand inside `dbus-run-session`, with busctl asking from outside (see the release notes in memory).
    #[test]
    #[ignore]
    fn serves_on_a_real_bus() {
        std::thread::spawn(|| {
            let mut told = false;
            println!("ended: {:?}", run_once(&mut told));
        });
        std::thread::sleep(Duration::from_secs(12));
    }
}
