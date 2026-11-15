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
    let mut s = TcpStream::connect_timeout(&([127, 0, 0, 1], crate::httpd::port()).into(), Duration::from_millis(600))?;
    s.set_read_timeout(Some(Duration::from_secs(3)))?;
    let tok = token.map(|t| format!("X-Token: {t}\r\n")).unwrap_or_default();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{tok}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        crate::httpd::port(),
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
    Ok(format!("http://127.0.0.1:{}/#{}", crate::httpd::port(), store::panel_token()?))
}

/// Start the background app (no console window) if it isn't already running.
pub fn start(show: bool) -> io::Result<bool> {
    if running() {
        return Ok(false);
    }
    // "Work with admin apps" is on: start through the admin task so it runs elevated (no UAC prompt).
    if !is_elevated() && admin_task_exists() && run_admin_task(show) {
        return Ok(true);
    }
    let exe = std::env::current_exe()?;
    let log = std::fs::File::create(store::config_dir().join("smoothkvm.log")).or_else(|_| {
        std::fs::create_dir_all(store::config_dir())?;
        std::fs::File::create(store::config_dir().join("smoothkvm.log"))
    })?;
    // Started at sign-in: just the tray icon. Started by you: its window opens too.
    let args: &[&str] = if show { &["daemon", "--show"] } else { &["daemon"] };
    Command::new(exe)
        .args(args)
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

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

pub fn autostart_enabled() -> bool {
    Command::new("reg")
        .args(["query", RUN_KEY, "/v", "SmoothKvm"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Open the SmoothKvm window when you sign in to Windows (it is never started hidden).
pub fn set_autostart(on: bool) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut c = Command::new("reg");
    if on {
        c.args(["add", RUN_KEY, "/v", "SmoothKvm", "/t", "REG_SZ", "/d", &format!("\"{}\" start", exe.display()), "/f"]);
    } else {
        c.args(["delete", RUN_KEY, "/v", "SmoothKvm", "/f"]);
    }
    let ok = c.creation_flags(CREATE_NO_WINDOW).output()?.status.success();
    if ok || !on {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::Other, "could not change the sign-in setting"))
    }
}

// ---------------------------------------------------------------- "Work with admin apps"
//
// Windows does not let a normal program send keys/mouse to, or see input going to, an app that runs as
// administrator (Task Manager, an admin terminal, installers...). So SmoothKvm can run elevated: a scheduled
// task (created once, with one UAC prompt) starts it with the highest rights at no further prompt. Because
// that task runs the program from the install folder, the folder is locked so only administrators can change
// it; otherwise any program could swap SmoothKvm.exe and be started as administrator.

const ADMIN_TASK: &str = "SmoothKvm admin";

/// This process runs with an elevated (administrator) token.
pub fn is_elevated() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut tok = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok).is_err() {
            return false;
        }
        let mut e = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(tok, TokenElevation, Some(&mut e as *mut _ as *mut _), size_of::<TOKEN_ELEVATION>() as u32, &mut len).is_ok();
        let _ = CloseHandle(tok);
        ok && e.TokenIsElevated != 0
    }
}

pub fn admin_task_exists() -> bool {
    Command::new("schtasks")
        .args(["/Query", "/TN", ADMIN_TASK])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Start SmoothKvm through the admin task and wait for it; `show` opens its window. False if that failed.
fn run_admin_task(show: bool) -> bool {
    let ok = Command::new("schtasks")
        .args(["/Run", "/TN", ADMIN_TASK])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        return false;
    }
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(200));
        if running() {
            if show {
                // its window loop starts just after the web server
                std::thread::sleep(Duration::from_millis(800));
                if let Ok(tok) = store::panel_token() {
                    let _ = http("POST", "/api/show", Some(&tok), "{}");
                }
            }
            return true;
        }
    }
    false
}

/// The installer's folder (has the uninstaller) that this program runs from.
fn installed_dir() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    dir.join("unins000.exe").exists().then_some(dir)
}

/// `SmoothKvm admin-setup on|off`: create or remove the admin task and lock or unlock the install folder.
/// Must already run elevated (the app asks Windows for that, see `set_admin_mode`).
pub fn admin_setup(on: bool) -> io::Result<()> {
    if !is_elevated() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "run this as administrator"));
    }
    let dir = installed_dir().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "only the installed app can do this"))?;
    let launcher = dir.join("SmoothKvmApp.exe");
    let script = if on {
        // The task runs only while you are signed in, in your session, with your account's full rights.
        // The folder: administrators and SYSTEM may change it, you may only read and run it. It is also
        // handed to Administrators, since an owner can always change the permissions back.
        "$ErrorActionPreference='Stop'; \
         $me=[Security.Principal.WindowsIdentity]::GetCurrent(); \
         $a=New-ScheduledTaskAction -Execute $env:SK_EXE -Argument 'start'; \
         $p=New-ScheduledTaskPrincipal -UserId $me.Name -LogonType Interactive -RunLevel Highest; \
         $s=New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew; \
         Register-ScheduledTask -TaskName $env:SK_TASK -Action $a -Principal $p -Settings $s -Force | Out-Null; \
         & icacls $env:SK_DIR /inheritance:r /grant:r '*S-1-5-32-544:(OI)(CI)F' '*S-1-5-18:(OI)(CI)F' ('*'+$me.User.Value+':(OI)(CI)RX') /C /Q | Out-Null; \
         if($LASTEXITCODE -ne 0){ exit 1 };          & icacls $env:SK_DIR /setowner '*S-1-5-32-544' /T /C /Q | Out-Null;          if($LASTEXITCODE -ne 0){ exit 1 }"
    } else {
        "Unregister-ScheduledTask -TaskName $env:SK_TASK -Confirm:$false -ErrorAction SilentlyContinue; \
         & icacls $env:SK_DIR /reset /T /C /Q | Out-Null; exit 0"
    };
    let ok = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script])
        .env("SK_DIR", &dir)
        .env("SK_EXE", &launcher)
        .env("SK_TASK", ADMIN_TASK)
        .creation_flags(CREATE_NO_WINDOW)
        .status()?
        .success();
    if ok {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::Other, "Windows refused to set it up"))
    }
}

/// Turn "Work with admin apps" on or off from the app: Windows asks you once for permission (UAC).
/// Turning it on while running normally restarts SmoothKvm elevated.
pub fn set_admin_mode(on: bool) -> io::Result<()> {
    if installed_dir().is_none() {
        return Err(io::Error::new(io::ErrorKind::Other, "this works only for SmoothKvm installed with its installer"));
    }
    let exe = std::env::current_exe()?;
    let arg = if on { "admin-setup on" } else { "admin-setup off" };
    if is_elevated() {
        admin_setup(on)?;
    } else {
        // Start-Process -Verb RunAs shows the UAC prompt; declining it fails the command.
        let ok = Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "try { $p=Start-Process -FilePath $env:SK_EXE -ArgumentList $env:SK_ARG -Verb RunAs -WindowStyle Hidden -Wait -PassThru; exit $p.ExitCode } catch { exit 2 }",
            ])
            .env("SK_EXE", &exe)
            .env("SK_ARG", arg)
            .creation_flags(CREATE_NO_WINDOW)
            .status()?;
        match ok.code() {
            Some(0) => {}
            Some(2) => return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Windows did not give permission")),
            _ => return Err(io::Error::new(io::ErrorKind::Other, "Windows refused to set it up")),
        }
    }
    if on && !is_elevated() {
        // restart through the task once this copy has quit
        Command::new(&exe).arg("relaunch").creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS).spawn()?;
    }
    Ok(())
}

/// `SmoothKvm relaunch`: wait for the running copy to quit, then start it again (elevated if set up) with its window.
pub fn relaunch() -> io::Result<()> {
    for _ in 0..50 {
        if !running() {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_millis(300));
    start(true).map(|_| ())
}
