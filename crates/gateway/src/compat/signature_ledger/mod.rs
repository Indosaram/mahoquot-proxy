//! Instance-owned replay store for Gemini thought signatures.
//!
//! Gemini rejects a historical `functionCall` whose thought signature is not
//! replayed, so the signature has to survive a full client round-trip. It is an
//! opaque reasoning blob (54 KiB on observed Antigravity traffic), so it is
//! parked here instead of being encoded into the client-visible tool call id.
//!
//! A miss is never fatal: callers fall back to Gemini's
//! `skip_thought_signature_validator` sentinel, which is also where an evicted
//! or expired entry lands.

mod persist;
mod store;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use store::LruStore;
pub use store::{DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES};

pub const DEFAULT_SNAPSHOT_FILE: &str = "gemini-signature-ledger.json";
const DEFAULT_SNAPSHOT_MAX_ENTRIES: usize = 512;
const DEFAULT_SNAPSHOT_MAX_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_COALESCE_WINDOW: Duration = Duration::from_millis(750);

#[derive(Clone, Debug)]
pub struct LedgerConfig {
    pub snapshot_path: Option<PathBuf>,
    pub max_entries: usize,
    pub max_bytes: usize,
    pub snapshot_max_entries: usize,
    pub snapshot_max_bytes: usize,
    /// Writes inside this window after the first dirtying write collapse into
    /// a single snapshot; the disk never sees per-tool-call traffic.
    pub coalesce_window: Duration,
}

impl Default for LedgerConfig {
    fn default() -> Self {
        Self {
            snapshot_path: None,
            max_entries: DEFAULT_MAX_ENTRIES,
            max_bytes: DEFAULT_MAX_BYTES,
            snapshot_max_entries: DEFAULT_SNAPSHOT_MAX_ENTRIES,
            snapshot_max_bytes: DEFAULT_SNAPSHOT_MAX_BYTES,
            coalesce_window: DEFAULT_COALESCE_WINDOW,
        }
    }
}

pub struct SignatureLedger {
    store: Mutex<LruStore>,
    config: LedgerConfig,
    dirty: AtomicBool,
    writes: AtomicU64,
    persists: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    unsigned_replays: AtomicU64,
    wake: tokio::sync::Notify,
    persisted: tokio::sync::Notify,
    shutdown: AtomicBool,
}

/// Counters a monitoring surface can read without touching the store itself:
/// `misses` counts replayed calls whose signature was no longer held, and
/// `unsigned_replays` counts the turns that went upstream carrying the
/// `skip_thought_signature_validator` sentinel instead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LedgerStats {
    pub entries: usize,
    pub bytes: usize,
    pub writes: u64,
    pub persists: u64,
    pub hits: u64,
    pub misses: u64,
    pub unsigned_replays: u64,
}

impl SignatureLedger {
    pub fn in_memory() -> Arc<Self> {
        Self::with_config(LedgerConfig::default())
    }

    /// Startup-only: reads the snapshot synchronously before the runtime takes
    /// requests, so a restart answers the first replay from the same entries.
    pub fn open(snapshot_path: impl Into<PathBuf>) -> Arc<Self> {
        Self::with_config(LedgerConfig {
            snapshot_path: Some(snapshot_path.into()),
            ..LedgerConfig::default()
        })
    }

    pub fn with_config(config: LedgerConfig) -> Arc<Self> {
        let mut store = LruStore::new(config.max_entries, config.max_bytes);
        if let Some(path) = config.snapshot_path.as_deref() {
            store.restore(persist::load(path), Instant::now());
        }
        Arc::new(Self {
            store: Mutex::new(store),
            config,
            dirty: AtomicBool::new(false),
            writes: AtomicU64::new(0),
            persists: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            unsigned_replays: AtomicU64::new(0),
            wake: tokio::sync::Notify::new(),
            persisted: tokio::sync::Notify::new(),
            shutdown: AtomicBool::new(false),
        })
    }

    pub fn scope(self: &Arc<Self>, model: &str, session_id: &str) -> ReplayScope {
        ReplayScope::new(Arc::clone(self), model, session_id)
    }

    pub fn len(&self) -> usize {
        self.store.lock().map(|store| store.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn bytes(&self) -> usize {
        self.store.lock().map(|store| store.bytes()).unwrap_or(0)
    }

    pub fn write_count(&self) -> u64 {
        self.writes.load(Ordering::Relaxed)
    }

    pub fn persist_count(&self) -> u64 {
        self.persists.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> LedgerStats {
        LedgerStats {
            entries: self.len(),
            bytes: self.bytes(),
            writes: self.write_count(),
            persists: self.persist_count(),
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            unsigned_replays: self.unsigned_replays.load(Ordering::Relaxed),
        }
    }

    pub fn snapshot_path(&self) -> Option<&Path> {
        self.config.snapshot_path.as_deref()
    }

    fn remember(&self, key: String, arguments: String, signature: String) {
        if let Ok(mut store) = self.store.lock() {
            store.insert(key, arguments, signature, Instant::now());
        }
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.dirty.store(true, Ordering::Release);
        self.wake.notify_one();
    }

    fn recall(&self, key: &str, arguments: &str) -> Option<String> {
        let mut store = self.store.lock().ok()?;
        store.get(key, arguments, Instant::now())
    }

    /// Serializes and writes the snapshot on the calling thread. Startup,
    /// shutdown, and tests use it; request handling must not.
    pub fn flush_blocking(&self) -> io::Result<()> {
        let Some(path) = self.config.snapshot_path.as_deref() else {
            self.dirty.store(false, Ordering::Release);
            return Ok(());
        };
        self.dirty.store(false, Ordering::Release);
        let records = {
            let Ok(store) = self.store.lock() else {
                return Ok(());
            };
            store.snapshot(
                self.config.snapshot_max_entries,
                self.config.snapshot_max_bytes,
                Instant::now(),
            )
        };
        let result = persist::store(path, &records);
        if result.is_ok() {
            self.persists.fetch_add(1, Ordering::Relaxed);
            self.persisted.notify_waiters();
        } else {
            self.dirty.store(true, Ordering::Release);
        }
        result
    }

    pub async fn flush(self: &Arc<Self>) -> io::Result<()> {
        let ledger = Arc::clone(self);
        match tokio::task::spawn_blocking(move || ledger.flush_blocking()).await {
            Ok(result) => result,
            Err(err) => Err(io::Error::other(err)),
        }
    }

    /// Awaits the next completed snapshot write. Lets a caller observe the
    /// worker by event instead of polling for the file to appear.
    pub async fn persisted(&self) {
        self.persisted.notified().await;
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.wake.notify_one();
    }

    /// One task per ledger: waits for a dirtying write, lets the coalescing
    /// window absorb the rest of the burst, then writes once off the hotpath.
    pub fn spawn_persistence_worker(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let ledger = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                ledger.wake.notified().await;
                if ledger.shutdown.load(Ordering::Acquire) {
                    let _ = ledger.flush().await;
                    return;
                }
                if !ledger.dirty.load(Ordering::Acquire) {
                    continue;
                }
                if !ledger.config.coalesce_window.is_zero() {
                    tokio::time::sleep(ledger.config.coalesce_window).await;
                }
                let _ = ledger.flush().await;
            }
        })
    }
}

/// Model + session scope for one conversation's replay traffic. A signature
/// parked by one model or session is never replayed into another: Gemini
/// validates it against the reasoning context that produced it.
#[derive(Clone)]
pub struct ReplayScope {
    ledger: Arc<SignatureLedger>,
    model: String,
    session_id: String,
}

impl ReplayScope {
    pub fn new(ledger: Arc<SignatureLedger>, model: &str, session_id: &str) -> Self {
        Self {
            ledger,
            model: model.to_string(),
            session_id: session_id.to_string(),
        }
    }

    pub fn ledger(&self) -> &Arc<SignatureLedger> {
        &self.ledger
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn remember(&self, call_id: &str, name: &str, arguments: &str, signature: &str) {
        if call_id.is_empty() || signature.is_empty() {
            return;
        }
        self.ledger.remember(
            self.key(call_id, name),
            canonical_arguments(arguments),
            signature.to_string(),
        );
    }

    pub fn recall(&self, call_id: &str, name: &str, arguments: &str) -> Option<String> {
        if call_id.is_empty() {
            return None;
        }
        let found = self
            .ledger
            .recall(&self.key(call_id, name), &canonical_arguments(arguments));
        let counter = if found.is_some() {
            &self.ledger.hits
        } else {
            &self.ledger.misses
        };
        counter.fetch_add(1, Ordering::Relaxed);
        found
    }

    /// A replayed call had no signature to reuse, so the request goes upstream
    /// with the `skip_thought_signature_validator` sentinel.
    pub fn record_unsigned_replay(&self) {
        self.ledger
            .unsigned_replays
            .fetch_add(1, Ordering::Relaxed);
    }

    fn key(&self, call_id: &str, name: &str) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{name}\u{1f}{call_id}",
            self.model, self.session_id
        )
    }
}

/// Two spellings of the same arguments object must hit the same entry, while a
/// genuinely different object must miss. `serde_json::Map` is key-ordered, so
/// a reparse-and-print is the canonical form.
fn canonical_arguments(arguments: &str) -> String {
    match serde_json::from_str::<Value>(arguments) {
        Ok(value) => value.to_string(),
        Err(_) => arguments.to_string(),
    }
}

/// Process-unique: a per-response index repeats across responses and binds
/// unrelated tool results to the same call.
pub fn synthetic_call_id(name: &str) -> String {
    format!("call_{name}_{}", uuid::Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests;
