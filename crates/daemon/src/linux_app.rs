//! The Linux desktop app: the same app page as on Windows, in its own window (a Chromium-family browser's
//! app window if one is installed, otherwise a normal browser window). No GTK/Qt libraries are needed.
//!
//! If the SmoothKvm service is running, this just opens the window. If not, SmoothKvm runs inside this
//! process while the window is open, and stops when the window is closed.
use crate::{ctl, store};
use std::io;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static LAST_SEEN: AtomicU64 = AtomicU64::new(0);
static BYE_AT: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// The app page is still open (it polls about every 1.5 s).
pub fn touch() {
    LAST_SEEN.store(now_ms(), Ordering::SeqCst);
    BYE_AT.store(0, Ordering::SeqCst);
}

/// The page is being closed (or reloaded: a reload touches again right away, which cancels this).
pub fn bye() {
    BYE_AT.store(now_ms(), Ordering::SeqCst);
}

pub fn url() -> io::Result<String> {
    Ok(format!("http://127.0.0.1:{}/#{}", crate::httpd::port(), store::panel_token()?))
}

/// Open the app page in its own window.
pub fn open_window() -> io::Result<()> {
    let url = url()?;
    let app_arg = format!("--app={url}");
    for b in ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "brave-browser", "microsoft-edge", "vivaldi"] {
        let ok = Command::new(b)
            .args([app_arg.as_str(), "--new-window", "--window-size=1000,860", "--class=SmoothKvm"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok();
        if ok {
            return Ok(());
        }
    }
    for opener in ["xdg-open", "firefox", "sensible-browser"] {
        if Command::new(opener).arg(&url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().is_ok() {
            return Ok(());
        }
    }
    Err(io::Error::new(io::ErrorKind::NotFound, format!("no web browser found; open {} yourself", url.split('#').next().unwrap_or(""))))
}

/// `SmoothKvm app`
pub fn run() -> io::Result<()> {
    if ctl::alive() {
        println!("Opening SmoothKvm...");
        return open_window();
    }
    crate::ensure_input_group();
    crate::start_embedded()?;
    open_window()?;
    eprintln!("SmoothKvm is running inside this app. It stops when you close its window.");
    // Wait for the page to show up, then for it to go away. Background tabs may poll only once a minute,
    // so "gone" means: closed (it says goodbye) or silent for several minutes.
    let start = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let (last, bye) = (LAST_SEEN.load(Ordering::SeqCst), BYE_AT.load(Ordering::SeqCst));
        if last == 0 {
            if start.elapsed() > Duration::from_secs(90) {
                break;
            }
            continue;
        }
        if bye != 0 && now_ms().saturating_sub(bye) > 4000 {
            break;
        }
        if now_ms().saturating_sub(last) > 180_000 {
            break;
        }
    }
    crate::stop_embedded();
    Ok(())
}
