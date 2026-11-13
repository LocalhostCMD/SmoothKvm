//! Update checks, one-click updates and uninstall.
//!
//! Releases are published on GitHub with a small manifest (`latest.json`: version, file names, SHA-256 hashes)
//! signed with the SmoothKvm Ed25519 release key, whose public half is built into this program. Nothing is
//! installed unless that signature checks out, the version is NEWER than the running one (no downgrades to an
//! old, signed-but-vulnerable build), and the downloaded file matches the signed hash. The download itself
//! goes over HTTPS, but security does not depend on it: a tampered file or manifest is simply refused.
//!
//! Windows: the signed installer runs silently (per-user, no admin) and restarts the app.
//! Linux: installing a .deb needs root, which the app never has. The app only drops the downloaded files in
//! /var/lib/smoothkvm and asks; the root `smoothkvm-helper` systemd unit (shipped in the package) copies them
//! somewhere only root can write, checks everything again itself, and then runs apt.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use std::io::{self, Read};
#[cfg(unix)]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const RELEASES: &str = "https://github.com/LocalhostCMD/SmoothKvm/releases/latest/download";
pub const RELEASE_PAGE: &str = "https://github.com/LocalhostCMD/SmoothKvm/releases";
/// Ed25519 public key of the SmoothKvm release key (the private key never leaves the maintainer's PC).
const PUBLIC_KEY_HEX: &str = "80b11525677c912fc3adab838bc46ebea2d5b02b867f7952c63cb541db56ba44";
#[cfg(unix)]
const BASE: &str = "/var/lib/smoothkvm";
#[cfg(unix)]
const HELPER_RUN: &str = "/run/smoothkvm-helper";
const MAX_MANIFEST: u64 = 64 * 1024;
const MAX_PACKAGE: u64 = 400 * 1024 * 1024;

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Asset {
    pub name: String,
    pub sha256: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub notes: String,
    pub windows: Option<Asset>,
    pub linux: Option<Asset>,
}

impl Manifest {
    fn asset(&self) -> Option<&Asset> {
        if cfg!(windows) {
            self.windows.as_ref()
        } else {
            self.linux.as_ref()
        }
    }
}

// ---------------------------------------------------------------- signatures and versions

fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn public_key() -> [u8; 32] {
    unhex(PUBLIC_KEY_HEX).and_then(|v| v.try_into().ok()).expect("built-in key")
}

/// Ed25519 signature check (RFC 8032): [S]B == R + [SHA512(R || A || M)]A, with S required to be canonical.
pub fn verify_ed25519(pk: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
    use curve25519_dalek::scalar::Scalar;
    let Some(a) = CompressedEdwardsY(*pk).decompress() else { return false };
    if a.is_small_order() {
        return false;
    }
    let r_bytes: [u8; 32] = sig[..32].try_into().unwrap();
    let s_bytes: [u8; 32] = sig[32..].try_into().unwrap();
    let Some(s) = Option::<Scalar>::from(Scalar::from_canonical_bytes(s_bytes)) else { return false };
    if CompressedEdwardsY(r_bytes).decompress().is_none() {
        return false;
    }
    let mut h = Sha512::new();
    h.update(r_bytes);
    h.update(pk);
    h.update(msg);
    let mut wide = [0u8; 64];
    wide.copy_from_slice(&h.finalize());
    let k = Scalar::from_bytes_mod_order_wide(&wide);
    // R' = [S]B - [k]A
    let r = EdwardsPoint::vartime_double_scalar_mul_basepoint(&k, &(-a), &s);
    r.compress().to_bytes() == r_bytes
}

/// Check the manifest's signature (hex text, as written by the release script) and parse it.
pub fn verify_manifest(bytes: &[u8], sig_text: &[u8]) -> Result<Manifest, String> {
    let sig: [u8; 64] = std::str::from_utf8(sig_text)
        .ok()
        .and_then(unhex)
        .and_then(|v| v.try_into().ok())
        .ok_or("the update signature is malformed")?;
    if !verify_ed25519(&public_key(), bytes, &sig) {
        return Err("the update is NOT signed by the SmoothKvm release key, so it was refused".into());
    }
    let m: Manifest = serde_json::from_slice(bytes).map_err(|e| format!("the update information is malformed: {e}"))?;
    if parse_version(&m.version).is_none() {
        return Err("the update has an invalid version number".into());
    }
    for a in [&m.windows, &m.linux].into_iter().flatten() {
        check_name(&a.name)?;
        if a.sha256.len() != 64 || unhex(&a.sha256).is_none() {
            return Err("the update has an invalid file hash".into());
        }
    }
    Ok(m)
}

fn check_name(n: &str) -> Result<(), String> {
    let ok = !n.is_empty() && n.len() <= 100 && !n.starts_with('.') && n.chars().all(|c| c.is_ascii_alphanumeric() || "._-+~".contains(c));
    if ok {
        Ok(())
    } else {
        Err("the update has an invalid file name".into())
    }
}

pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.trim().trim_start_matches('v').split('.');
    let p = |s: Option<&str>| s.and_then(|x| x.parse::<u64>().ok());
    let r = (p(it.next())?, p(it.next())?, p(it.next())?);
    if it.next().is_some() {
        return None;
    }
    Some(r)
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn sha256_file(p: &Path) -> io::Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

// ---------------------------------------------------------------- state shown in the app

#[derive(Serialize, Clone, Default)]
pub struct UpdateState {
    pub latest: Option<String>,
    pub notes: String,
    pub available: bool,
    pub checked_at: u64,
    pub checking: bool,
    /// What is happening right now ("Downloading...", "Installing..."), while an update runs.
    pub busy: Option<String>,
    pub error: Option<String>,
}

static STATE: Mutex<Option<UpdateState>> = Mutex::new(None);
static NOTIFIED: Mutex<Option<String>> = Mutex::new(None);

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn with<R>(f: impl FnOnce(&mut UpdateState) -> R) -> R {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(UpdateState::default))
}

pub fn state() -> serde_json::Value {
    let s = with(|s| s.clone());
    let mut v = serde_json::to_value(s).unwrap_or_default();
    v["current"] = serde_json::json!(crate::VERSION);
    v["page"] = serde_json::json!(RELEASE_PAGE);
    v
}

#[cfg_attr(unix, allow(dead_code))] // used by the Windows tray menu
pub fn available() -> Option<String> {
    with(|s| if s.available && s.busy.is_none() { s.latest.clone() } else { None })
}

// ---------------------------------------------------------------- downloading

fn work_dir() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("SmoothKvm").join("updates")
    }
    #[cfg(unix)]
    {
        // Installed from the .deb: use the folder the root helper reads from.
        let d = Path::new(BASE).join("update");
        let probe = d.join(".probe");
        if Path::new(BASE).is_dir() && std::fs::create_dir_all(&d).is_ok() && std::fs::write(&probe, b"").is_ok() {
            let _ = std::fs::remove_file(&probe);
            return d;
        }
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
        home.join(".cache").join("smoothkvm").join("update")
    }
}

/// Download `url` to `dest` with the system's own HTTPS client (curl ships with Windows 10+ and nearly every
/// Linux; wget / python3 are fallbacks). Size is capped; the content is verified by the caller.
fn fetch(url: &str, dest: &Path, max: u64) -> Result<(), String> {
    let _ = std::fs::remove_file(dest);
    let dest_s = dest.to_string_lossy().to_string();
    let max_s = max.to_string();
    let curl_args = ["-fsSL", "--proto", "=https", "--proto-redir", "=https", "--connect-timeout", "20", "--max-time", "900", "--max-filesize", &max_s, "-o", &dest_s, url];
    #[cfg(windows)]
    let result = {
        use std::os::windows::process::CommandExt;
        let sys = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        std::process::Command::new(format!(r"{sys}\System32\curl.exe")).args(curl_args).creation_flags(0x0800_0000).output()
    };
    #[cfg(unix)]
    let result = {
        let run = |prog: &str, args: &[&str]| std::process::Command::new(prog).args(args).stdin(std::process::Stdio::null()).output();
        let py = "import sys,urllib.request\nr=urllib.request.urlopen(sys.argv[1],timeout=60)\nassert r.geturl().startswith('https://')\nopen(sys.argv[2],'wb').write(r.read(int(sys.argv[3])+1))";
        match run("curl", &curl_args) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => match run("wget", &["-q", "--https-only", "--timeout=30", "-O", &dest_s, url]) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => run("python3", &["-c", py, url, &dest_s, &max_s]),
                r => r,
            },
            r => r,
        }
    };
    let out = result.map_err(|e| format!("could not run the downloader: {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(dest);
        let err = String::from_utf8_lossy(&out.stderr);
        let err = crate::shared::clean(err.lines().last().unwrap_or("").trim());
        return Err(if err.contains("404") {
            "no published release was found (is the GitHub repository public, with a release?)".into()
        } else if err.is_empty() {
            "the download failed (no internet?)".into()
        } else {
            format!("the download failed: {err}")
        });
    }
    match std::fs::metadata(dest) {
        Ok(m) if m.len() <= max => Ok(()),
        _ => {
            let _ = std::fs::remove_file(dest);
            Err("the download was too large or missing".into())
        }
    }
}

/// Fetch and verify the newest release information. Returns the manifest if it is newer than this version.
pub fn check() -> Result<Option<Manifest>, String> {
    with(|s| s.checking = true);
    let res = (|| -> Result<Manifest, String> {
        let dir = work_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let (mf, sf) = (dir.join("latest.json"), dir.join("latest.json.sig"));
        fetch(&format!("{RELEASES}/latest.json"), &mf, MAX_MANIFEST)?;
        fetch(&format!("{RELEASES}/latest.json.sig"), &sf, 1024)?;
        let m = verify_manifest(&std::fs::read(&mf).map_err(|e| e.to_string())?, &std::fs::read(&sf).map_err(|e| e.to_string())?)?;
        Ok(m)
    })();
    with(|s| {
        s.checking = false;
        s.checked_at = now();
        match &res {
            Ok(m) => {
                s.available = is_newer(&m.version, crate::VERSION) && m.asset().is_some();
                s.latest = Some(m.version.clone());
                s.notes = crate::shared::clean(&m.notes);
                s.error = None;
            }
            Err(e) => s.error = Some(e.clone()),
        }
    });
    res.map(|m| if is_newer(&m.version, crate::VERSION) && m.asset().is_some() { Some(m) } else { None })
}

/// Check once shortly after start and then about once a day (if the setting is on).
pub fn start_checker() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(20));
        let mut last = 0u64;
        loop {
            if crate::store::load_config().update_check && now().saturating_sub(last) >= 24 * 3600 {
                last = now();
                if let Ok(Some(m)) = check() {
                    let mut n = NOTIFIED.lock().unwrap_or_else(|e| e.into_inner());
                    if n.as_deref() != Some(m.version.as_str()) {
                        *n = Some(m.version.clone());
                        crate::shared::notify(&format!("SmoothKvm {} is available. Open SmoothKvm and click Update.", m.version));
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(600));
        }
    });
}

/// Check now (from the app's "Check for updates" button).
pub fn check_now() -> serde_json::Value {
    let _ = check();
    state()
}

// ---------------------------------------------------------------- applying an update

/// Download, verify and install the newest version in the background (progress in `state()`).
pub fn apply_async() -> Result<(), String> {
    let started = with(|s| {
        if s.busy.is_some() {
            false
        } else {
            s.busy = Some("Checking for the newest version...".into());
            s.error = None;
            true
        }
    });
    if !started {
        return Err("an update is already running".into());
    }
    std::thread::spawn(|| {
        if let Err(e) = apply() {
            crate::shared::warn(&format!("Update failed: {e}"));
            with(|s| {
                s.busy = None;
                s.error = Some(e);
            });
        }
    });
    Ok(())
}

fn busy(t: &str) {
    with(|s| s.busy = Some(t.to_string()));
}

fn apply() -> Result<(), String> {
    let m = check()?.ok_or_else(|| format!("You already have the newest version ({}).", crate::VERSION))?;
    let a = m.asset().ok_or("this release has no download for this system")?.clone();
    busy(&format!("Downloading SmoothKvm {}...", m.version));
    let dir = work_dir();
    let file = dir.join(&a.name);
    fetch(&format!("{RELEASES}/{}", a.name), &file, MAX_PACKAGE)?;
    let got = sha256_file(&file).map_err(|e| e.to_string())?;
    if !got.eq_ignore_ascii_case(&a.sha256) {
        let _ = std::fs::remove_file(&file);
        return Err("the downloaded file does not match the signed release (it was refused and deleted)".into());
    }
    crate::shared::info(&format!("Downloaded SmoothKvm {} and verified its signature; installing", m.version));
    busy(&format!("Installing SmoothKvm {}... the app restarts by itself", m.version));
    install(&file, &m)
}

#[cfg(windows)]
fn install(file: &Path, _m: &Manifest) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let installed = installer_dir().is_some();
    let mut c = std::process::Command::new(file);
    if installed {
        // Same options as before (remembered by the installer), no questions, and it reopens the app.
        c.args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-"]);
    }
    c.creation_flags(0x0000_0008 | 0x0000_0200).spawn().map_err(|e| format!("could not start the installer: {e}"))?;
    quit_soon();
    Ok(())
}

#[cfg(unix)]
fn install(file: &Path, m: &Manifest) -> Result<(), String> {
    if !file.starts_with(BASE) {
        return Err(format!(
            "SmoothKvm was not installed from its .deb package, so it cannot update itself. The new version was downloaded to {}; install it with: sudo apt install {}",
            file.display(),
            file.display()
        ));
    }
    let nonce = ask_helper("update")?;
    // On success the package restarts this service, so normally we never get past this loop.
    for _ in 0..600 {
        std::thread::sleep(Duration::from_millis(500));
        if let Some(r) = helper_result(&nonce) {
            return match r.strip_prefix("ok") {
                Some(_) => {
                    busy(&format!("SmoothKvm {} is installed. Restarting...", m.version));
                    Ok(())
                }
                None => Err(r.trim_start_matches("error").trim().to_string()),
            };
        }
    }
    Err("the installer did not answer (is the smoothkvm-helper.path unit enabled? sudo systemctl enable --now smoothkvm-helper.path)".into())
}

#[cfg(windows)]
fn quit_soon() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(400));
        crate::gui::quit_app();
        std::thread::sleep(Duration::from_secs(3));
        std::process::exit(0);
    });
}

/// The folder SmoothKvm was installed into by its installer (it has the uninstaller next to the program).
#[cfg(windows)]
fn installer_dir() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    dir.join("unins000.exe").exists().then_some(dir)
}

// ---------------------------------------------------------------- uninstall

/// Remove SmoothKvm from this computer (and, if `wipe`, its settings, device key and paired devices).
pub fn uninstall(wipe: bool) -> Result<String, String> {
    let cfg_dir = crate::store::config_dir();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let dir = installer_dir().ok_or_else(|| {
            let here = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.display().to_string())).unwrap_or_default();
            format!("This copy of SmoothKvm was not installed with the installer, so there is nothing to uninstall. Just close it and delete the folder {here}")
        })?;
        crate::shared::warn(&format!("Uninstalling SmoothKvm{}", if wipe { " and deleting its settings" } else { "" }));
        std::process::Command::new(dir.join("unins000.exe"))
            .args(["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART"])
            .creation_flags(0x0000_0008 | 0x0000_0200)
            .spawn()
            .map_err(|e| format!("could not start the uninstaller: {e}"))?;
        if wipe {
            // after this app has closed (the webview keeps files open until then)
            let d = cfg_dir.display().to_string();
            if !d.contains('"') && crate::store::is_config_dir_name(&cfg_dir) {
                let _ = std::process::Command::new("cmd")
                    .args(["/C", &format!("ping -n 6 127.0.0.1 >nul & rmdir /S /Q \"{d}\"")])
                    .creation_flags(0x0800_0000 | 0x0000_0008)
                    .spawn();
            }
        }
        quit_soon();
        Ok("SmoothKvm is being removed. This window closes now.".into())
    }
    #[cfg(unix)]
    {
        if !Path::new(BASE).is_dir() {
            return Err("SmoothKvm was not installed from its .deb package. Remove it with: sudo apt remove smoothkvm".into());
        }
        if wipe && crate::store::is_config_dir_name(&cfg_dir) {
            let _ = std::fs::remove_dir_all(&cfg_dir);
        }
        crate::shared::warn("Uninstalling SmoothKvm");
        #[cfg(target_os = "linux")]
        crate::edge_linux::unload();
        let nonce = ask_helper("uninstall")?;
        for _ in 0..240 {
            std::thread::sleep(Duration::from_millis(500));
            if let Some(r) = helper_result(&nonce) {
                return match r.strip_prefix("ok") {
                    Some(_) => Ok("SmoothKvm was removed from this computer.".into()),
                    None => Err(r.trim_start_matches("error").trim().to_string()),
                };
            }
        }
        Err("the uninstaller did not answer; remove it with: sudo apt remove smoothkvm".into())
    }
}

// ---------------------------------------------------------------- Linux root helper

/// Ask the root helper to do something; returns the request id to wait for.
#[cfg(unix)]
fn ask_helper(what: &str) -> Result<String, String> {
    let mut b = [0u8; 8];
    let _ = getrandom::getrandom(&mut b);
    let nonce: String = b.iter().map(|x| format!("{x:02x}")).collect();
    let tmp = Path::new(BASE).join("request.tmp");
    std::fs::write(&tmp, format!("{what} {nonce}\n")).map_err(|e| format!("cannot ask the system helper: {e}"))?;
    std::fs::rename(&tmp, Path::new(BASE).join("request")).map_err(|e| format!("cannot ask the system helper: {e}"))?;
    Ok(nonce)
}

#[cfg(unix)]
fn helper_result(nonce: &str) -> Option<String> {
    let s = std::fs::read_to_string(Path::new(HELPER_RUN).join("result")).ok()?;
    s.trim().strip_prefix(nonce).map(|r| r.trim().to_string())
}

/// Open a file without following symlinks, refusing anything that is not a regular file or is too big.
#[cfg(unix)]
fn open_plain(p: &Path, max: u64) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(p)?;
    let m = f.metadata()?;
    if !m.is_file() || m.len() > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} is not a plain file of a sane size", p.display())));
    }
    Ok(f)
}

#[cfg(unix)]
fn read_plain(p: &Path, max: u64) -> io::Result<Vec<u8>> {
    let mut v = Vec::new();
    open_plain(p, max)?.take(max).read_to_end(&mut v)?;
    Ok(v)
}

/// `SmoothKvm helper`: run by the smoothkvm-helper systemd unit (as root) when the app drops a request.
#[cfg(unix)]
pub fn helper_main() -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "the helper is started by systemd as root, not by hand"));
    }
    let base = Path::new(BASE);
    let req = read_plain(&base.join("request"), 256).unwrap_or_default();
    let _ = std::fs::remove_file(base.join("request"));
    let req = String::from_utf8_lossy(&req).to_string();
    let mut it = req.split_whitespace();
    let what = it.next().unwrap_or("").to_string();
    let nonce: String = it.next().unwrap_or("0").chars().filter(|c| c.is_ascii_alphanumeric()).take(32).collect();
    let res = match what.as_str() {
        "update" => helper_update(base),
        "uninstall" => run_apt(&["remove", "-y", "smoothkvm"]).map(|_| "removed".to_string()),
        _ => Err("unknown request".into()),
    };
    let run = Path::new(HELPER_RUN);
    std::fs::create_dir_all(run)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(run, std::fs::Permissions::from_mode(0o755))?;
    }
    let line = match &res {
        Ok(m) => format!("{nonce} ok {m}\n"),
        Err(e) => format!("{nonce} error {}\n", crate::shared::clean(e)),
    };
    let tmp = run.join("result.tmp");
    std::fs::write(&tmp, line)?;
    std::fs::rename(&tmp, run.join("result"))?;
    match res {
        Ok(m) => println!("smoothkvm-helper: {what}: {m}"),
        Err(e) => eprintln!("smoothkvm-helper: {what} failed: {e}"),
    }
    Ok(())
}

#[cfg(unix)]
fn helper_update(base: &Path) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    let up = base.join("update");
    // Everything is re-checked here, by root, on copies the app can no longer touch.
    let manifest = read_plain(&up.join("latest.json"), MAX_MANIFEST).map_err(|e| e.to_string())?;
    let sig = read_plain(&up.join("latest.json.sig"), 1024).map_err(|e| e.to_string())?;
    let m = verify_manifest(&manifest, &sig)?;
    if !is_newer(&m.version, crate::VERSION) {
        return Err(format!("{} is not newer than the installed {}", m.version, crate::VERSION));
    }
    let a = m.linux.clone().ok_or("this release has no Linux package")?;
    let work = Path::new(HELPER_RUN).join("work");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    let dst = work.join(&a.name);
    {
        let mut src = open_plain(&up.join(&a.name), MAX_PACKAGE).map_err(|e| e.to_string())?;
        let mut out = std::fs::File::create(&dst).map_err(|e| e.to_string())?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = src.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
            out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        }
        let got: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if !got.eq_ignore_ascii_case(&a.sha256) {
            let _ = std::fs::remove_dir_all(&work);
            return Err("the package does not match the signed release; refused".into());
        }
    }
    let r = run_apt(&["install", "-y", &dst.to_string_lossy()]);
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_file(up.join(&a.name));
    r.map(|_| format!("installed {}", m.version))
}

#[cfg(unix)]
fn run_apt(args: &[&str]) -> Result<(), String> {
    let mut all = vec!["-o", "DPkg::Lock::Timeout=180", "-o", "Dpkg::Options::=--force-confdef", "-o", "Dpkg::Options::=--force-confold"];
    all.extend_from_slice(args);
    let out = std::process::Command::new("apt-get")
        .args(&all)
        .env("DEBIAN_FRONTEND", "noninteractive")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("cannot run apt-get: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(format!("apt-get failed: {}", err.lines().filter(|l| l.starts_with("E:")).last().unwrap_or(err.lines().last().unwrap_or("")).trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 8032, section 7.1, TEST 1 and TEST 2
    #[test]
    fn rfc8032_vectors() {
        let pk: [u8; 32] = unhex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a").unwrap().try_into().unwrap();
        let sig: [u8; 64] = unhex("e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b").unwrap().try_into().unwrap();
        assert!(verify_ed25519(&pk, b"", &sig));
        assert!(!verify_ed25519(&pk, b"x", &sig));
        let pk2: [u8; 32] = unhex("3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c").unwrap().try_into().unwrap();
        let sig2: [u8; 64] = unhex("92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00").unwrap().try_into().unwrap();
        assert!(verify_ed25519(&pk2, &[0x72], &sig2));
        let mut bad = sig2;
        bad[5] ^= 1;
        assert!(!verify_ed25519(&pk2, &[0x72], &bad));
        // S not reduced mod L must be refused (signature malleability)
        let mut big = sig2;
        big[63] |= 0xf0;
        assert!(!verify_ed25519(&pk2, &[0x72], &big));
    }

    #[test]
    fn versions() {
        assert!(is_newer("0.9.0", "0.8.0"));
        assert!(is_newer("v1.0.0", "0.99.9"));
        assert!(is_newer("0.8.10", "0.8.9"));
        assert!(!is_newer("0.8.0", "0.8.0"));
        assert!(!is_newer("0.7.9", "0.8.0"));
        assert!(!is_newer("0.9", "0.8.0"));
        assert!(!is_newer("0.9.0-evil", "0.8.0"));
    }

    #[test]
    fn manifest_must_be_signed() {
        let m = br#"{"version":"9.9.9","windows":{"name":"SmoothKvm-Setup-9.9.9.exe","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}}"#;
        let fake_sig = "00".repeat(64);
        assert!(verify_manifest(m, fake_sig.as_bytes()).is_err());
        assert!(verify_manifest(m, b"nonsense").is_err());
    }

    #[test]
    fn file_names_are_plain() {
        assert!(check_name("smoothkvm_0.9.0_amd64.deb").is_ok());
        assert!(check_name("SmoothKvm-Setup-0.9.0.exe").is_ok());
        assert!(check_name("../etc/passwd").is_err());
        assert!(check_name("a/b.deb").is_err());
        assert!(check_name(".hidden").is_err());
    }

    /// A manifest signed by the real release key (made by the release script) is accepted.
    /// Run with SMOOTHKVM_TEST_MANIFEST=<dir containing latest.json and latest.json.sig>.
    #[test]
    fn real_signed_manifest() {
        let Some(dir) = std::env::var_os("SMOOTHKVM_TEST_MANIFEST") else { return };
        let dir = PathBuf::from(dir);
        let m = std::fs::read(dir.join("latest.json")).unwrap();
        let s = std::fs::read(dir.join("latest.json.sig")).unwrap();
        verify_manifest(&m, &s).expect("signed by the release key");
        let mut t = m.clone();
        t[5] ^= 1;
        assert!(verify_manifest(&t, &s).is_err());
    }
}
