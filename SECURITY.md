# Security

## Verify a download
Every release lists SHA-256 hashes, and ships `SHA256SUMS` with a GPG signature.

**GPG (all files)** - the signing key is `packaging/signing/SmoothKvm-release-key.asc`
(fingerprint `B637 A9A6 9CB1 BF43 4ADD 437E 1859 0F16 F66B 3B05`):
```
gpg --import SmoothKvm-release-key.asc
gpg --verify SHA256SUMS.asc SHA256SUMS
sha256sum -c SHA256SUMS            # Linux;  Windows: Get-FileHash <file> and compare
gpg --verify smoothkvm_<version>_amd64.deb.asc smoothkvm_<version>_amd64.deb
```
Check that the fingerprint printed by gpg matches the one above, and that it matches what you were told when you got
the key (compare against the copy in your own clone of the repository, not only the release page).

**Windows signatures** - `SmoothKvm.exe`, `SmoothKvmApp.exe` and the installer are Authenticode-signed and
timestamped with a self-issued certificate (`packaging/signing/SmoothKvm-code-signing.cer`, thumbprint
`8907D19055E223D2AD4FEF0A47CBE682EE66286C`). Windows will say the publisher is "unknown" until you choose to trust
that certificate on a machine, because it was not issued by a public certificate authority. The signature still proves
the file is unmodified: right-click -> Properties -> Digital Signatures, or
`Get-AuthenticodeSignature <file>` (status `UnknownError` = signed but the root is not trusted; `HashMismatch` = the file
was changed after signing, do not run it).

To trust it on a machine you control (PowerShell as the current user):
```
Import-Certificate -FilePath SmoothKvm-code-signing.cer -CertStoreLocation Cert:\CurrentUser\TrustedPublisher
```
Only do this after verifying the thumbprint above through a channel you trust.

## Transparency
- On Windows the app is never hidden: starting it always opens its window, and closing the window (or choosing Quit in the tray) stops everything - it releases held keys, closes every connection and listener, and exits. "Open when I sign in" opens the window too.
- No telemetry, no accounts, no auto-update, no internet connections of its own. It only connects to the addresses you add, and listens only when you turn on "Let paired computers control this PC".
- The app shows an activity and security log (also `/log` in the terminal, saved to `events.log` in the settings folder). Pairing, new devices, sessions, emergency stops, removed devices, and refused or failed connection attempts are recorded; failed attempts are marked as security alerts and counted on the tray icon.

## What protects what
- Transport: Noise_XX_25519_ChaChaPoly_BLAKE2s; pairing: Noise_XXpsk3 with a single-use 8-digit PIN that is burned after
  any attempt. Receivers accept only paired keys, clamp every received number, and release held keys if the controller
  goes silent. Unauthenticated connections: 5 s and 2 slots per address.
- Messages are decoded with `postcard` (maintained) with every length checked; the decoder is fuzzed in the test suite.
- Local control API: 127.0.0.1 only, 256-bit token (constant-time compare), Host check, CSP and isolation headers.
- Secrets: the device private key is encrypted with Windows DPAPI (this account, this PC); on Linux it is a 0600 file in a
  0700 folder, re-tightened if loosened. The Linux service runs with systemd sandbox options.
- Dependencies are scanned with `cargo audit` (no known vulnerabilities in either shipped program).

## Known limits
- A paired controller is fully trusted: it can type anything on the machine it controls.
- Code signing uses a self-issued certificate and a GPG key kept on the maintainer's PC (no passphrase). A certificate from
  a public authority (Azure Trusted Signing, SignPath Foundation for open source, or an OV/EV certificate) would remove
  the "unknown publisher" warning; it needs a paid or verified account.
- Linux keeps the device key in a protected file rather than a keyring.

## Reporting a problem
Open a private security advisory on the repository.
