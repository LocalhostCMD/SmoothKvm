# SmoothKvm

One keyboard and mouse across several computers. Your Windows PC controls other machines
(Ubuntu today) as extra screens on its left/right/top/bottom. Everything between machines is
encrypted and only paired devices are accepted.

## Install
**Windows**: run `SmoothKvm-Setup-<version>.exe`. It adds a Start Menu icon (optional desktop icon,
start-with-Windows, and `SmoothKvm` in your terminal). No admin rights needed.

**Ubuntu / Debian**: install `smoothkvm_<version>_amd64.deb` (double-click it, or
`sudo apt install ./smoothkvm_<version>_amd64.deb`). It installs a service that starts on boot, sets up
the virtual keyboard/mouse permission, and adds the `SmoothKvm` / `smoothkvm` commands and a menu entry.

## Use
- **Windows app**: open *SmoothKvm* from the Start Menu. Click a side of "This PC" to add a screen, enter
  its address (`host:24800`, e.g. a Tailscale IP), press **Pair this device**.
- **Terminal UI (both)**: type `SmoothKvm`, then `/help`. On the Ubuntu screen type `/pair` first, then on
  Windows `/pair <name>`; confirm the same code on both.
- **Emergency stop**: red button, tray menu, `/stop`, or press **Scroll Lock three times**. It returns control,
  makes every screen release held keys/buttons, and pauses switching until you resume.

## Features
Up to four screens (one per side) · effects: Bounce/Elastic/Slide/Overshoot/Spring/None or your own cubic-bezier
curve (duration, distance, on arrival and/or return) · game mode: no switching while a full-screen app/game runs ·
on-demand virtual display for headless machines (Xvfb + XFCE + x11vnc through the encrypted tunnel, viewed with
TigerVNC) · tray icon, window freed when closed.

## Security model
Noise_XX_25519_ChaChaPoly_BLAKE2s (mutual authentication, forward secrecy). Each device has a long-term key;
only **paired** keys are accepted, pairing needs the same 6-digit code confirmed on both devices. Receivers release
held keys if the sender goes silent for 10 s. The VNC server only listens on the receiver's localhost. The local
control API listens on 127.0.0.1 only, with a secret token and a Host check.

## Build
`cargo build --release` (Windows: `SmoothKvm.exe`, `SmoothKvmApp.exe`; Linux: `SmoothKvm`).
Windows installer: `packaging/windows/SmoothKvm.iss` (Inno Setup 6). Ubuntu package:
`packaging/linux/build-deb.sh` (or `wsl-build.sh` from Windows).

## Not yet
Linux as the *controlling* machine, Windows as a *controlled* screen, clipboard sync, overlay visual effects,
code signing (unsigned programs can trigger antivirus warnings, see below).
