//! Text clipboard access (Windows: Win32; Linux: wl-clipboard / xclip / xsel when installed).
//! Only plain text is ever shared, capped in size. Contents are never written to the log.
#[cfg(windows)]
use std::time::Duration;

/// Largest clipboard text that is shared (fits in one encrypted frame with room to spare).
pub const MAX_TEXT: usize = 32 * 1024;

fn limit(s: String) -> Option<String> {
    if s.is_empty() || s.len() > MAX_TEXT {
        None
    } else {
        Some(s)
    }
}

#[cfg(windows)]
pub fn get() -> Option<String> {
    use windows::core::w;
    use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
    use windows::Win32::System::DataExchange::*;
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    unsafe {
        // Password managers mark what they copy as "do not monitor"; respect that and never share it.
        let secret = RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing"));
        if secret != 0 && IsClipboardFormatAvailable(secret).is_ok() {
            return None;
        }
        if IsClipboardFormatAvailable(13).is_err() {
            return None;
        }
        // Another program may hold the clipboard for a moment.
        let mut opened = false;
        for _ in 0..5 {
            if OpenClipboard(HWND::default()).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        if !opened {
            return None;
        }
        let mut out = None;
        if let Ok(h) = GetClipboardData(13) {
            let hg = HGLOBAL(h.0 as *mut _);
            let p = GlobalLock(hg) as *const u16;
            if !p.is_null() {
                let mut n = 0usize;
                while n < MAX_TEXT && *p.add(n) != 0 {
                    n += 1;
                }
                out = Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, n)));
                let _ = GlobalUnlock(hg);
            }
        }
        let _ = CloseClipboard();
        let _ = HANDLE::default();
        out.and_then(limit)
    }
}

#[cfg(windows)]
pub fn set(text: &str) -> bool {
    use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND};
    use windows::Win32::System::DataExchange::*;
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    if text.len() > MAX_TEXT {
        return false;
    }
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let Ok(hg) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) else { return false };
        let p = GlobalLock(hg) as *mut u16;
        if p.is_null() {
            let _ = GlobalFree(hg);
            return false;
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
        let _ = GlobalUnlock(hg);
        let mut opened = false;
        for _ in 0..5 {
            if OpenClipboard(HWND::default()).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        if !opened {
            let _ = GlobalFree(hg);
            return false;
        }
        let _ = EmptyClipboard();
        let ok = SetClipboardData(13, HANDLE(hg.0)).is_ok();
        if !ok {
            let _ = GlobalFree(hg);
        }
        let _ = CloseClipboard();
        ok
    }
}

#[cfg(unix)]
mod lin {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// Environment that lets a background service reach the logged-in user's clipboard.
    fn session_env(cmd: &mut Command, wayland: bool) {
        let uid = unsafe { libc::getuid() };
        let run = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{uid}"));
        cmd.env("XDG_RUNTIME_DIR", &run);
        if wayland {
            let disp = std::env::var("WAYLAND_DISPLAY").ok().or_else(|| {
                (0..3).map(|i| format!("wayland-{i}")).find(|n| std::path::Path::new(&run).join(n).exists())
            });
            if let Some(d) = disp {
                cmd.env("WAYLAND_DISPLAY", d);
            }
        } else {
            cmd.env("DISPLAY", std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into()));
        }
    }

    fn candidates(read: bool) -> Vec<(Command, bool)> {
        let mut v = Vec::new();
        let mk = |p: &str, a: &[&str], wl: bool| {
            let mut c = Command::new(p);
            c.args(a);
            session_env(&mut c, wl);
            (c, wl)
        };
        if read {
            v.push(mk("wl-paste", &["--no-newline", "--type", "text/plain"], true));
            v.push(mk("xclip", &["-selection", "clipboard", "-o"], false));
            v.push(mk("xsel", &["--clipboard", "--output"], false));
        } else {
            v.push(mk("wl-copy", &[], true));
            v.push(mk("xclip", &["-selection", "clipboard", "-i"], false));
            v.push(mk("xsel", &["--clipboard", "--input"], false));
        }
        v
    }

    pub fn get() -> Option<String> {
        for (mut c, _) in candidates(true) {
            let Ok(mut child) = c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else { continue };
            let start = Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(st)) => {
                        let mut s = String::new();
                        if st.success() {
                            if let Some(o) = child.stdout.take() {
                                let _ = o.take(super::MAX_TEXT as u64 + 1).read_to_string(&mut s);
                            }
                            return super::limit(s);
                        }
                        break;
                    }
                    Ok(None) if start.elapsed() < Duration::from_millis(1500) => std::thread::sleep(Duration::from_millis(10)),
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        None
    }

    pub fn set(text: &str) -> bool {
        for (mut c, _) in candidates(false) {
            let Ok(mut child) = c.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else { continue };
            if let Some(mut i) = child.stdin.take() {
                if i.write_all(text.as_bytes()).is_err() {
                    continue;
                }
            }
            // wl-copy / xclip stay alive to serve the selection; reap them in the background.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return true;
        }
        false
    }
}

#[cfg(unix)]
pub fn get() -> Option<String> {
    lin::get()
}

#[cfg(unix)]
pub fn set(text: &str) -> bool {
    text.len() <= MAX_TEXT && lin::set(text)
}

// ---- sync logic shared by controller and receiver sessions ----

use std::sync::atomic::{AtomicBool, Ordering};

/// Mirrors the "share clipboard" setting (config.clipboard); off by default.
pub static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// One per connection. Remembers what this connection last saw so a copy is sent once and never echoed back.
pub struct ClipSync {
    last: String,
    primed: bool,
}

impl ClipSync {
    /// Starts from the current clipboard: only things copied AFTER connecting are shared.
    pub fn new() -> Self {
        Self { last: if enabled() { get().unwrap_or_default() } else { String::new() }, primed: enabled() }
    }

    /// New local text to send, if the clipboard changed since last time.
    pub fn outgoing(&mut self) -> Option<String> {
        if !enabled() {
            return None;
        }
        if !self.primed {
            self.last = get().unwrap_or_default();
            self.primed = true;
            return None;
        }
        let t = get()?;
        if t == self.last {
            return None;
        }
        self.last = t.clone();
        crate::shared::info(&format!("Clipboard sent ({} characters)", t.chars().count()));
        Some(t)
    }

    /// Text from the other computer (only applied when the setting is on).
    pub fn incoming(&mut self, t: String) {
        if !enabled() || t.is_empty() || t.len() > MAX_TEXT || t == self.last {
            return;
        }
        if set(&t) {
            crate::shared::info(&format!("Clipboard received ({} characters)", t.chars().count()));
        }
        self.last = t;
    }
}
