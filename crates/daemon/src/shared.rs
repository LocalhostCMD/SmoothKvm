//! State shared between connection threads, the panel/control API and the terminal UI.
use serde::Serialize;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ---- per-screen connection status (controller side) ----

#[derive(Clone, Serialize)]
pub struct Status {
    /// connecting | connected | not_paired | offline | disabled
    pub state: String,
    pub detail: String,
}

static STATUS: Mutex<Option<HashMap<String, Status>>> = Mutex::new(None);
static PAIR_REQ: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// Drop control characters so remote-controlled text can never inject terminal escape sequences.
pub fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(300).collect()
}

#[allow(dead_code)]
pub fn set_status(name: &str, state: &str, detail: &str) {
    if let Ok(mut g) = STATUS.lock() {
        g.get_or_insert_with(HashMap::new)
            .insert(name.to_string(), Status { state: state.into(), detail: clean(detail) });
    }
}

#[allow(dead_code)]
pub fn statuses() -> HashMap<String, Status> {
    STATUS.lock().ok().and_then(|g| g.clone()).unwrap_or_default()
}

/// The user asked to pair with screen `name` using the PIN shown on that device.
#[allow(dead_code)]
pub fn request_pair(name: &str, pin: &str) {
    if let Ok(mut g) = PAIR_REQ.lock() {
        g.get_or_insert_with(HashMap::new).insert(name.to_string(), pin.to_string());
    }
}

#[allow(dead_code)]
pub fn pair_requested(name: &str) -> bool {
    PAIR_REQ.lock().ok().and_then(|g| g.as_ref().map(|m| m.contains_key(name))).unwrap_or(false)
}

#[allow(dead_code)]
pub fn take_pair_request(name: &str) -> Option<String> {
    PAIR_REQ.lock().ok().and_then(|mut g| g.as_mut().and_then(|m| m.remove(name)))
}

// ---- receiver side: pairing window (single-use PIN) ----

static PAIRING_UNTIL: AtomicU64 = AtomicU64::new(0);
static PAIRING_PIN: Mutex<Option<String>> = Mutex::new(None);

/// Open a pairing window and return the new 8-digit PIN (shown to the user on THIS device only).
/// Each window has its own PIN, valid for exactly one pairing attempt.
pub fn open_pairing(secs: u64) -> String {
    let mut b = [0u8; 8];
    let _ = getrandom::getrandom(&mut b);
    // 8 digits, uniformly (rejection-free enough: modulo bias on u64 is negligible)
    let n = u64::from_le_bytes(b) % 100_000_000;
    let pin = format!("{n:08}");
    if let Ok(mut g) = PAIRING_PIN.lock() {
        *g = Some(pin.clone());
    }
    PAIRING_UNTIL.store(now_secs() + secs, Ordering::SeqCst);
    pin
}

/// Close pairing and forget the PIN. Called after ANY pairing attempt (success or failure) and on expiry,
/// so a PIN that was ever exposed to a failed guess can never be used again.
pub fn burn_pairing() {
    PAIRING_UNTIL.store(0, Ordering::SeqCst);
    if let Ok(mut g) = PAIRING_PIN.lock() {
        *g = None;
    }
}

pub fn pairing_remaining() -> u64 {
    PAIRING_UNTIL.load(Ordering::SeqCst).saturating_sub(now_secs())
}

/// The PIN that is currently valid, if pairing is open.
pub fn pairing_pin() -> Option<String> {
    if pairing_remaining() == 0 {
        return None;
    }
    PAIRING_PIN.lock().ok().and_then(|g| g.clone())
}

// ---- receiver side: live sessions ----

#[derive(Clone, Serialize)]
pub struct Session {
    pub peer: String,
    pub secs: u64,
}

static SESSIONS: Mutex<Vec<(u64, String, u64)>> = Mutex::new(Vec::new());
static SESSION_ID: AtomicU64 = AtomicU64::new(1);

pub fn session_start(peer: &str) -> u64 {
    let id = SESSION_ID.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut s) = SESSIONS.lock() {
        s.push((id, clean(peer), now_secs()));
    }
    id
}

pub fn session_end(id: u64) {
    if let Ok(mut s) = SESSIONS.lock() {
        s.retain(|x| x.0 != id);
    }
}

pub fn sessions() -> Vec<Session> {
    let now = now_secs();
    SESSIONS
        .lock()
        .map(|s| s.iter().map(|(_, p, t)| Session { peer: p.clone(), secs: now.saturating_sub(*t) }).collect())
        .unwrap_or_default()
}

// ---- abuse limits for connections that have not authenticated yet ----

const MAX_PREAUTH_TOTAL: usize = 8;
const MAX_PREAUTH_PER_IP: usize = 2;

static PREAUTH_TOTAL: AtomicUsize = AtomicUsize::new(0);
static PREAUTH_IPS: Mutex<Option<HashMap<IpAddr, usize>>> = Mutex::new(None);

/// A slot for a not-yet-authenticated connection; freed on drop. Without these limits one host could
/// open many idle connections and starve real sessions.
pub struct PreauthSlot(IpAddr);

pub fn preauth_enter(ip: IpAddr) -> Option<PreauthSlot> {
    if PREAUTH_TOTAL.load(Ordering::SeqCst) >= MAX_PREAUTH_TOTAL {
        return None;
    }
    let mut g = PREAUTH_IPS.lock().ok()?;
    let map = g.get_or_insert_with(HashMap::new);
    let n = map.entry(ip).or_insert(0);
    if *n >= MAX_PREAUTH_PER_IP {
        return None;
    }
    *n += 1;
    PREAUTH_TOTAL.fetch_add(1, Ordering::SeqCst);
    Some(PreauthSlot(ip))
}

impl Drop for PreauthSlot {
    fn drop(&mut self) {
        PREAUTH_TOTAL.fetch_sub(1, Ordering::SeqCst);
        if let Ok(mut g) = PREAUTH_IPS.lock() {
            if let Some(map) = g.as_mut() {
                if let Some(n) = map.get_mut(&self.0) {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        map.remove(&self.0);
                    }
                }
            }
        }
    }
}
