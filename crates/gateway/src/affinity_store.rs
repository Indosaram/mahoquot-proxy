//! Durable Claude OAuth session -> account bindings.
//!
//! The router's own affinity map lives in memory and rebinds on any unhealthy
//! moment, so a restart or a single cooldown moves a conversation to another
//! account and rewrites its whole prompt cache there. This store survives
//! restarts and is keyed by a build-stable digest.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

pub const STORE_FILE: &str = "claude-affinity.store";
pub const BINDING_TTL_SECS: i64 = 6 * 3600;
pub const MAX_BINDINGS: usize = 20_000;

/// 64-bit FNV-1a. Stable across builds and platforms, unlike `DefaultHasher`,
/// which is why persisted keys use it.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Digest of one tagged key source, so a header value and a body value that
/// happen to be equal cannot collide.
pub fn digest(tag: &str, material: &[u8]) -> u64 {
    let mut buf = Vec::with_capacity(tag.len() + 1 + material.len());
    buf.extend_from_slice(tag.as_bytes());
    buf.push(0);
    buf.extend_from_slice(material);
    fnv1a64(&buf)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub account_id: String,
    pub last_used_unix: i64,
}

#[derive(Debug, Default)]
pub struct AffinityStore {
    path: Option<PathBuf>,
    map: Mutex<HashMap<u64, Binding>>,
    dirty: AtomicBool,
}

impl AffinityStore {
    /// Load bindings from `path`, dropping entries idle past the TTL. A missing
    /// or unreadable file starts empty: losing bindings costs one cache
    /// rewrite per session, never a failed request.
    pub fn load(path: PathBuf, now_unix: i64) -> Self {
        let map = std::fs::read(&path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<HashMap<String, Binding>>(&raw).ok())
            .map(|raw| {
                raw.into_iter()
                    .filter_map(|(key, binding)| Some((u64::from_str_radix(&key, 16).ok()?, binding)))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        let store = Self {
            path: Some(path),
            map: Mutex::new(map),
            dirty: AtomicBool::new(false),
        };
        store.prune(now_unix);
        store
    }

    pub fn in_memory() -> Self {
        Self::default()
    }

    pub fn get(&self, key: u64, now_unix: i64) -> Option<String> {
        let map = self.map.lock().unwrap_or_else(|p| p.into_inner());
        map.get(&key)
            .filter(|binding| now_unix - binding.last_used_unix <= BINDING_TTL_SECS)
            .map(|binding| binding.account_id.clone())
    }

    pub fn put(&self, key: u64, account_id: &str, now_unix: i64) {
        let mut map = self.map.lock().unwrap_or_else(|p| p.into_inner());
        map.insert(
            key,
            Binding {
                account_id: account_id.to_string(),
                last_used_unix: now_unix,
            },
        );
        if map.len() > MAX_BINDINGS {
            Self::trim(&mut map, now_unix);
        }
        self.dirty.store(true, Ordering::Release);
    }

    /// Refresh the idle timer only while the binding still names this
    /// account. A one-off failover must not steal the durable binding.
    pub fn touch(&self, key: u64, account_id: &str, now_unix: i64) {
        let mut map = self.map.lock().unwrap_or_else(|p| p.into_inner());
        let Some(binding) = map.get_mut(&key) else {
            return;
        };
        if binding.account_id != account_id {
            return;
        }
        binding.last_used_unix = now_unix;
        self.dirty.store(true, Ordering::Release);
    }

    /// Number of live bindings per account, for the console.
    pub fn counts(&self, now_unix: i64) -> HashMap<String, usize> {
        let map = self.map.lock().unwrap_or_else(|p| p.into_inner());
        let mut counts = HashMap::new();
        for binding in map.values() {
            if now_unix - binding.last_used_unix <= BINDING_TTL_SECS {
                *counts.entry(binding.account_id.clone()).or_insert(0) += 1;
            }
        }
        counts
    }

    pub fn len(&self) -> usize {
        self.map.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn prune(&self, now_unix: i64) {
        let mut map = self.map.lock().unwrap_or_else(|p| p.into_inner());
        Self::trim(&mut map, now_unix);
    }

    fn trim(map: &mut HashMap<u64, Binding>, now_unix: i64) {
        map.retain(|_, binding| now_unix - binding.last_used_unix <= BINDING_TTL_SECS);
        if map.len() > MAX_BINDINGS {
            let mut ages: Vec<i64> = map.values().map(|b| b.last_used_unix).collect();
            ages.sort_unstable();
            let cutoff = ages[ages.len() - MAX_BINDINGS];
            map.retain(|_, binding| binding.last_used_unix >= cutoff);
        }
    }

    /// Serialize the live map when it changed since the last write. Returns
    /// the bytes to write, or None when nothing is dirty or there is no file.
    fn snapshot_if_dirty(&self, now_unix: i64) -> Option<(PathBuf, Vec<u8>)> {
        let path = self.path.clone()?;
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return None;
        }
        let mut map = self.map.lock().unwrap_or_else(|p| p.into_inner());
        Self::trim(&mut map, now_unix);
        let raw: HashMap<String, &Binding> = map
            .iter()
            .map(|(key, binding)| (format!("{key:016x}"), binding))
            .collect();
        let bytes = serde_json::to_vec(&raw).ok()?;
        Some((path, bytes))
    }

    /// Synchronous write, for shutdown and tests. Request paths never call it.
    pub fn flush_blocking(&self, now_unix: i64) -> std::io::Result<()> {
        match self.snapshot_if_dirty(now_unix) {
            Some((path, bytes)) => write_private(&path, &bytes),
            None => Ok(()),
        }
    }

    /// Write at most every `every` while dirty, off the request path.
    pub fn spawn_persistence(
        store: std::sync::Arc<Self>,
        every: std::time::Duration,
        shutdown: std::sync::Arc<tokio::sync::Notify>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(every);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let stopping = tokio::select! {
                    _ = shutdown.notified() => true,
                    _ = ticker.tick() => false,
                };
                if let Some((path, bytes)) = store.snapshot_if_dirty(unix_now()) {
                    let result =
                        tokio::task::spawn_blocking(move || write_private(&path, &bytes)).await;
                    match result {
                        Ok(Err(error)) => tracing::warn!(%error, "failed to persist claude affinity"),
                        Err(error) => tracing::warn!(%error, "claude affinity persistence task failed"),
                        Ok(Ok(())) => {}
                    }
                }
                if stopping {
                    break;
                }
            }
        })
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Temp file + rename, created 0600: the file maps sessions to accounts.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mahoquot-affinity-{tag}-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        dir.join(STORE_FILE)
    }

    #[test]
    fn fnv_digest_is_a_fixed_constant() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn bindings_survive_a_reload() {
        let path = temp_path("roundtrip");
        let store = AffinityStore::load(path.clone(), 1_000);
        store.put(42, "claude-a", 1_000);
        store.flush_blocking(1_000).expect("flush");
        drop(store);

        let reloaded = AffinityStore::load(path.clone(), 1_100);
        assert_eq!(reloaded.get(42, 1_100).as_deref(), Some("claude-a"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn entries_idle_past_the_ttl_are_dropped_on_load() {
        let path = temp_path("ttl");
        let store = AffinityStore::load(path.clone(), 0);
        store.put(1, "old", 0);
        store.put(2, "fresh", BINDING_TTL_SECS);
        store.flush_blocking(BINDING_TTL_SECS).expect("flush");

        let reloaded = AffinityStore::load(path.clone(), BINDING_TTL_SECS + 10);
        assert_eq!(reloaded.get(1, BINDING_TTL_SECS + 10), None);
        assert_eq!(
            reloaded.get(2, BINDING_TTL_SECS + 10).as_deref(),
            Some("fresh")
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn touch_refreshes_an_active_binding() {
        let store = AffinityStore::in_memory();
        store.put(7, "claude-a", 0);
        let later = BINDING_TTL_SECS + 100;
        store.touch(7, "claude-a", later);
        // Without the refresh this entry would be past its idle TTL.
        assert_eq!(store.get(7, later).as_deref(), Some("claude-a"));
    }

    #[test]
    fn touch_never_steals_a_binding_from_another_account() {
        let store = AffinityStore::in_memory();
        store.put(8, "claude-a", 0);
        store.touch(8, "claude-b", 10);
        assert_eq!(store.get(8, 10).as_deref(), Some("claude-a"));
    }

    #[test]
    fn touch_ignores_an_unknown_binding() {
        let store = AffinityStore::in_memory();
        store.touch(9, "claude-a", 5);
        assert_eq!(store.get(9, 5), None);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn the_cap_drops_the_oldest_entries() {
        let store = AffinityStore::in_memory();
        for i in 0..(MAX_BINDINGS as u64 + 5) {
            store.put(i, "acct", i as i64);
        }
        assert_eq!(store.len(), MAX_BINDINGS);
        assert_eq!(store.get(0, MAX_BINDINGS as i64), None);
        assert!(store.get(MAX_BINDINGS as u64 + 4, MAX_BINDINGS as i64).is_some());
    }

    #[test]
    fn a_clean_store_writes_nothing() {
        let path = temp_path("clean");
        let store = AffinityStore::load(path.clone(), 0);
        store.flush_blocking(0).expect("flush");
        assert!(!path.exists());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
