//! On-disk state: config, this device's identity key, trusted peers, panel token.
use kvm_protocol::secure::{from_hex, generate_keypair, to_hex};
use kvm_protocol::Config;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

pub fn config_dir() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    base.unwrap_or_else(|| PathBuf::from(".")).join("softwarekvm")
}

/// Write a file that only the current user may read (secrets).
fn write_private(path: &PathBuf, contents: &str) -> io::Result<()> {
    fs::create_dir_all(path.parent().unwrap_or(&PathBuf::from(".")))?;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(contents.as_bytes())
}

pub struct Identity {
    pub private: Vec<u8>,
    pub public: Vec<u8>,
}

impl Identity {
    pub fn load_or_create() -> io::Result<Self> {
        let path = config_dir().join("identity.key");
        if let Ok(text) = fs::read_to_string(&path) {
            let mut lines = text.lines();
            if let (Some(a), Some(b)) = (lines.next(), lines.next()) {
                if let (Some(private), Some(public)) = (from_hex(a), from_hex(b)) {
                    return Ok(Self { private, public });
                }
            }
        }
        let (private, public) = generate_keypair()?;
        write_private(&path, &format!("{}\n{}\n", to_hex(&private), to_hex(&public)))?;
        Ok(Self { private, public })
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

pub fn save_config(cfg: &Config) -> io::Result<()> {
    let text = toml::to_string_pretty(cfg).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    fs::create_dir_all(config_dir())?;
    fs::write(config_path(), text)
}

/// Random secret that the local config panel requires on every API call.
pub fn panel_token() -> io::Result<String> {
    let path = config_dir().join("panel.token");
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
