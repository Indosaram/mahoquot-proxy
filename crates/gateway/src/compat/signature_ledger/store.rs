use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const DEFAULT_MAX_ENTRIES: usize = 1024;
pub const DEFAULT_MAX_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const TTL: Duration = Duration::from_secs(60 * 60);

pub(crate) struct Entry {
    arguments: String,
    signature: String,
    stored_at: Instant,
    seq: u64,
}

impl Entry {
    fn weight(&self, key: &str) -> usize {
        key.len() + self.arguments.len() + self.signature.len()
    }
}

/// Bounded by entries and by bytes, ordered by last *access*. The predecessor
/// was a write-ordered FIFO, so an entry a live conversation kept replaying was
/// evicted exactly like a dead one.
pub(crate) struct LruStore {
    entries: HashMap<String, Entry>,
    order: BTreeMap<u64, String>,
    bytes: usize,
    next_seq: u64,
    max_entries: usize,
    max_bytes: usize,
}

impl LruStore {
    pub(crate) fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: BTreeMap::new(),
            bytes: 0,
            next_seq: 0,
            max_entries: max_entries.max(1),
            max_bytes: max_bytes.max(1),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    #[cfg(test)]
    pub(crate) fn order_len(&self) -> usize {
        self.order.len()
    }

    #[cfg(test)]
    pub(crate) fn accounted_bytes(&self) -> usize {
        self.entries
            .iter()
            .map(|(key, entry)| entry.weight(key))
            .sum()
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    pub(crate) fn insert(
        &mut self,
        key: String,
        arguments: String,
        signature: String,
        stored_at: Instant,
    ) {
        self.remove(&key);
        let seq = self.bump();
        let entry = Entry {
            arguments,
            signature,
            stored_at,
            seq,
        };
        self.bytes += entry.weight(&key);
        self.order.insert(seq, key.clone());
        self.entries.insert(key, entry);
        self.evict();
    }

    pub(crate) fn get(&mut self, key: &str, arguments: &str, now: Instant) -> Option<String> {
        let (expired, matches) = match self.entries.get(key) {
            Some(entry) => (
                now.saturating_duration_since(entry.stored_at) > TTL,
                entry.arguments == arguments,
            ),
            None => return None,
        };
        if expired {
            self.remove(key);
            return None;
        }
        if !matches {
            return None;
        }
        let seq = self.bump();
        let entry = self.entries.get_mut(key)?;
        let previous = entry.seq;
        entry.seq = seq;
        entry.stored_at = now;
        let signature = entry.signature.clone();
        self.order.remove(&previous);
        self.order.insert(seq, key.to_string());
        Some(signature)
    }

    pub(crate) fn remove(&mut self, key: &str) {
        if let Some(entry) = self.entries.remove(key) {
            self.bytes = self.bytes.saturating_sub(entry.weight(key));
            self.order.remove(&entry.seq);
        }
    }

    /// Most-recently-used first: a bounded snapshot keeps the live
    /// conversations and drops the cold tail.
    pub(crate) fn snapshot(
        &self,
        max_entries: usize,
        max_bytes: usize,
        now: Instant,
    ) -> Vec<SnapshotRecord> {
        let mut out = Vec::new();
        let mut bytes = 0_usize;
        for key in self.order.values().rev() {
            let Some(entry) = self.entries.get(key) else {
                continue;
            };
            let age = now.saturating_duration_since(entry.stored_at);
            if age > TTL {
                continue;
            }
            let weight = entry.weight(key);
            if out.len() >= max_entries || bytes + weight > max_bytes {
                break;
            }
            bytes += weight;
            out.push(SnapshotRecord {
                key: key.clone(),
                arguments: entry.arguments.clone(),
                signature: entry.signature.clone(),
                age_ms: age.as_millis() as u64,
            });
        }
        out
    }

    pub(crate) fn restore(&mut self, records: Vec<SnapshotRecord>, now: Instant) {
        for record in records.into_iter().rev() {
            let age = Duration::from_millis(record.age_ms);
            if age > TTL {
                continue;
            }
            let stored_at = now.checked_sub(age).unwrap_or(now);
            self.insert(record.key, record.arguments, record.signature, stored_at);
        }
    }

    fn bump(&mut self) -> u64 {
        self.next_seq = self.next_seq.wrapping_add(1);
        self.next_seq
    }

    fn evict(&mut self) {
        while self.entries.len() > self.max_entries || self.bytes > self.max_bytes {
            let victim = match self.order.values().next() {
                Some(key) => key.clone(),
                None => break,
            };
            self.remove(&victim);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SnapshotRecord {
    pub(crate) key: String,
    pub(crate) arguments: String,
    pub(crate) signature: String,
    /// `Instant` has no cross-process meaning, so an entry travels as the age
    /// it had at snapshot time and resumes with its remaining TTL.
    pub(crate) age_ms: u64,
}
