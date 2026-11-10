//! State shared between connection threads, the panel/control API and the terminal UI.
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ---- per-screen connection status (sender side) ----

#[derive(Clone, Serialize)]
pub struct Status {
    /// connecting | connected | not_paired | offline | disabled
    pub state: String,
    pub detail: String,
}

static STATUS: Mutex<Option<HashMap<String, Status>>> = Mutex::new(None);
static PAIR_REQ: Mutex<Option<HashSet<String>>> = Mutex::new(None);

#[allow(dead_code)]
pub fn set_status(name: &str, state: &str, detail: &str) {
    if let Ok(mut g) = STATUS.lock() {
        g.get_or_insert_with(HashMap::new)
            .insert(name.to_string(), Status { state: state.into(), detail: detail.into() });
    }
}

#[allow(dead_code)]
pub fn statuses() -> HashMap<String, Status> {
    STATUS.lock().ok().and_then(|g| g.clone()).unwrap_or_default()
}

#[allow(dead_code)]
pub fn request_pair(name: &str) {
    if let Ok(mut g) = PAIR_REQ.lock() {
        g.get_or_insert_with(HashSet::new).insert(name.to_string());
    }
}

#[allow(dead_code)]
pub fn pair_requested(name: &str) -> bool {
    PAIR_REQ.lock().ok().and_then(|g| g.as_ref().map(|s| s.contains(name))).unwrap_or(false)
}

#[allow(dead_code)]
pub fn take_pair_request(name: &str) -> bool {
    PAIR_REQ.lock().ok().and_then(|mut g| g.as_mut().map(|s| s.remove(name))).unwrap_or(false)
}

// ---- receiver side: pairing window and live sessions ----

static PAIRING_UNTIL: AtomicU64 = AtomicU64::new(0);

/// Accept new, unknown devices for the next `secs` seconds.
pub fn open_pairing(secs: u64) {
    PAIRING_UNTIL.store(now_secs() + secs, Ordering::SeqCst);
}

pub fn close_pairing() {
    PAIRING_UNTIL.store(0, Ordering::SeqCst);
}

pub fn pairing_remaining() -> u64 {
    PAIRING_UNTIL.load(Ordering::SeqCst).saturating_sub(now_secs())
}

pub fn pairing_open() -> bool {
    pairing_remaining() > 0
}

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
        s.push((id, peer.to_string(), now_secs()));
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

// ---- pairing confirmation shown in the panel / terminal UI ----

#[derive(Clone, Serialize)]
pub struct PendingPair {
    pub name: String,
    pub sas: String,
}

struct PairSlot {
    pending: Option<PendingPair>,
    answer: Option<bool>,
}

static PAIR: Mutex<PairSlot> = Mutex::new(PairSlot { pending: None, answer: None });
static PAIR_CV: Condvar = Condvar::new();

/// Show the code in the panel/TUI and wait (up to 2 minutes) for the user's answer.
pub fn prompt_pair(name: &str, sas: &str) -> bool {
    let Ok(mut g) = PAIR.lock() else { return false };
    g.pending = Some(PendingPair { name: name.into(), sas: sas.into() });
    g.answer = None;
    let deadline = Instant::now() + Duration::from_secs(120);
    let answer = loop {
        if let Some(a) = g.answer.take() {
            break a;
        }
        let now = Instant::now();
        if now >= deadline {
            break false;
        }
        g = match PAIR_CV.wait_timeout(g, deadline - now) {
            Ok((g, _)) => g,
            Err(_) => return false,
        };
    };
    g.pending = None;
    answer
}

pub fn pending_pair() -> Option<PendingPair> {
    PAIR.lock().ok().and_then(|g| g.pending.clone())
}

pub fn answer_pair(accept: bool) {
    if let Ok(mut g) = PAIR.lock() {
        if g.pending.is_some() {
            g.answer = Some(accept);
            PAIR_CV.notify_all();
        }
    }
}
