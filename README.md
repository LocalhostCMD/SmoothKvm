# SmoothKvm

One keyboard and mouse across several computers. Your Windows PC controls other machines (Ubuntu today) as
extra screens on its left/right/top/bottom, and a Windows PC can also be controlled by another paired computer.
Everything between machines is encrypted and only paired devices are accepted.

## Install
**Windows**: run `SmoothKvm-Setup-<version>.exe` (no admin rights; Start Menu icon, optional start-with-Windows,
optional `SmoothKvm` in your terminal).
**Ubuntu / Debian**: install `smoothkvm_<version>_amd64.deb` (double-click, or `sudo apt install ./smoothkvm_*.deb`).
It starts a service on boot and adds the `SmoothKvm` / `smoothkvm` commands.

## Pairing (once per pair of devices)
1. On the device that will be **controlled**: open pairing (Ubuntu terminal UI: `pair`; Windows app: *Allow a new device to
   pair*). It shows a **single-use 8-digit PIN**.
2. On the controlling PC: *Pair this device* in the app and type the PIN (terminal UI: `pair <screen> <PIN>`).
The PIN is mixed into the key exchange, works for one attempt only, and any wrong try cancels it.

## Use
- **Windows app**: Start Menu -> SmoothKvm. Click a side of "This PC" to add a screen, enter `host:24800`.
- **Terminal UI (both)**: type `SmoothKvm`. Commands work with or without a leading `/`; `help` lists them.
  `F2` = emergency stop, `F3` = resume.
- **Emergency stop**: red button, tray menu, `stop`, F2, or press **Scroll Lock three times**.
- **Effects** (colourful, drawn on screen, never move your mouse): Ripple, Glow, Sparks, Comet x Rainbow, Ocean,
  Sunset, Neon, Mono, with motion curves (bounce, elastic, spring, overshoot or your own).
- **Game mode**: no screen switching while a full-screen app or game runs.
- **Virtual display** for headless machines (Xvfb + XFCE + x11vnc through the encrypted tunnel).

## Security model
- Transport: Noise_XX_25519_ChaChaPoly_BLAKE2s (mutual authentication, forward secrecy, strict nonces); pairing uses
  Noise_XXpsk3 with a PIN-derived key, so a man-in-the-middle cannot join. Only paired keys are accepted afterwards.
- Unauthenticated connections get 5 s and a small per-address budget. Receivers clamp every number they receive and
  release held keys if the controller goes silent for 10 s.
- Local control API: 127.0.0.1 only, 256-bit token compared in constant time, Host-header check, strict CSP and
  isolation headers. The display tunnel is one-shot (one connection, 60 s). Secret files are owner-only (0600/0700, and
  re-tightened if loosened). The Linux service is sandboxed with systemd hardening options.
- Known limits: a paired controller is fully trusted (it can type anything on the controlled machine); binaries are
  unsigned, so antivirus may warn (compare the SHA-256 on the release page); Windows keys are not DPAPI-encrypted.

## Build
`cargo build --release`. Windows installer: `packaging/windows/SmoothKvm.iss` (Inno Setup 6). Ubuntu package:
`packaging/linux/build-deb.sh` (or `wsl-build.sh` from Windows).

## Verifying downloads
Releases include `SHA256SUMS` + GPG signatures and Authenticode-signed Windows files; see [SECURITY.md](SECURITY.md).
