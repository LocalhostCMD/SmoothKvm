//! On-disk state: config, this device's identity key, trusted peers, panel token.
use kvm_protocol::secure::{from_hex, generate_keypair, to_hex};
use kvm_protocol::Config;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

/// The settings folder: %APPDATA%\SmoothKvm on Windows, ~/.config/smoothkvm on Linux.
/// Versions before 0.8.3 used "softwarekvm"; that folder is moved here the first time (keeping every pairing).
pub fn config_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        #[cfg(windows)]
        let base = std::env::var_os("APPDATA").map(PathBuf::from);
        #[cfg(not(windows))]
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
        let base = base.unwrap_or_else(|| PathBuf::from("."));
        let new = base.join(if cfg!(windows) { "SmoothKvm" } else { "smoothkvm" });
        let old = base.join("softwarekvm");
        if !new.exists() && old.is_dir() && fs::rename(&old, &new).is_err() {
            // still in use (another copy running): keep using it, it is moved next time
            return old;
        }
        new
    })
    .clone()
}

/// True for a settings folder name (current or pre-0.8.3), used as a safety check before deleting one.
pub fn is_config_dir_name(p: &std::path::Path) -> bool {
    p.file_name().map(|n| n.to_string_lossy().to_lowercase()).map(|n| n == "smoothkvm" || n == "softwarekvm").unwrap_or(false)
}

/// Write a file that only the current user may read (secrets).
fn write_private(path: &PathBuf, contents: &str) -> io::Result<()> {
    let dir = path.parent().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&dir)?;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        opts.mode(0o600);
        // the folder holding the secrets is private to this user too
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    let mut f = opts.open(path)?;
    // a file that already existed keeps its old mode, so force it
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(contents.as_bytes())
}

/// Secrets must never be readable by other accounts: tighten them if something (a backup, a copy,
/// a loose umask) widened the permissions.
#[cfg(unix)]
pub(crate) fn harden(path: &PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(m) = fs::metadata(path) {
        if m.permissions().mode() & 0o077 != 0 {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
    }
}
#[cfg(not(unix))]
pub(crate) fn harden(_path: &PathBuf) {}

pub struct Identity {
    pub private: Vec<u8>,
    pub public: Vec<u8>,
}

impl Identity {
    pub fn load_or_create() -> io::Result<Self> {
        let path = config_dir().join("identity.key");
        harden(&path);
        if let Ok(text) = fs::read_to_string(&path) {
            let mut lines = text.lines();
            if let (Some(a), Some(b)) = (lines.next(), lines.next()) {
                if let Some(blob_hex) = a.strip_prefix("dpapi:") {
                    // Windows: the private key is encrypted for this Windows account.
                    #[cfg(windows)]
                    if let (Some(blob), Some(public)) = (from_hex(blob_hex), from_hex(b)) {
                        return match crate::dpapi::unprotect(&blob) {
                            Ok(private) => Ok(Self { private, public }),
                            // Never quietly invent a new identity: that would orphan every pairing.
                            Err(_) => Err(io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                "identity.key cannot be decrypted (it belongs to another Windows account or computer)",
                            )),
                        };
                    }
                    let _ = blob_hex;
                } else if let (Some(private), Some(public)) = (from_hex(a), from_hex(b)) {
                    // Older plaintext key: use it, and upgrade the file in place (encrypts it on Windows).
                    let me = Self { private, public };
                    me.save(&path)?;
                    return Ok(me);
                }
            }
        }
        let (private, public) = generate_keypair()?;
        let me = Self { private, public };
        me.save(&path)?;
        Ok(me)
    }

    fn save(&self, path: &PathBuf) -> io::Result<()> {
        #[cfg(windows)]
        let first = format!("dpapi:{}", to_hex(&crate::dpapi::protect(&self.private)?));
        #[cfg(not(windows))]
        let first = to_hex(&self.private);
        write_private(path, &format!("{first}\n{}\n", to_hex(&self.public)))
    }
}

/// Short human-readable fingerprint of a public key (hex, grouped).
pub fn short(key_hex: &str) -> String {
    key_hex.get(..16).unwrap_or(key_hex).as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join("-")
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Peer {
    pub name: String,
    pub key: String,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Peers {
    #[serde(default)]
    pub peers: Vec<Peer>,
}

impl Peers {
    fn path() -> PathBuf {
        config_dir().join("peers.toml")
    }

    pub fn load() -> Self {
        fs::read_to_string(Self::path()).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> io::Result<()> {
        let text = toml::to_string_pretty(self).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        write_private(&Self::path(), &text)
    }

    pub fn is_trusted(&self, key: &[u8]) -> bool {
        let k = to_hex(key);
        self.peers.iter().any(|p| p.key == k)
    }

    pub fn add(&mut self, name: &str, key: &[u8]) {
        let k = to_hex(key);
        if !self.peers.iter().any(|p| p.key == k) {
            self.peers.push(Peer { name: name.to_string(), key: k });
        }
    }
}

fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn load_config() -> Config {
    fs::read_to_string(config_path()).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
}

#[allow(dead_code)]
pub fn config_mtime() -> Option<std::time::SystemTime> {
    fs::metadata(config_path()).and_then(|m| m.modified()).ok()
}

#[allow(dead_code)]
pub fn save_config(cfg: &Config) -> io::Result<()> {
    let text = toml::to_string_pretty(cfg).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    fs::create_dir_all(config_dir())?;
    fs::write(config_path(), text)
}

/// Random secret that the local config panel requires on every API call.
#[allow(dead_code)]
pub fn panel_token() -> io::Result<String> {
    let path = config_dir().join("panel.token");
    harden(&path);
    if let Ok(t) = fs::read_to_string(&path) {
        if t.trim().len() == 64 {
            return Ok(t.trim().to_string());
        }
    }
    let mut b = [0u8; 32];
    getrandom::getrandom(&mut b).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    let t = to_hex(&b);
    write_private(&path, &t)?;
    Ok(t)
}
