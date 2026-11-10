//! On-demand virtual display for a headless machine (Linux).
//!
//! Start launches Xvfb (virtual screen) + XFCE + x11vnc, where x11vnc listens on 127.0.0.1
//! only. The Windows side reaches it through the encrypted, paired tunnel, so nothing new is
//! exposed on the network. It can be controlled locally (`SmoothKvm display ...`) or remotely
//! from the SmoothKvm panel.
use crate::store::config_dir;
use kvm_protocol::DisplayAction;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const DISPLAY: &str = ":99";
const VNC_PORT: &str = "5900";

fn pid_file() -> PathBuf {
    config_dir().join("display.pids")
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn read_pids() -> Vec<(String, i32)> {
    fs::read_to_string(pid_file())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (n, p) = l.split_once(' ')?;
            let pid: i32 = p.parse().ok()?;
            // pid 0/1/negative would make kill(-pid) hit far more than our own processes
            if pid <= 1 {
                return None;
            }
            Some((n.to_string(), pid))
        })
        .collect()
}

fn save_pids(pids: &[(String, i32)]) -> io::Result<()> {
    fs::create_dir_all(config_dir())?;
    fs::write(pid_file(), pids.iter().map(|(n, p)| format!("{n} {p}\n")).collect::<String>())
}

fn launch(name: &str, cmd: &mut Command, pids: &mut Vec<(String, i32)>) -> io::Result<i32> {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0);
    let child = cmd.spawn().map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            io::Error::new(io::ErrorKind::NotFound, format!("{name} not found; install: sudo apt install xvfb x11vnc xfce4"))
        } else {
            e
        }
    })?;
    let pid = child.id() as i32;
    pids.push((name.to_string(), pid));
    Ok(pid)
}

fn valid_resolution(w: u32, h: u32) -> bool {
    (320..=7680).contains(&w) && (200..=4320).contains(&h)
}

fn is_running() -> bool {
    read_pids().iter().any(|(_, p)| alive(*p))
}

pub fn start(w: u32, h: u32) -> io::Result<String> {
    if !valid_resolution(w, h) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "resolution out of range"));
    }
    if is_running() {
        return Ok("display already running".into());
    }
    let mut pids = Vec::new();
    let result = (|| -> io::Result<()> {
        launch(
            "Xvfb",
            Command::new("Xvfb").args([DISPLAY, "-screen", "0", &format!("{w}x{h}x24"), "-nolisten", "tcp"]),
            &mut pids,
        )?;
        let sock = format!("/tmp/.X11-unix/X{}", &DISPLAY[1..]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !std::path::Path::new(&sock).exists() {
            if Instant::now() > deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "Xvfb did not start (is display :99 already in use?)"));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        launch(
            "startxfce4",
            Command::new("startxfce4")
                .env("DISPLAY", DISPLAY)
                .env("XDG_SESSION_TYPE", "x11")
                .env_remove("WAYLAND_DISPLAY")
                .env_remove("DBUS_SESSION_BUS_ADDRESS")
                .env_remove("SESSION_MANAGER"),
            &mut pids,
        )?;
        let vnc = launch(
            "x11vnc",
            Command::new("x11vnc").args([
                "-display", DISPLAY, "-localhost", "-rfbport", VNC_PORT, "-nopw", "-forever", "-shared", "-noxdamage", "-quiet",
            ]),
            &mut pids,
        )?;
        std::thread::sleep(Duration::from_millis(1200));
        if !alive(vnc) {
            return Err(io::Error::new(io::ErrorKind::Other, format!("x11vnc exited (is port {VNC_PORT} already in use?)")));
        }
        Ok(())
    })();
    save_pids(&pids)?;
    if let Err(e) = result {
        let _ = stop();
        return Err(e);
    }
    Ok(format!("virtual display {DISPLAY} ({w}x{h}) is running"))
}

pub fn stop() -> io::Result<String> {
    let pids = read_pids();
    let mut any = false;
    for (_, pid) in pids.iter().rev() {
        if alive(*pid) {
            // negative pid = the whole process group we created for it
            unsafe { libc::kill(-*pid, libc::SIGTERM) };
            any = true;
        }
    }
    let _ = fs::remove_file(pid_file());
    Ok(if any { "display stopped".into() } else { "display was not running".into() })
}

pub fn status() -> (bool, String) {
    let running = is_running();
    (running, if running { "display running".into() } else { "display not running".into() })
}

/// Handle a remote control request from a paired sender.
pub fn command(a: DisplayAction) -> (bool, String) {
    match a {
        DisplayAction::Start { width, height } => match start(width, height) {
            Ok(m) => (true, m),
            Err(e) => (false, e.to_string()),
        },
        DisplayAction::Stop => match stop() {
            Ok(m) => (false, m),
            Err(e) => (is_running(), e.to_string()),
        },
        DisplayAction::Status => status(),
    }
}

pub fn run(action: Option<&str>, arg: Option<&str>) -> io::Result<()> {
    match action {
        Some("start") => {
            let res = arg.unwrap_or("1920x1080");
            let (w, h) = res
                .split_once('x')
                .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "resolution must look like 1920x1080"))?;
            println!("{}", start(w, h)?);
        }
        Some("stop") => println!("{}", stop()?),
        Some("status") => println!("{}", status().1),
        _ => eprintln!("usage: SmoothKvm display start [WxH] | stop | status"),
    }
    Ok(())
}
