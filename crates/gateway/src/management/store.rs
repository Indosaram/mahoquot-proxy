use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;

use super::settings::{Settings, SettingsError};

pub type SnapshotProvider = Arc<dyn Fn() -> Arc<mahoquot_registry::RegistrySnapshot> + Send + Sync>;
pub type PoolPublisher = Arc<
    dyn Fn(Arc<mahoquot_registry::RegistrySnapshot>) -> Result<(), anyhow::Error> + Send + Sync,
>;
/// Notified with every newly published document so in-memory projections of
/// the settings (scoped-key index, and anything added later) rebuild in one
/// place instead of at each mutation call site.
pub type SettingsObserver = Arc<dyn Fn(&Settings) + Send + Sync>;

/// Holds the live settings document and swaps it atomically.
///
/// The relay hot path reads settings on every request under a p50 latency
/// gate, so readers must never contend: `current()` is a lock-free atomic load
/// and mutation publishes a whole new `Arc` rather than editing in place. A
/// `RwLock` here would put every proxied request behind a writer.
pub struct SettingsStore {
    current: ArcSwap<Settings>,
    path: PathBuf,
    /// Serializes read-modify-write cycles so concurrent setting changes
    /// cannot silently drop each other's edits.
    mutate_lock: std::sync::Mutex<()>,
    snapshot_provider: std::sync::RwLock<Option<SnapshotProvider>>,
    pool_publisher: std::sync::RwLock<Option<PoolPublisher>>,
    observers: std::sync::RwLock<Vec<SettingsObserver>>,
}

impl std::fmt::Debug for SettingsStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsStore")
            .field("current", &self.current)
            .field("path", &self.path)
            .finish()
    }
}

impl SettingsStore {
    pub fn new(settings: Settings, path: PathBuf) -> Self {
        Self {
            current: ArcSwap::from_pointee(settings),
            path,
            mutate_lock: std::sync::Mutex::new(()),
            snapshot_provider: std::sync::RwLock::new(None),
            pool_publisher: std::sync::RwLock::new(None),
            observers: std::sync::RwLock::new(Vec::new()),
        }
    }

    /// Load from `path`, falling back to the supplied environment-derived
    /// document when the file is absent. A file that exists but does not parse
    /// is an error rather than a silent fallback, so a typo in config.yaml can
    /// never be mistaken for "no config" and quietly revert live settings.
    pub fn load_or(path: PathBuf, fallback: Settings) -> Result<Self, SettingsError> {
        if path.exists() {
            let settings = Settings::load(&path)?;
            let store = Self::new(settings, path);
            let snapshot = store.active_snapshot();
            store.current().validate_against_registry(&snapshot)?;
            return Ok(store);
        }
        // Upstream always has a config file, so `GET /config.yaml` never 404s
        // there. Materialise the boot document once so the served surface
        // matches instead of depending on a write having happened first.
        //
        // An empty path means no config file was requested at all (embedding
        // and tests construct the gateway this way), and writing to it would
        // fail; such a store stays purely in memory.
        let store = Self::new(fallback, path);
        if !store.path.as_os_str().is_empty() {
            store.mutate(|_| {})?;
        }
        Ok(store)
    }

    pub fn current(&self) -> Arc<Settings> {
        self.current.load_full()
    }

    pub(crate) fn composition_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.mutate_lock.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn active_snapshot(&self) -> Arc<mahoquot_registry::RegistrySnapshot> {
        if let Some(ref provider) = *self.snapshot_provider.read().unwrap() {
            return provider();
        }
        Arc::new(
            mahoquot_registry::embedded_registry_snapshot()
                .expect("embedded snapshot must be valid"),
        )
    }

    pub fn set_snapshot_provider(&self, provider: SnapshotProvider) {
        *self.snapshot_provider.write().unwrap() = Some(provider);
    }

    pub fn set_pool_publisher(&self, publisher: PoolPublisher) {
        *self.pool_publisher.write().unwrap() = Some(publisher);
    }

    pub fn add_observer(&self, observer: SettingsObserver) {
        self.observers.write().unwrap().push(observer);
    }

    fn notify_observers(&self, settings: &Settings) {
        for observer in self.observers.read().unwrap().iter() {
            observer(settings);
        }
    }

    /// Re-read the document from disk, validate it atomically against active registry, and publish it.
    pub fn reload(&self) -> Result<Arc<Settings>, SettingsError> {
        // Same read-modify-publish shape as `mutate`, so it takes the same lock:
        // without it a reload can land between a mutation's persist and its
        // publish and drop that edit from the live document.
        let _write = self
            .mutate_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let settings = Settings::load(&self.path)?;
        let active_snapshot = self.active_snapshot();
        let candidate_registry = settings.validate_against_registry(&active_snapshot)?;

        let published = Arc::new(settings);
        self.current.store(published.clone());
        self.notify_observers(&published);

        if let Some(ref publisher) = *self.pool_publisher.read().unwrap() {
            if let Err(err) = publisher(Arc::new(candidate_registry)) {
                tracing::error!("failed to publish candidate registry on reload: {err}");
            }
        }

        Ok(published)
    }

    /// Apply `edit` to a copy of the live document, validate it against active registry, persist it, then publish.
    ///
    /// Persisting before publishing means a failed write leaves the in-memory
    /// document untouched, so the API never reports a change it did not manage
    /// to save.
    pub fn mutate<F>(&self, edit: F) -> Result<Arc<Settings>, SettingsError>
    where
        F: FnOnce(&mut Settings),
    {
        let _write = self
            .mutate_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut next = Settings::clone(&self.current());
        edit(&mut next);

        let active_snapshot = self.active_snapshot();
        let candidate_registry = next.validate_against_registry(&active_snapshot)?;

        next.persist(&self.path)?;
        let published = Arc::new(next);
        self.current.store(published.clone());
        self.notify_observers(&published);

        if let Some(ref publisher) = *self.pool_publisher.read().unwrap() {
            if let Err(err) = publisher(Arc::new(candidate_registry)) {
                tracing::error!("failed to publish candidate registry on mutate: {err}");
            }
        }

        Ok(published)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::settings::{
        WarmupAccountPolicy, WarmupProviderPolicy,
    };

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mahoquot-store-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }


    #[test]
    fn a_present_file_wins_over_the_env_fallback() {
        // given a config.yaml on disk and a different env-derived fallback
        let dir = temp_dir("precedence");
        let path = dir.join("config.yaml");
        std::fs::write(&path, "port: 19001\n").expect("write");
        let fallback = Settings {
            port: 18801,
            ..Settings::default()
        };
        // when the store loads
        let store = SettingsStore::load_or(path, fallback).expect("loads");
        // then the file's value is live
        assert_eq!(store.current().port, 19001);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_env_fallback_is_used_when_no_file_exists() {
        // given no config.yaml
        let dir = temp_dir("fallback");
        let path = dir.join("config.yaml");
        let fallback = Settings {
            port: 18842,
            ..Settings::default()
        };
        // when the store loads
        let store = SettingsStore::load_or(path, fallback).expect("loads");
        // then the environment-derived document is live
        assert_eq!(store.current().port, 18842);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unparsable_file_is_an_error_not_a_silent_fallback() {
        // given a corrupt config.yaml
        let dir = temp_dir("corrupt");
        let path = dir.join("config.yaml");
        std::fs::write(&path, "port: [this is not a number\n").expect("write");
        // when the store loads
        let result = SettingsStore::load_or(path, Settings::default());
        // then it refuses rather than reverting to defaults
        assert!(result.is_err(), "corrupt config must not load silently");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mutate_persists_and_publishes() {
        // given a store backed by a file
        let dir = temp_dir("mutate");
        let path = dir.join("config.yaml");
        let store = SettingsStore::load_or(path.clone(), Settings::default()).expect("loads");
        // when a field is mutated
        store.mutate(|s| s.request_retry = 9).expect("mutates");
        // then the live document reflects it
        assert_eq!(store.current().request_retry, 9);
        // and so does the file on disk
        let on_disk = Settings::load(&path).expect("reloads");
        assert_eq!(on_disk.request_retry, 9);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reload_picks_up_an_external_edit() {
        // given a store whose file is changed behind its back
        let dir = temp_dir("reload");
        let path = dir.join("config.yaml");
        let store = SettingsStore::load_or(path.clone(), Settings::default()).expect("loads");
        assert_eq!(store.current().request_retry, 0);
        std::fs::write(&path, "request-retry: 4\n").expect("write");
        // when reload runs
        store.reload().expect("reloads");
        // then the new value is live without a restart
        assert_eq!(store.current().request_retry, 4);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failed_persist_leaves_the_live_document_unchanged() {
        // given a store whose path is not writable (parent is a file)
        let dir = temp_dir("failwrite");
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, "x").expect("write");
        let path = blocker.join("config.yaml");
        let store = SettingsStore::new(Settings::default(), path);
        // when a mutation cannot be saved
        let result = store.mutate(|s| s.request_retry = 5);
        // then it reports the failure and does not publish the change
        assert!(result.is_err(), "write into a file-as-directory must fail");
        assert_eq!(store.current().request_retry, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn readers_observe_a_consistent_document_across_concurrent_mutations() {
        // given a store under concurrent readers and writers
        let dir = temp_dir("concurrent");
        let path = dir.join("config.yaml");
        let store = Arc::new(SettingsStore::load_or(path, Settings::default()).expect("loads"));
        let writer = {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                for i in 1..=25 {
                    store
                        .mutate(|s| {
                            // both fields move together; a reader must never
                            // see one updated without the other
                            s.request_retry = i;
                            s.max_retry_interval = i * 2;
                        })
                        .expect("mutates");
                }
            })
        };
        let reader = {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                for _ in 0..500 {
                    let snapshot = store.current();
                    assert_eq!(
                        snapshot.max_retry_interval,
                        snapshot.request_retry * 2,
                        "torn read: fields from different generations"
                    );
                }
            })
        };
        // then no reader ever observes a torn document
        writer.join().expect("writer");
        reader.join().expect("reader");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_warmup_settings_mutate_persistence_and_reload() {
        // given a store with default settings
        let dir = temp_dir("warmup_roundtrip");
        let path = dir.join("config.yaml");
        let store = SettingsStore::load_or(path.clone(), Settings::default()).expect("loads");

        // when provider and account warmup policies are mutated
        store
            .mutate(|s| {
                s.warmup.providers.insert(
                    "codex".to_string(),
                    WarmupProviderPolicy {
                        enabled: true,
                        model: Some("gpt-5.6-sol".to_string()),
                        idle_secs: 1800,
                        min_interval_secs: 120,
                    },
                );
                s.warmup.accounts.insert(
                    "ag-acct-2".to_string(),
                    WarmupAccountPolicy::Custom {
                        model: Some("gemini-3.7-flash-high".to_string()),
                        idle_secs: 1800,
                        min_interval_secs: 120,
                    },
                );
                s.warmup.accounts.insert(
                    "codex-acct-1".to_string(),
                    WarmupAccountPolicy::Inherit,
                );
                s.warmup.accounts.insert(
                    "generic-cline-1".to_string(),
                    WarmupAccountPolicy::Off,
                );
            })
            .expect("mutates");

        // then an independent store reload from disk produces exact equality
        let reloaded = SettingsStore::load_or(path, Settings::default()).expect("reloads");
        assert_eq!(store.current().warmup, reloaded.current().warmup);
        assert_eq!(
            reloaded.current().warmup.providers.get("codex"),
            Some(&WarmupProviderPolicy {
                enabled: true,
                model: Some("gpt-5.6-sol".to_string()),
                idle_secs: 1800,
                min_interval_secs: 120,
            })
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_warmup_concurrent_mutate_unrelated_scopes_retained() {
        // given a store shared across concurrent writers
        let dir = temp_dir("warmup_concurrent");
        let path = dir.join("config.yaml");
        let store = Arc::new(SettingsStore::load_or(path.clone(), Settings::default()).expect("loads"));

        let barrier = Arc::new(std::sync::Barrier::new(2));

        let store1 = Arc::clone(&store);
        let barrier1 = Arc::clone(&barrier);
        let t1 = std::thread::spawn(move || {
            barrier1.wait();
            store1
                .mutate(|s| {
                    s.warmup.providers.insert(
                        "codex".to_string(),
                        WarmupProviderPolicy {
                            enabled: true,
                            model: Some("gpt-5.6-sol".to_string()),
                            idle_secs: 1800,
                            min_interval_secs: 120,
                        },
                    );
                })
                .expect("provider mutate");
        });

        let store2 = Arc::clone(&store);
        let barrier2 = Arc::clone(&barrier);
        let t2 = std::thread::spawn(move || {
            barrier2.wait();
            store2
                .mutate(|s| {
                    s.warmup.accounts.insert(
                        "generic-cline-1".to_string(),
                        WarmupAccountPolicy::Custom {
                            model: Some("z-ai/glm-5.3-flash".to_string()),
                            idle_secs: 7200,
                            min_interval_secs: 600,
                        },
                    );
                })
                .expect("account mutate");
        });

        t1.join().expect("t1 join");
        t2.join().expect("t2 join");

        // then both distinct policy keys are retained in memory
        let in_memory = store.current();
        assert!(
            in_memory.warmup.providers.contains_key("codex"),
            "provider scope must be retained"
        );
        assert!(
            in_memory.warmup.accounts.contains_key("generic-cline-1"),
            "account scope must be retained"
        );

        // and an independent reload proves both distinct keys are retained on disk
        let reloaded = SettingsStore::load_or(path, Settings::default()).expect("reloads");
        assert_eq!(
            reloaded.current().warmup.providers.get("codex"),
            Some(&WarmupProviderPolicy {
                enabled: true,
                model: Some("gpt-5.6-sol".to_string()),
                idle_secs: 1800,
                min_interval_secs: 120,
            })
        );
        assert_eq!(
            reloaded.current().warmup.accounts.get("generic-cline-1"),
            Some(&WarmupAccountPolicy::Custom {
                model: Some("z-ai/glm-5.3-flash".to_string()),
                idle_secs: 7200,
                min_interval_secs: 600,
            })
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_warmup_malformed_bounds_rejected() {
        let dir = temp_dir("warmup_bounds");
        let path = dir.join("config.yaml");
        let store = SettingsStore::load_or(path.clone(), Settings::default()).expect("loads");

        // provider idle_secs = 0 (too small: min 1)
        let res = store.mutate(|s| {
            s.warmup.providers.insert(
                "codex".to_string(),
                WarmupProviderPolicy {
                    enabled: true,
                    model: None,
                    idle_secs: 0,
                    min_interval_secs: 300,
                },
            );
        });
        assert!(res.is_err(), "idle_secs 0 must be rejected");
        assert!(store.current().warmup.providers.is_empty(), "failed mutate must leave memory unchanged");

        // provider idle_secs = 86401 (too large: max 86400)
        let res = store.mutate(|s| {
            s.warmup.providers.insert(
                "codex".to_string(),
                WarmupProviderPolicy {
                    enabled: true,
                    model: None,
                    idle_secs: 86401,
                    min_interval_secs: 300,
                },
            );
        });
        assert!(res.is_err(), "idle_secs 86401 must be rejected");

        // provider min_interval_secs = 0 (too small: min 1)
        let res = store.mutate(|s| {
            s.warmup.providers.insert(
                "codex".to_string(),
                WarmupProviderPolicy {
                    enabled: true,
                    model: None,
                    idle_secs: 3600,
                    min_interval_secs: 0,
                },
            );
        });
        assert!(res.is_err(), "min_interval_secs 0 must be rejected");

        // provider min_interval_secs = 604801 (too large: max 604800)
        let res = store.mutate(|s| {
            s.warmup.providers.insert(
                "codex".to_string(),
                WarmupProviderPolicy {
                    enabled: true,
                    model: None,
                    idle_secs: 3600,
                    min_interval_secs: 604801,
                },
            );
        });
        assert!(res.is_err(), "min_interval_secs 604801 must be rejected");

        // account custom idle_secs = 0
        let res = store.mutate(|s| {
            s.warmup.accounts.insert(
                "acct-1".to_string(),
                WarmupAccountPolicy::Custom {
                    model: None,
                    idle_secs: 0,
                    min_interval_secs: 300,
                },
            );
        });
        assert!(res.is_err(), "account custom idle_secs 0 must be rejected");

        // account custom min_interval_secs = 0
        let res = store.mutate(|s| {
            s.warmup.accounts.insert(
                "acct-1".to_string(),
                WarmupAccountPolicy::Custom {
                    model: None,
                    idle_secs: 3600,
                    min_interval_secs: 0,
                },
            );
        });
        assert!(res.is_err(), "account custom min_interval_secs 0 must be rejected");

        // reload from disk confirms nothing was persisted
        let reloaded = SettingsStore::load_or(path, Settings::default()).expect("reloads");
        assert!(reloaded.current().warmup.providers.is_empty());
        assert!(reloaded.current().warmup.accounts.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_warmup_defaults_automatic_off() {
        let settings = Settings::default();
        assert!(settings.warmup.providers.is_empty());
        assert!(settings.warmup.accounts.is_empty());

        let provider_default = WarmupProviderPolicy::default();
        assert!(!provider_default.enabled, "default automatic must be off");
        assert_eq!(provider_default.model, None);
        assert_eq!(provider_default.idle_secs, 3600);
        assert_eq!(provider_default.min_interval_secs, 300);

        let account_default = WarmupAccountPolicy::default();
        assert_eq!(account_default, WarmupAccountPolicy::Inherit);

        let effective = settings.warmup.effective_account_policy("codex", "codex-acct-1");
        assert!(!effective.enabled, "unconfigured account inherits default provider policy with enabled=false");
        assert_eq!(effective.model, None);
        assert_eq!(effective.idle_secs, 3600);
        assert_eq!(effective.min_interval_secs, 300);
    }

    #[test]
    fn test_warmup_account_custom_implicitly_enabled() {
        let mut settings = Settings::default();
        // provider is disabled (default)
        settings.warmup.providers.insert(
            "codex".to_string(),
            WarmupProviderPolicy {
                enabled: false,
                model: None,
                idle_secs: 3600,
                min_interval_secs: 300,
            },
        );

        // account custom is implicitly enabled even when provider is disabled
        settings.warmup.accounts.insert(
            "codex-acct-custom".to_string(),
            WarmupAccountPolicy::Custom {
                model: Some("gpt-5.6-sol".to_string()),
                idle_secs: 1800,
                min_interval_secs: 120,
            },
        );
        let custom_effective = settings.warmup.effective_account_policy("codex", "codex-acct-custom");
        assert!(custom_effective.enabled, "custom account policy is implicitly enabled");
        assert_eq!(custom_effective.model, Some("gpt-5.6-sol".to_string()));
        assert_eq!(custom_effective.idle_secs, 1800);
        assert_eq!(custom_effective.min_interval_secs, 120);

        // account custom with null model is also implicitly enabled
        settings.warmup.accounts.insert(
            "codex-acct-null-model".to_string(),
            WarmupAccountPolicy::Custom {
                model: None,
                idle_secs: 2400,
                min_interval_secs: 200,
            },
        );
        let null_model_effective = settings.warmup.effective_account_policy("codex", "codex-acct-null-model");
        assert!(null_model_effective.enabled, "custom account with null model is implicitly enabled");
        assert_eq!(null_model_effective.model, None);
        assert_eq!(null_model_effective.idle_secs, 2400);
        assert_eq!(null_model_effective.min_interval_secs, 200);

        // account off is disabled regardless of provider
        settings.warmup.providers.get_mut("codex").unwrap().enabled = true;
        settings.warmup.accounts.insert(
            "codex-acct-off".to_string(),
            WarmupAccountPolicy::Off,
        );
        let off_effective = settings.warmup.effective_account_policy("codex", "codex-acct-off");
        assert!(!off_effective.enabled, "off account is disabled");

        // account inherit follows provider
        settings.warmup.accounts.insert(
            "codex-acct-inherit".to_string(),
            WarmupAccountPolicy::Inherit,
        );
        let inherit_effective = settings.warmup.effective_account_policy("codex", "codex-acct-inherit");
        assert!(inherit_effective.enabled, "inherit follows enabled provider");
    }

    #[test]
    fn test_warmup_custom_required_values_deserialization() {
        // missing idle_secs and min_interval_secs in custom must fail to deserialize
        let missing_both = serde_json::from_str::<WarmupAccountPolicy>(r#"{"type":"custom","model":null}"#);
        assert!(missing_both.is_err(), "custom policy missing idle_secs and min_interval_secs must fail");

        // missing min_interval_secs in custom must fail to deserialize
        let missing_interval = serde_json::from_str::<WarmupAccountPolicy>(r#"{"type":"custom","idle_secs":1800}"#);
        assert!(missing_interval.is_err(), "custom policy missing min_interval_secs must fail");

        // missing idle_secs in custom must fail to deserialize
        let missing_idle = serde_json::from_str::<WarmupAccountPolicy>(r#"{"type":"custom","min_interval_secs":120}"#);
        assert!(missing_idle.is_err(), "custom policy missing idle_secs must fail");

        // inherit and off deserialize successfully without extra fields
        let inherit: WarmupAccountPolicy = serde_json::from_str(r#"{"type":"inherit"}"#).expect("parses inherit");
        assert_eq!(inherit, WarmupAccountPolicy::Inherit);

        let off: WarmupAccountPolicy = serde_json::from_str(r#"{"type":"off"}"#).expect("parses off");
        assert_eq!(off, WarmupAccountPolicy::Off);

        // valid custom with null model deserializes successfully
        let valid_custom: WarmupAccountPolicy = serde_json::from_str(
            r#"{"type":"custom","model":null,"idle_secs":1800,"min_interval_secs":120}"#
        ).expect("parses custom with null model");
        assert_eq!(
            valid_custom,
            WarmupAccountPolicy::Custom {
                model: None,
                idle_secs: 1800,
                min_interval_secs: 120,
            }
        );
    }
}
