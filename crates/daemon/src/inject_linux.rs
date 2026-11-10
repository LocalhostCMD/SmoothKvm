//! Linux input injection via /dev/uinput (works on X11 and Wayland).
//! Needs write access to /dev/uinput (run as root, or see README for a udev rule).
use kvm_protocol::{InputEvent, MouseButton};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::{Mutex, OnceLock};

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const BTN_LEFT: u16 = 0x110;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;

const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_SETUP: libc::c_ulong = 0x405C_5503;
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;

#[repr(C)]
struct UinputSetup {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct RawEvent {
    time: libc::timeval,
    kind: u16,
    code: u16,
    value: i32,
}

pub struct Uinput {
    file: File,
}

fn ioctl(fd: i32, req: libc::c_ulong, arg: libc::c_ulong) -> io::Result<()> {
    if unsafe { libc::ioctl(fd, req as _, arg) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl Uinput {
    pub fn new() -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/uinput")?;
        let fd = file.as_raw_fd();
        for ev in [EV_KEY, EV_REL, EV_SYN] {
            ioctl(fd, UI_SET_EVBIT, ev as _)?;
        }
        for key in 1..=255u16 {
            ioctl(fd, UI_SET_KEYBIT, key as _)?;
        }
        for btn in BTN_LEFT..=BTN_EXTRA {
            ioctl(fd, UI_SET_KEYBIT, btn as _)?;
        }
        for rel in [REL_X, REL_Y, REL_HWHEEL, REL_WHEEL] {
            ioctl(fd, UI_SET_RELBIT, rel as _)?;
        }
        let mut name = [0u8; 80];
        let label = b"softwarekvm virtual input";
        name[..label.len()].copy_from_slice(label);
        let setup = UinputSetup { bustype: 0x06, vendor: 0x1234, product: 0x5678, version: 1, name, ff_effects_max: 0 };
        ioctl(fd, UI_DEV_SETUP, &setup as *const _ as libc::c_ulong)?;
        ioctl(fd, UI_DEV_CREATE, 0)?;
        // Give the desktop a moment to notice the new device.
        std::thread::sleep(std::time::Duration::from_millis(300));
        Ok(Self { file })
    }

    fn emit(&mut self, kind: u16, code: u16, value: i32) {
        let ev = RawEvent { time: libc::timeval { tv_sec: 0, tv_usec: 0 }, kind, code, value };
        let bytes = unsafe {
            std::slice::from_raw_parts(&ev as *const RawEvent as *const u8, std::mem::size_of::<RawEvent>())
        };
        let _ = self.file.write_all(bytes);
    }

    fn sync(&mut self) {
        self.emit(EV_SYN, 0, 0);
    }

    pub fn inject(&mut self, ev: &InputEvent) {
        match *ev {
            InputEvent::MouseMoveRel { dx, dy } => {
                if dx != 0 {
                    self.emit(EV_REL, REL_X, dx);
                }
                if dy != 0 {
                    self.emit(EV_REL, REL_Y, dy);
                }
            }
            // Absolute positioning isn't supported by this virtual relative mouse.
            InputEvent::MouseMoveAbs { .. } => return,
            InputEvent::MouseButton { button, pressed } => {
                let code = match button {
                    MouseButton::Left => BTN_LEFT,
                    MouseButton::Right => BTN_LEFT + 1,
                    MouseButton::Middle => BTN_LEFT + 2,
                    MouseButton::Back => BTN_SIDE,
                    MouseButton::Forward => BTN_EXTRA,
                };
                self.emit(EV_KEY, code, pressed as i32);
            }
            InputEvent::Scroll { dx, dy } => {
                if dy != 0 {
                    self.emit(EV_REL, REL_WHEEL, dy);
                }
                if dx != 0 {
                    self.emit(EV_REL, REL_HWHEEL, dx);
                }
            }
            InputEvent::Key { code, pressed } => match hid_to_linux(code) {
                Some(k) => self.emit(EV_KEY, k, pressed as i32),
                None => return,
            },
        }
        self.sync();
    }
}

/// USB HID usage -> Linux KEY_* code
fn hid_to_linux(code: u16) -> Option<u16> {
    const LETTERS: [u16; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44,
    ];
    Some(match code {
        0x04..=0x1D => LETTERS[(code - 0x04) as usize],
        0x1E..=0x27 => 2 + (code - 0x1E), // 1..9, 0
        0x28 => 28,  // Enter
        0x29 => 1,   // Esc
        0x2A => 14,  // Backspace
        0x2B => 15,  // Tab
        0x2C => 57,  // Space
        0x2D => 12,  // -
        0x2E => 13,  // =
        0x2F => 26,  // [
        0x30 => 27,  // ]
        0x31 => 43,  // backslash
        0x33 => 39,  // ;
        0x34 => 40,  // '
        0x35 => 41,  // `
        0x36 => 51,  // ,
        0x37 => 52,  // .
        0x38 => 53,  // /
        0x39 => 58,  // CapsLock
        0x3A..=0x43 => 59 + (code - 0x3A), // F1-F10
        0x44 => 87,  // F11
        0x45 => 88,  // F12
        0x47 => 70,  // ScrollLock
        0x49 => 110, // Insert
        0x4A => 102, // Home
        0x4B => 104, // PageUp
        0x4C => 111, // Delete
        0x4D => 107, // End
        0x4E => 109, // PageDown
        0x4F => 106, // Right
        0x50 => 105, // Left
        0x51 => 108, // Down
        0x52 => 103, // Up
        0xE0 => 29,  // LCtrl
        0xE1 => 42,  // LShift
        0xE2 => 56,  // LAlt
        0xE3 => 125, // LMeta
        0xE4 => 97,  // RCtrl
        0xE5 => 54,  // RShift
        0xE6 => 100, // RAlt
        _ => return None,
    })
}

static DEVICE: OnceLock<Mutex<Uinput>> = OnceLock::new();

pub fn inject(ev: &InputEvent) {
    if DEVICE.get().is_none() {
        match Uinput::new() {
            Ok(d) => {
                let _ = DEVICE.set(Mutex::new(d));
            }
            Err(e) => {
                // Say it once, not for every key event.
                static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    eprintln!("cannot open /dev/uinput: {e} (the SmoothKvm package sets this up; otherwise run as root)");
                }
                return;
            }
        }
    }
    if let Some(dev) = DEVICE.get() {
        if let Ok(mut d) = dev.lock() {
            d.inject(ev);
        }
    }
}
