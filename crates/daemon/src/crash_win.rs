//! Crash notes (Windows). If SmoothKvm is about to be killed by a fatal error (memory corruption, a bad
//! pointer), write the error code and the call stack as raw addresses to `crash.log` in the settings folder,
//! so the cause can be found with the build's debug symbols. The handler uses no heap memory (the heap may
//! be what broke): a stack buffer and a file opened at startup.
use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;
use windows::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, EXCEPTION_ILLEGAL_INSTRUCTION, EXCEPTION_STACK_OVERFLOW, NTSTATUS};
use windows::Win32::System::Diagnostics::Debug::{AddVectoredExceptionHandler, RtlCaptureStackBackTrace, EXCEPTION_POINTERS};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;

static LOG: OnceLock<File> = OnceLock::new();
static WRITTEN: AtomicU32 = AtomicU32::new(0);

const HEAP_CORRUPTION: i32 = 0xC0000374u32 as i32;
const STACK_BUFFER_OVERRUN: i32 = 0xC0000409u32 as i32;

pub fn install() {
    let path = crate::store::config_dir().join("crash.log");
    let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(path) else { return };
    if LOG.set(f).is_ok() {
        unsafe {
            AddVectoredExceptionHandler(1, Some(handler));
        }
    }
}

struct Buf {
    b: [u8; 4096],
    n: usize,
}

impl Buf {
    fn s(&mut self, t: &str) {
        for &c in t.as_bytes() {
            if self.n < self.b.len() {
                self.b[self.n] = c;
                self.n += 1;
            }
        }
    }
    fn hex(&mut self, v: u64) {
        self.s("0x");
        let mut started = false;
        for i in (0..16).rev() {
            let d = ((v >> (i * 4)) & 0xF) as u8;
            if d != 0 || started || i == 0 {
                started = true;
                if self.n < self.b.len() {
                    self.b[self.n] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
                    self.n += 1;
                }
            }
        }
    }
}

unsafe extern "system" fn handler(info: *mut EXCEPTION_POINTERS) -> i32 {
    const CONTINUE_SEARCH: i32 = 0;
    let Some(rec) = info.as_ref().and_then(|i| i.ExceptionRecord.as_ref()) else { return CONTINUE_SEARCH };
    let code = rec.ExceptionCode;
    let fatal = [EXCEPTION_ACCESS_VIOLATION, EXCEPTION_ILLEGAL_INSTRUCTION, EXCEPTION_STACK_OVERFLOW, NTSTATUS(HEAP_CORRUPTION), NTSTATUS(STACK_BUFFER_OVERRUN)];
    if !fatal.contains(&code) || WRITTEN.fetch_add(1, Ordering::SeqCst) >= 8 {
        return CONTINUE_SEARCH;
    }
    let Some(mut f) = LOG.get() else { return CONTINUE_SEARCH };
    let mut b = Buf { b: [0; 4096], n: 0 };
    let base = GetModuleHandleW(None).map(|m| m.0 as u64).unwrap_or(0);
    b.s("crash ");
    b.s(env!("CARGO_PKG_VERSION"));
    b.s(" t=");
    b.hex(crate::shared::now_ms());
    b.s(" code=");
    b.hex(code.0 as u32 as u64);
    b.s(" at=");
    b.hex(rec.ExceptionAddress as u64);
    b.s(" base=");
    b.hex(base);
    b.s("\n frames:");
    let mut frames = [std::ptr::null_mut(); 62];
    let n = RtlCaptureStackBackTrace(0, &mut frames, None) as usize;
    for p in &frames[..n.min(62)] {
        b.s(" ");
        b.hex(*p as u64);
    }
    b.s("\n");
    let _ = f.write_all(&b.b[..b.n]);
    let _ = f.flush();
    CONTINUE_SEARCH
}
