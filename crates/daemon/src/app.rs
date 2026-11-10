//! Terminal-facing lifecycle commands for Windows: `SmoothKvm`, start, stop, status, install.
use crate::store;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;

fn http(method: &str, path: &str, token: Option<&str>, body: &str) -> io::Result<String> {
    let mut s = TcpStream::connect_timeout(&"127.0.0.1:24801".parse().unwrap(), Duration::from_millis(600))?;
    s.set_read_timeout(Some(Duration::from_secs(3)))?;
    let tok = token.map(|t| format!("X-Token: {t}\r\n")).unwrap_or_default();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:24801\r\n{tok}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    Ok(out)
}

pub fn running() -> bool {
    http("GET", "/ping", None, "").is_ok()
}

pub fn panel_url() -> io::Result<String> {
    Ok(format!("http://127.0.0.1:24801/#{}", store::panel_token()?))
}

/// Start the background app (no console window) if it isn't already running.
pub fn start(show: bool) -> io::Result<bool> {
    if running() {
        return Ok(false);
    }
    let exe = std::env::current_exe()?;
    let log = std::fs::File::create(store::config_dir().join("smoothkvm.log")).or_else(|_| {
        std::fs::create_dir_all(store::config_dir())?;
        std::fs::File::create(store::config_dir().join("smoothkvm.log"))
    })?;
    Command::new(exe)
        .args(if show { vec!["daemon", "--show"] } else { vec!["daemon"] })
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()?;
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(200));
        if running() {
            return Ok(true);
        }
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "SmoothKvm did not start; see smoothkvm.log in its config folder"))
}

pub fn stop() -> io::Result<()> {
    if !running() {
        println!("SmoothKvm is not running.");
        return Ok(());
    }
    let tok = store::panel_token()?;
    let _ = http("POST", "/api/quit", Some(&tok), "");
    std::thread::sleep(Duration::from_millis(500));
    println!("SmoothKvm stopped.");
    Ok(())
}

pub fn status() -> io::Result<()> {
    println!("SmoothKvm is {}", if running() { "running" } else { "not running" });
    Ok(())
}

/// `SmoothKvm`: start the app if needed and bring its window to the front.
pub fn open_panel() -> io::Result<()> {
    if !running() {
        // Starting it with --show makes the new instance open its own window.
        start(true)?;
        return Ok(());
    }
    let tok = store::panel_token()?;
    http("POST", "/api/show", Some(&tok), "{}")?;
    Ok(())
}

fn install_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("SmoothKvm")
}

fn powershell(script: &str, dir: &PathBuf) -> io::Result<()> {
    let ok = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("SK_DIR", dir)
        .creation_flags(CREATE_NO_WINDOW)
        .status()?
        .success();
    if ok {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::Other, "PowerShell step failed"))
    }
}

/// Copy the program to %LOCALAPPDATA%\SmoothKvm, put it on PATH, optionally start with Windows.
pub fn install(autostart: bool) -> io::Result<()> {
    let dir = install_dir();
    let target = dir.join("SmoothKvm.exe");
    let _ = stop();
    std::fs::create_dir_all(&dir)?;
    let me = std::env::current_exe()?;
    if me != target {
        std::fs::copy(&me, &target)?;
    }
    powershell(
        "$p=[Environment]::GetEnvironmentVariable('Path','User'); if(($p -split ';') -notcontains $env:SK_DIR){[Environment]::SetEnvironmentVariable('Path',($p.TrimEnd(';')+';'+$env:SK_DIR),'User')}",
        &dir,
    )?;
    if autostart {
        let ok = Command::new("reg")
            .args([
                "add",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "SmoothKvm",
                "/t",
                "REG_SZ",
                "/d",
                &format!("\"{}\" start", target.display()),
                "/f",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .status()?
            .success();
        if !ok {
            return Err(io::Error::new(io::ErrorKind::Other, "could not set autostart"));
        }
    }
    println!("Installed to {}", dir.display());
    println!("Open a NEW terminal and type:  SmoothKvm");
    if autostart {
        println!("SmoothKvm will start automatically when you sign in.");
    }
    Ok(())
}

pub fn uninstall() -> io::Result<()> {
    let dir = install_dir();
    let _ = stop();
    let _ = Command::new("reg")
        .args(["delete", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run", "/v", "SmoothKvm", "/f"])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    powershell(
        "$p=[Environment]::GetEnvironmentVariable('Path','User'); [Environment]::SetEnvironmentVariable('Path',(($p -split ';' | Where-Object { $_ -and $_ -ne $env:SK_DIR }) -join ';'),'User')",
        &dir,
    )?;
    println!("Removed autostart and PATH entry. You can delete {} and {}.", dir.display(), store::config_dir().display());
    Ok(())
}
