//! Bounded in-process replay store for Gemini thought signatures.
//!
//! Gemini rejects a historical `functionCall` whose thought signature is not
//! replayed, so the signature has to survive a full client round-trip. Encoding
//! it into the client-visible tool call id kept the gateway stateless, but the
//! signature is an opaque reasoning blob (54 KiB on observed Antigravity
//! traffic), so every later request re-uploaded it and clients that cap tool
//! call id length silently dropped it. Signatures are parked here instead and
//! the client only ever sees the upstream call id.
//!
//! A miss is never fatal: callers fall back to Gemini's
//! `skip_thought_signature_validator` sentinel, which is also where an evicted
//! entry, an expired entry, or a gateway restart lands.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const MAX_ENTRIES: usize = 1024;
const MAX_BYTES: usize = 32 * 1024 * 1024;
const TTL: Duration = Duration::from_secs(60 * 60);

static STORE: LazyLock<Mutex<Store>> = LazyLock::new(|| Mutex::new(Store::default()));
static SYNTHETIC_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Entry {
    arguments: String,
    signature: String,
    stored_at: Instant,
}

impl Entry {
    fn weight(&self, key: &str) -> usize {
        key.len() + self.arguments.len() + self.signature.len()
    }
}

#[derive(Default)]
struct Store {
    entries: HashMap<String, Entry>,
    order: VecDeque<String>,
    bytes: usize,
}

impl Store {
    fn insert(&mut self, key: String, entry: Entry) {
        self.remove(&key);
        self.bytes += entry.weight(&key);
        self.order.push_back(key.clone());
        self.entries.insert(key, entry);
        while self.entries.len() > MAX_ENTRIES || self.bytes > MAX_BYTES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(entry.weight(&oldest));
            }
        }
    }

    fn remove(&mut self, key: &str) {
        if let Some(entry) = self.entries.remove(key) {
            self.bytes = self.bytes.saturating_sub(entry.weight(key));
        }
        if let Some(index) = self.order.iter().position(|existing| existing == key) {
            self.order.remove(index);
        }
    }
}

fn key(call_id: &str, name: &str) -> String {
    format!("{name}\u{1f}{call_id}")
}

/// `arguments` must be canonical JSON text: an id reused with different
/// arguments must not replay a stale signature.
pub fn remember(call_id: &str, name: &str, arguments: &str, signature: &str) {
    if call_id.is_empty() || signature.is_empty() {
        return;
    }
    let Ok(mut store) = STORE.lock() else {
        return;
    };
    store.insert(
        key(call_id, name),
        Entry {
            arguments: arguments.to_string(),
            signature: signature.to_string(),
            stored_at: Instant::now(),
        },
    );
}

pub fn recall(call_id: &str, name: &str, arguments: &str) -> Option<String> {
    if call_id.is_empty() {
        return None;
    }
    let mut store = STORE.lock().ok()?;
    let key = key(call_id, name);
    if store
        .entries
        .get(&key)
        .is_some_and(|entry| entry.stored_at.elapsed() > TTL)
    {
        store.remove(&key);
        return None;
    }
    let entry = store.entries.get_mut(&key)?;
    if entry.arguments != arguments {
        return None;
    }
    entry.stored_at = Instant::now();
    Some(entry.signature.clone())
}

/// Process-unique: a per-response index repeats across responses and binds
/// unrelated tool results to the same call.
pub fn synthetic_call_id(name: &str) -> String {
    let sequence = SYNTHETIC_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("call_{name}_{sequence}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parked_signature_is_recovered_for_the_same_call() {
        remember("ledger-a", "bash", "{\"command\":\"ls\"}", "SIG-A");
        assert_eq!(
            recall("ledger-a", "bash", "{\"command\":\"ls\"}").as_deref(),
            Some("SIG-A")
        );
    }

    #[test]
    fn a_reused_id_with_other_arguments_does_not_replay() {
        remember("ledger-b", "eval", "{\"offset\":10}", "SIG-B");
        assert_eq!(recall("ledger-b", "eval", "{\"offset\":20}"), None);
    }

    #[test]
    fn an_unknown_call_has_no_signature() {
        assert_eq!(recall("ledger-missing", "bash", "{}"), None);
        assert_eq!(recall("", "bash", "{}"), None);
    }

    #[test]
    fn a_different_tool_name_does_not_share_the_entry() {
        remember("ledger-c", "read", "{}", "SIG-C");
        assert_eq!(recall("ledger-c", "write", "{}"), None);
    }

    #[test]
    fn the_oldest_entries_are_evicted_past_capacity() {
        remember("ledger-evicted", "tool", "{}", "SIG-OLD");
        for index in 0..MAX_ENTRIES {
            remember(&format!("ledger-fill-{index}"), "tool", "{}", "SIG-FILL");
        }
        assert_eq!(recall("ledger-evicted", "tool", "{}"), None);
    }

    #[test]
    fn synthetic_ids_never_repeat() {
        let first = synthetic_call_id("eval");
        let second = synthetic_call_id("eval");
        assert_ne!(first, second);
        assert!(first.starts_with("call_eval_"));
    }
}
