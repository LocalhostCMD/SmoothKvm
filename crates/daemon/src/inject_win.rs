//! Windows input injection via SendInput.
use kvm_protocol::{InputEvent, MouseButton};
use std::mem::size_of;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;

fn send(input: INPUT) {
    unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
}

fn mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) {
    send(INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    });
}

fn key(scan: u16, extended: bool, pressed: bool) {
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    send(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    });
}

/// USB HID usage -> (set-1 scancode, extended)
fn hid_to_scan(code: u16) -> Option<(u16, bool)> {
    const LETTERS: [u16; 26] = [
        0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32,
        0x31, 0x18, 0x19, 0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C,
    ];
    Some(match code {
        0x04..=0x1D => (LETTERS[(code - 0x04) as usize], false),
        0x1E..=0x26 => (0x02 + (code - 0x1E), false),
        0x27 => (0x0B, false),
        0x28 => (0x1C, false),
        0x29 => (0x01, false),
        0x2A => (0x0E, false),
        0x2B => (0x0F, false),
        0x2C => (0x39, false),
        0x2D => (0x0C, false),
        0x2E => (0x0D, false),
        0x2F => (0x1A, false),
        0x30 => (0x1B, false),
        0x31 => (0x2B, false),
        0x33 => (0x27, false),
        0x34 => (0x28, false),
        0x35 => (0x29, false),
        0x36 => (0x33, false),
        0x37 => (0x34, false),
        0x38 => (0x35, false),
        0x4F => (0x4D, true),
        0x50 => (0x4B, true),
        0x51 => (0x50, true),
        0x52 => (0x48, true),
        0xE0 => (0x1D, false),
        0xE1 => (0x2A, false),
        0xE2 => (0x38, false),
        0xE3 => (0x5B, true),
        0xE4 => (0x1D, true),
        0xE5 => (0x36, false),
        0xE6 => (0x38, true),
        _ => return None,
    })
}

pub fn inject(ev: &InputEvent) {
    match *ev {
        InputEvent::MouseMoveRel { dx, dy } => mouse(MOUSEEVENTF_MOVE, dx, dy, 0),
        InputEvent::MouseMoveAbs { x, y } => unsafe {
            let _ = SetCursorPos(x, y);
        },
        InputEvent::MouseButton { button, pressed } => {
            let (flags, data) = match (button, pressed) {
                (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, 1),
                (MouseButton::Back, false) => (MOUSEEVENTF_XUP, 1),
                (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, 2),
                (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, 2),
            };
            mouse(flags, 0, 0, data);
        }
        InputEvent::Scroll { dx, dy } => {
            if dy != 0 {
                mouse(MOUSEEVENTF_WHEEL, 0, 0, dy * 120);
            }
            if dx != 0 {
                mouse(MOUSEEVENTF_HWHEEL, 0, 0, dx * 120);
            }
        }
        InputEvent::Key { code, pressed } => {
            if let Some((scan, ext)) = hid_to_scan(code) {
                key(scan, ext, pressed);
            }
        }
    }
}
