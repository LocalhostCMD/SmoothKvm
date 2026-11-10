//! State shared between the connection threads and the panel (Windows daemon).
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Serialize)]
pub struct Status {
    /// connecting | connected | not_paired | offline | disabled
    pub state: String,
    pub detail: String,
}

static STATUS: Mutex<Option<HashMap<String, Status>>> = Mutex::new(None);
static PAIR_REQ: Mutex<Option<HashSet<String>>> = Mutex::new(None);

pub fn set_status(name: &str, state: &str, detail: &str) {
    if let Ok(mut g) = STATUS.lock() {
        g.get_or_insert_with(HashMap::new)
            .insert(name.to_string(), Status { state: state.into(), detail: detail.into() });
    }
}

pub fn statuses() -> HashMap<String, Status> {
    STATUS.lock().ok().and_then(|g| g.clone()).unwrap_or_default()
}

pub fn request_pair(name: &str) {
    if let Ok(mut g) = PAIR_REQ.lock() {
        g.get_or_insert_with(HashSet::new).insert(name.to_string());
    }
}

pub fn pair_requested(name: &str) -> bool {
    PAIR_REQ.lock().ok().and_then(|g| g.as_ref().map(|s| s.contains(name))).unwrap_or(false)
}

pub fn take_pair_request(name: &str) -> bool {
    PAIR_REQ.lock().ok().and_then(|mut g| g.as_mut().map(|s| s.remove(name))).unwrap_or(false)
}

// ---- pairing confirmation shown in the panel ----

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

/// Show the code in the panel and wait (up to 2 minutes) for the user's answer.
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
