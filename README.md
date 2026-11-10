# SmoothKvm

Share one keyboard and mouse across several computers. Windows PC = the one with the keyboard
and mouse; the other machines (Linux receivers) are screens on its left/right/top/bottom.

## Quick start
Windows:
```
SmoothKvm install --autostart     # once: puts SmoothKvm on PATH, starts with Windows
SmoothKvm                         # opens the control panel (starts the background app)
```
Linux receiver (the machine being controlled):
```
sudo SmoothKvm listen --pair      # first time, then: sudo SmoothKvm listen
```
In the panel: click a side of "This PC" to add a screen, enter the receiver's address
(`host:24800`, e.g. its Tailscale IP), press **Pair this device**, confirm that both sides show
the same code. Everything else (animations, game mode, displays) is in the panel.

## Features
- **Several screens**: up to four, one per side of your PC. Each has its own name, address, size.
- **Native app**: its own window (no browser) plus a tray icon. Closing the window keeps it running in the
  tray and frees the window's memory. Double-click it or type `SmoothKvm`.
- **Emergency stop**: red button in the app, tray menu item, or press the escape key (Scroll Lock) **three
  times quickly**. Returns control to this PC, makes every screen release held keys/buttons, releases
  modifiers/buttons here, and pauses switching until you press Resume.
- **Effects**: Bounce / Elastic / Slide / Overshoot / Spring / **Custom bezier curve** (with live plot and
  presets), duration and distance, on arrival and/or return, applied on both your PC and the remote screen.
- **Game mode**: while a full-screen app or game is running (Windows' own detection) the cursor
  will not switch screens. Toggle in the panel; the header shows when it is active.
- **On-demand display** for headless machines (Xvfb + XFCE + x11vnc), started/stopped from the panel
  and viewed through an encrypted tunnel with a VNC viewer (TigerVNC). `sudo apt install xvfb x11vnc xfce4`.
- Escape key (default Scroll Lock) always returns control to this PC.

## Security model
- Transport: **Noise_XX_25519_ChaChaPoly_BLAKE2s** (mutual authentication, forward secrecy, ChaCha20-Poly1305).
- Every device has a long-term key (`identity.key`, user-only permissions). Only **paired** keys are accepted.
- Pairing is explicit: confirm the same 6-digit code on both devices (defeats a man-in-the-middle).
  Receiver pairing mode closes after 120 s or the first pairing.
- The receiver releases all held keys/buttons if the sender leaves or goes silent for 10 s.
- The VNC server only listens on the receiver's localhost; it is reached only through the paired tunnel.
- Config panel: loopback only, random secret token, Host check, strict CSP.

## Files
Config dir: `%APPDATA%\softwarekvm` (Windows) or `~/.config/softwarekvm` (Linux):
`config.toml`, `identity.key`, `peers.toml`, `panel.token`, `smoothkvm.log`.

## Commands
Windows: `SmoothKvm` | `start` | `stop` | `status` | `install [--autostart]` | `uninstall`
Linux: `SmoothKvm listen [--pair]` | `display start [WxH]|stop|status`
Both: `SmoothKvm view <addr>` (display tunnel on localhost:5901) | `test <addr> [--pair]`

### Run the Linux receiver without sudo
```
echo 'KERNEL=="uinput", GROUP="input", MODE="0660"' | sudo tee /etc/udev/rules.d/99-uinput.rules
sudo usermod -aG input $USER     # log out and in again
sudo udevadm control --reload && sudo udevadm trigger
```

## Layout
`crates/protocol` shared types + encrypted transport; `crates/daemon` (`SmoothKvm`): Windows capture
and panel, Linux/Windows injection, display control.

## Not yet
Clipboard sync, system-tray icon, edge-switching input into the virtual (Xvfb) display.
