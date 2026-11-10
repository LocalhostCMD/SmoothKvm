//! On-demand virtual display for a headless machine (Linux).
//!
//! `kvmd display start` launches Xvfb (virtual screen) + XFCE + x11vnc, where x11vnc listens
//! on 127.0.0.1 only. The Windows side reaches it through the encrypted, paired tunnel
//! (`kvmd view`), so nothing new is exposed on the network. `kvmd display stop` removes it all.
//! Run as your normal user (no sudo needed).
use crate::store::config_dir;
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
            Some((n.to_string(), p.parse().ok()?))
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
            io::Error::new(io::ErrorKind::NotFound, format!("{name} not found; install it: sudo apt install xvfb x11vnc xfce4"))
        } else {
            e
        }
    })?;
    let pid = child.id() as i32;
    pids.push((name.to_string(), pid));
    Ok(pid)
}

fn valid_resolution(s: &str) -> bool {
    match s.split_once('x') {
        Some((w, h)) => match (w.parse::<u32>(), h.parse::<u32>()) {
            (Ok(w), Ok(h)) => (320..=7680).contains(&w) && (200..=4320).contains(&h),
            _ => false,
        },
        None => false,
    }
}

pub fn run(action: Option<&str>, arg: Option<&str>) -> io::Result<()> {
    match action {
        Some("start") => start(arg.unwrap_or("1920x1080")),
        Some("stop") => stop(),
        Some("status") => status(),
        _ => {
            eprintln!("usage: kvmd display start [WxH] | stop | status");
            Ok(())
        }
    }
}

fn start(res: &str) -> io::Result<()> {
    if !valid_resolution(res) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "resolution must look like 1920x1080"));
    }
    if read_pids().iter().any(|(_, p)| alive(*p)) {
        println!("display already running (kvmd display status)");
        return Ok(());
    }
    let mut pids = Vec::new();
    let result = (|| -> io::Result<()> {
        launch(
            "Xvfb",
            Command::new("Xvfb").args([DISPLAY, "-screen", "0", &format!("{res}x24"), "-nolisten", "tcp"]),
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
    println!("virtual display {DISPLAY} ({res}) is running; VNC on 127.0.0.1:{VNC_PORT} (local only).");
    println!("From Windows: kvmd view <this-machine-ip>:24800   then point a VNC viewer at localhost:5901");
    Ok(())
}

fn stop() -> io::Result<()> {
    let pids = read_pids();
    for (name, pid) in pids.iter().rev() {
        if alive(*pid) {
            // negative pid = the whole process group we created for it
            unsafe { libc::kill(-*pid, libc::SIGTERM) };
            println!("stopped {name}");
        }
    }
    let _ = fs::remove_file(pid_file());
    Ok(())
}

fn status() -> io::Result<()> {
    let pids = read_pids();
    if pids.is_empty() {
        println!("display: not running");
    }
    for (name, pid) in pids {
        println!("{name}: {}", if alive(pid) { "running" } else { "stopped" });
    }
    Ok(())
}
