// SmoothKvmApp: the double-clickable app icon. It is a windowless program that just starts
// SmoothKvm.exe (next to it) without a console window and exits.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::Command;

fn main() {
    let exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("SmoothKvm.exe")));
    let Some(exe) = exe else { return };
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        args.push("open".into());
    }
    let mut cmd = Command::new(exe);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let _ = cmd.spawn();
}
