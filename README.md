# Softwarekvm

Share one keyboard and mouse between a Windows PC (sender) and a Linux machine (receiver).

## Security model
- Transport: **Noise_XX_25519_ChaChaPoly_BLAKE2s** (mutual authentication, forward secrecy, ChaCha20-Poly1305).
- Every device has a long-term key (`identity.key`, user-only permissions). Only **paired** keys are accepted.
- Pairing is explicit: run both sides with `--pair`, confirm the same 6-digit code on both screens
  (this defeats a man-in-the-middle). Pairing mode on the receiver closes after 120 s or the first pairing.
- Unknown or unpaired peers are rejected before any input is processed.
- The receiver releases all held keys/buttons if the sender leaves or goes silent for 10 s.
- Escape key (default Scroll Lock) always returns control to the sender PC.
- Config panel: loopback only, random secret token, Host check, strict CSP.
- Not implemented yet: clipboard sync (setting exists), system-tray icon, auto-start.

## Files
Config dir: `%APPDATA%\softwarekvm` (Windows) or `~/.config/softwarekvm` (Linux):
`config.toml`, `identity.key`, `peers.toml`, `panel.token`.

## Usage
Receiver (Linux), needs access to `/dev/uinput`:
```
sudo ./target/release/kvmd listen --pair      # first time: pair
sudo ./target/release/kvmd listen             # afterwards
```
Sender (Windows):
```
kvmd send 192.168.1.50:24800 --pair           # first time: pair, confirm the code on both
kvmd send                                     # afterwards (address comes from config)
kvmd panel                                    # config panel in your browser
```

### Run the receiver without sudo
```
echo 'KERNEL=="uinput", GROUP="input", MODE="0660"' | sudo tee /etc/udev/rules.d/99-uinput.rules
sudo usermod -aG input $USER     # log out and in again
sudo udevadm control --reload && sudo udevadm trigger
```

## Layout
`crates/protocol` shared types + encrypted transport; `crates/daemon` (`kvmd`): Windows capture,
Linux/Windows injection, local config panel.

## Roadmap
Clipboard, tray icon + autostart, on-demand video from the DL380.

## On-demand display for a headless machine
On the Linux box (as your normal user, no sudo): `sudo apt install xvfb x11vnc xfce4`, then
```
kvmd display start [1920x1080]    # Xvfb + XFCE + x11vnc (127.0.0.1 only)
kvmd display stop
kvmd display status
```
The receiver (`sudo kvmd listen`) must be running. On Windows:
```
kvmd view 10.0.0.180:24800        # then connect any VNC viewer to localhost:5901
```
The VNC stream travels inside the same paired, encrypted connection; no extra port is exposed.
Note: the VNC viewer supplies its own keyboard/mouse for that display. Edge-switching input
(uinput) controls a real desktop, not the Xvfb screen.
