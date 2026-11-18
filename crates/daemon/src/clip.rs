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

/// One clipboard operation at a time in this process. Each connection (this PC controlling another, and
/// another controlling this PC) syncs the clipboard on its own thread; Windows lets two threads of one
/// process have it open together, so a read could meet a write that frees the text being read (that was
/// the heap-corruption crash in 0.11-0.12.2).
#[cfg(windows)]
static CLIP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(windows)]
pub fn get() -> Option<String> {
    use windows::core::w;
    use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
    use windows::Win32::System::DataExchange::*;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    let _one = CLIP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
                // never read past the memory Windows gave us, even if the text has no terminating zero
                let cap = (GlobalSize(hg) / 2).min(MAX_TEXT);
                let mut n = 0usize;
                while n < cap && *p.add(n) != 0 {
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

/// A hidden message-only window that owns what SmoothKvm puts on the clipboard.
#[cfg(windows)]
fn owner() -> windows::Win32::Foundation::HWND {
    use windows::core::w;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE};
    static OWNER: std::sync::OnceLock<isize> = std::sync::OnceLock::new();
    let h = *OWNER.get_or_init(|| unsafe {
        CreateWindowExW(WINDOW_EX_STYLE(0), w!("STATIC"), w!("SmoothKvm clipboard"), WINDOW_STYLE(0), 0, 0, 0, 0, HWND_MESSAGE, None, None, None).map(|h| h.0 as isize).unwrap_or(0)
    });
    HWND(h as *mut _)
}

#[cfg(windows)]
pub fn set(text: &str) -> bool {
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::*;
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    if text.len() > MAX_TEXT {
        return false;
    }
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let _one = CLIP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        for _ in 0..10 {
            if OpenClipboard(owner()).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        if !opened {
            let _ = GlobalFree(hg);
            return false;
        }
        // EmptyClipboard makes the window passed to OpenClipboard the owner; with no window the owner is
        // nobody and SetClipboardData may fail, so it was opened with our own (hidden) window above.
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

    fn run_dir() -> String {
        let uid = unsafe { libc::getuid() };
        std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{uid}"))
    }

    /// The Wayland desktop's socket, if the logged-in session is a Wayland one (KDE Plasma, GNOME...).
    fn wayland_display() -> Option<String> {
        let run = run_dir();
        std::env::var("WAYLAND_DISPLAY")
            .ok()
            .filter(|d| !d.is_empty())
            .or_else(|| (0..3).map(|i| format!("wayland-{i}")).find(|n| std::path::Path::new(&run).join(n).exists()))
    }

    /// Environment that lets a background service reach the logged-in user's clipboard.
    fn session_env(cmd: &mut Command, wayland: Option<&str>) {
        cmd.env("XDG_RUNTIME_DIR", run_dir());
        if let Some(d) = wayland {
            cmd.env("WAYLAND_DISPLAY", d);
        } else if let Some((d, auth)) = crate::x11::find_display() {
            cmd.env("DISPLAY", d);
            if let Some(a) = auth {
                cmd.env("XAUTHORITY", a);
            }
        } else {
            cmd.env("DISPLAY", ":0");
        }
    }

    fn candidates(read: bool) -> (Vec<Command>, bool) {
        let mut v = Vec::new();
        let wl = wayland_display();
        let mk = |p: &str, a: &[&str]| {
            let mut c = Command::new(p);
            c.args(a);
            session_env(&mut c, wl.as_deref());
            c
        };
        if wl.is_some() {
            // On Wayland only wl-clipboard sees the real clipboard. The X11 one (xclip/xsel) is only kept in
            // step while an X11 app has focus, so it hands back stale text and swallows what is written to it.
            // "text" = whichever plain-text type the app offers (text/plain;charset=utf-8, UTF8_STRING, ...)
            v.push(if read { mk("wl-paste", &["--no-newline", "--type", "text"]) } else { mk("wl-copy", &[]) });
        } else if read {
            v.push(mk("xclip", &["-selection", "clipboard", "-o"]));
            v.push(mk("xsel", &["--clipboard", "--output"]));
        } else {
            v.push(mk("xclip", &["-selection", "clipboard", "-i"]));
            v.push(mk("xsel", &["--clipboard", "--input"]));
        }
        (v, wl.is_some())
    }

    /// Say once (not on every poll) when no clipboard tool could be started at all.
    fn missing_tool(wayland: bool) {
        static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
            crate::shared::warn(if wayland {
                "Copy and paste between computers needs wl-clipboard: sudo apt install wl-clipboard"
            } else {
                "Copy and paste between computers needs xclip: sudo apt install xclip"
            });
        }
    }

    pub fn get() -> Option<String> {
        let (cmds, wayland) = candidates(true);
        let mut started = false;
        for mut c in cmds {
            let Ok(mut child) = c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else { continue };
            started = true;
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
        if !started {
            missing_tool(wayland);
        }
        None
    }

    pub fn set(text: &str) -> bool {
        let (cmds, wayland) = candidates(false);
        let mut started = false;
        for mut c in cmds {
            let Ok(mut child) = c.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else { continue };
            started = true;
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
        if !started {
            missing_tool(wayland);
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

/// Mirrors the "share clipboard" setting (config.clipboard); on by default.
pub static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// One per connection. Something is sent only when THIS computer's clipboard really changed (a new copy here),
/// never merely because it differs from what arrived: if writing the clipboard ever fails, old text here is not
/// bounced back over what you just copied on the other computer.
/// This computer's clipboard text for `ClipSync::outgoing` (nothing when sharing is off). Slow, so it is never
/// called while holding a lock that the input path also takes.
pub fn read() -> Option<String> {
    if enabled() {
        get()
    } else {
        None
    }
}

pub struct ClipSync {
    /// What this computer's clipboard held when it was last looked at.
    seen: String,
    /// What came from the other computer last (so it is not sent straight back).
    received: String,
    primed: bool,
}

impl ClipSync {
    /// Starts from the current clipboard: only things copied AFTER connecting are shared.
    pub fn new() -> Self {
        let on = enabled();
        Self { seen: if on { get().unwrap_or_default() } else { String::new() }, received: String::new(), primed: on }
    }

    /// What to send, given what the clipboard here holds now (`None` = unreadable or not text).
    fn step(&mut self, now: Option<String>) -> Option<String> {
        let t = now?;
        if t == self.seen {
            return None; // nothing new copied here
        }
        self.seen = t.clone();
        if t == self.received {
            return None; // it is what the other computer just gave us
        }
        Some(t)
    }

    /// New local text to send, if something new was copied here since last time.
    /// `now` is what `read()` returned: read the clipboard first, with no lock held (that can take a while:
    /// Windows may be busy with it, Linux starts wl-paste), then hand it over here.
    pub fn outgoing(&mut self, now: Option<String>) -> Option<String> {
        if !enabled() {
            return None;
        }
        if !self.primed {
            self.seen = now.unwrap_or_default();
            self.primed = true;
            return None;
        }
        let t = self.step(now)?;
        crate::shared::info(&format!("Clipboard sent ({} characters)", t.chars().count()));
        Some(t)
    }

    /// Text from the other computer (only applied when the setting is on).
    pub fn incoming(&mut self, t: String) {
        if !enabled() || t.is_empty() || t.len() > MAX_TEXT || t == self.received {
            return;
        }
        self.received = t.clone();
        if set(&t) {
            crate::shared::info(&format!("Clipboard received ({} characters)", t.chars().count()));
        } else {
            crate::shared::warn("Could not put the copied text on this computer's clipboard");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sync(seen: &str) -> ClipSync {
        ClipSync { seen: seen.into(), received: String::new(), primed: true }
    }

    #[test]
    fn sends_only_new_local_copies() {
        let mut c = sync("old");
        assert_eq!(c.step(Some("old".into())), None);
        assert_eq!(c.step(Some("hello".into())), Some("hello".into()));
        assert_eq!(c.step(Some("hello".into())), None, "sent once");
        assert_eq!(c.step(None), None, "an unreadable clipboard sends nothing");
    }

    #[test]
    fn received_text_is_not_sent_back() {
        let mut c = sync("old");
        c.received = "from laptop".into(); // arrived and was written here
        assert_eq!(c.step(Some("from laptop".into())), None);
    }

    /// The bug seen on KDE: writing the clipboard silently did nothing and reading kept returning old text.
    /// That old text must never be bounced back over what was just copied on the other computer.
    #[test]
    fn failed_write_does_not_echo_stale_text() {
        let mut c = sync("stale 9ch");
        c.received = "copied on laptop".into();
        for _ in 0..5 {
            assert_eq!(c.step(Some("stale 9ch".into())), None);
        }
        assert_eq!(c.step(Some("new here".into())), Some("new here".into()), "a real new copy is still sent");
    }
}
