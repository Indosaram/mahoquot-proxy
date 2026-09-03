use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use mahoquot_router::Router;
use mahoquot_types::{Health, PoolMember};

use crate::account::{load_account_members, AccountMember};
use crate::config::GatewayConfig;
use crate::inbound::ApiKeys;
use crate::management::observability::LogTail;
use crate::management::settings::ScopedApiKey;
use crate::management::store::SettingsStore;
use crate::metrics::{AdminStatsResponse, GatewayMetrics};
use crate::monitor::MonitorState;
pub use crate::runtime_state::{
    compute_candidate_composition, PoolSnapshot, RefreshCoordinator, RuntimeComposition,
    RuntimeState, UnifiedRuntimeState,
};
use crate::telemetry::TelemetryStore;

/// One scoped key's live state: the immutable published definition plus the
/// counter that moves on every request.
///
/// Token usage is a plain atomic rather than a field inside the settings
/// document because the relay updates it per request; routing every increment
/// through `SettingsStore::mutate` would put a disk write and a global mutex on
/// the hot path.
#[derive(Debug)]
pub struct ScopedKeyEntry {
    pub key: Arc<ScopedApiKey>,
    token_used: AtomicU64,
}

impl ScopedKeyEntry {
    fn new(key: ScopedApiKey) -> Self {
        let token_used = AtomicU64::new(key.token_used);
        Self {
            key: Arc::new(key),
            token_used,
        }
    }

    pub fn token_used(&self) -> u64 {
        self.token_used.load(Ordering::Relaxed)
    }

    pub fn token_limit(&self) -> u64 {
        self.key.token_limit
    }

    /// A zero limit means unlimited, matching the settings default for a key
    /// minted without a cap.
    pub fn is_exhausted(&self) -> bool {
        self.key.token_limit > 0 && self.token_used() >= self.key.token_limit
    }

    /// Charge `tokens` against the key and report the new total. Saturating so a
    /// pathological usage report cannot wrap the counter back under the limit.
    pub fn consume(&self, tokens: u64) -> u64 {
        let mut current = self.token_used.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_add(tokens);
            match self.token_used.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return next,
                Err(observed) => current = observed,
            }
        }
    }

    pub fn is_usable_at(&self, now_ms: i64) -> bool {
        self.key.is_usable_at(now_ms)
    }
}

/// Lock-free index of scoped inbound keys, keyed by the one-way key identifier.
///
/// Lookups on the request path are a single `ArcSwap` load plus a hash probe;
/// writers publish a whole new map, so an authenticating request never blocks
/// behind a key edit. Counters survive a republish: an identifier still present
/// after reconciliation keeps its live `token_used`.
#[derive(Debug, Default)]
pub struct ScopedKeyTracker {
    entries: arc_swap::ArcSwap<std::collections::HashMap<String, Arc<ScopedKeyEntry>>>,
}

impl ScopedKeyTracker {
    pub fn new(keys: &[ScopedApiKey]) -> Self {
        let tracker = Self::default();
        tracker.reconcile(keys);
        tracker
    }

    /// Republish the index from the settings document, carrying live usage
    /// counters across for keys that still exist. A key whose persisted
    /// `token_used` has moved ahead of the in-memory counter (an operator reset
    /// or an external edit) adopts the larger of the two, so a restart or a
    /// manual bump can never hand back already-spent allowance.
    pub fn reconcile(&self, keys: &[ScopedApiKey]) {
        let previous = self.entries.load();
        let mut next = std::collections::HashMap::with_capacity(keys.len());
        for key in keys {
            let entry = ScopedKeyEntry::new(key.clone());
            if let Some(existing) = previous.get(&key.key_identifier) {
                let live = existing.token_used();
                if live > entry.token_used() {
                    entry.token_used.store(live, Ordering::Relaxed);
                }
            }
            next.insert(key.key_identifier.clone(), Arc::new(entry));
        }
        self.entries.store(Arc::new(next));
    }

    /// O(1) lookup by the presented key's stable identifier.
    pub fn get(&self, key_identifier: &str) -> Option<Arc<ScopedKeyEntry>> {
        self.entries.load().get(key_identifier).cloned()
    }

    /// Resolve a raw presented key to its scoped entry, if any.
    pub fn lookup_raw(&self, presented: &str) -> Option<Arc<ScopedKeyEntry>> {
        self.get(&crate::request_history::stable_key_identifier(presented))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.load().is_empty()
    }

    /// Charge a request's tokens to a scoped key and report the key's new
    /// total. `None` when the identifier belongs to a master key (or to no key
    /// at all), which is the common case and costs one hash probe.
    pub fn record_usage(&self, key_identifier: Option<&str>, tokens: u64) -> Option<u64> {
        let entry = key_identifier.and_then(|id| self.get(id))?;
        if tokens == 0 {
            return Some(entry.token_used());
        }
        Some(entry.consume(tokens))
    }

    /// Snapshot of live counters, for reporting and for persisting usage back
    /// into the settings document.
    pub fn usage_snapshot(&self) -> Vec<(String, u64)> {
        self.entries
            .load()
            .iter()
            .map(|(identifier, entry)| (identifier.clone(), entry.token_used()))
            .collect()
    }
}

pub struct AppState {
    pub router: Router,
    pub pool: Arc<arc_swap::ArcSwap<PoolSnapshot>>,
    pub runtime: Arc<UnifiedRuntimeState>,
    pub catalog: Arc<crate::registry::CatalogManager>,
    pub models_env: Option<String>,
    pub http_client: reqwest::Client,
    pub metrics: Arc<GatewayMetrics>,
    pub monitor: Arc<MonitorState>,
    pub api_keys: Arc<ApiKeys>,
    /// In-memory scoped-key index: O(1), lock-free authentication and token
    /// accounting for delegated inbound keys.
    pub scoped_keys: Arc<ScopedKeyTracker>,
    pub refresh_url: String,
    pub auth_refresh_enabled: bool,
    pub refreshed: AtomicU64,
    pub max_failover: usize,
    pub model_restrictions: AtomicBool,
    pub settings: Arc<SettingsStore>,
    pub scheduler: crate::scheduler::SchedulerRegistry,
    pub history: crate::request_history::HistoryService,
    pub telemetry: Arc<TelemetryStore>,
    /// Live in-memory log tail, always fed regardless of `logging-to-file`.
    pub log_tail: LogTail,
    pub usage_samples: crate::usage::UsageSampleStore,
    pub usage_state: crate::usage::UsageStateStore,
    pub shutdown: Arc<tokio::sync::Notify>,
    /// Per-account usage-poll backoff (unix secs). A 429 from a usage endpoint
    /// parks the account here so the poller stops keeping the throttle hot.
    pub usage_poll_backoff: std::sync::Mutex<std::collections::HashMap<String, i64>>,
}

pub(crate) fn adopt_runtime_state(target: &AccountMember, previous: &Arc<AccountMember>) {
    let seq = std::sync::atomic::Ordering::Relaxed;
    *target
        .health
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = *previous
        .health
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *target
        .usage
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = previous
        .usage
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    *target
        .unsupported_models
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = previous
        .unsupported_models
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    target.ok_count.store(previous.ok_count.load(seq), seq);
    target.fail_count.store(previous.fail_count.load(seq), seq);
}

impl AppState {
    pub fn new(config: &GatewayConfig) -> anyhow::Result<Self> {
        let members = load_account_members(&config.auth_dir)?;

        let http_client = reqwest::Client::builder()
            .tcp_nodelay(true)
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build reqwest client: {}", e))?;

        let router = Router::new(config.strategy);
        let metrics = Arc::new(GatewayMetrics::default());
        let monitor = Arc::new(MonitorState::default());
        let refresh_url = config.refresh_url.clone();
        let auth_refresh_enabled = config.auth_refresh_enabled;
        let settings = Arc::new(SettingsStore::load_or(
            config.config_path.clone(),
            config.as_settings(),
        )?);
        let scoped_keys = Arc::new(ScopedKeyTracker::new(&settings.current().scoped_api_keys));
        // Every published settings document rebuilds the index, so a key that
        // is revoked or re-scoped through the management API takes effect on
        // the next request without a restart.
        let scoped_keys_for_observer = Arc::clone(&scoped_keys);
        settings.add_observer(Arc::new(move |published| {
            scoped_keys_for_observer.reconcile(&published.scoped_api_keys);
        }));
        let api_keys = Arc::new(ApiKeys::with_live_settings(
            Arc::clone(&settings),
            config.api_keys.clone(),
        ));
        let scheduler = crate::scheduler::SchedulerRegistry::load(&config.config_path, &members);
        let history = crate::request_history::HistoryService::open(
            &config.config_path.with_file_name("request-history.sqlite"),
            config.history_queue_capacity,
            config.history_batch_size,
            Arc::clone(&metrics),
        );
        if let Ok(store) = history.store() {
            if let Err(error) = store.prune(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
                    .unwrap_or(0),
            ) {
                tracing::warn!(%error, "failed to prune request history on startup");
            }
        }
        let telemetry = Arc::new(TelemetryStore::load(
            config.config_path.with_file_name("telemetry.json"),
        ));
        let usage_state = crate::usage::UsageStateStore::load(
            config.config_path.with_file_name("usage-state.json"),
        );
        // Restore the last observed quota snapshots so the console shows real
        // windows right after a restart instead of waiting for the first poll.
        let restored_usage = usage_state.restore();
        for member in &members {
            if let Some(snapshot) = restored_usage.get(&member.id) {
                member.set_usage(snapshot.clone());
            }
        }

        let catalog_settings = settings.current().model_catalog.clone();
        let catalog_config = crate::registry::CatalogConfig {
            cache_path: config.catalog_cache_path.clone(),
            remote_catalog_url: catalog_settings.as_ref().map(|catalog| catalog.url.clone()),
            remote_signature_url: catalog_settings
                .as_ref()
                .map(|catalog| catalog.signature_url.clone()),
            ..crate::registry::CatalogConfig::default()
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or_default();
        let catalog = Arc::new(crate::registry::CatalogManager::boot_with_metrics(
            catalog_config,
            now,
            Arc::clone(&metrics),
        ));
        let base_registry = catalog.current_snapshot();
        let initial_registry = settings
            .current()
            .validate_against_registry(&base_registry)
            .map(Arc::new)
            .unwrap_or(base_registry);
        let candidate = compute_candidate_composition(
            1,
            members,
            initial_registry,
            config.models_env.as_deref(),
        )
        .map_err(|e| anyhow::anyhow!("failed to compute initial composition: {e}"))?;

        let runtime = Arc::new(UnifiedRuntimeState::new(
            candidate,
            config.models_env.clone(),
        ));
        let pool = runtime.pool();
        catalog.bind_runtime(&runtime);

        let runtime_for_snapshot = Arc::clone(&runtime);
        settings.set_snapshot_provider(Arc::new(move || {
            Arc::clone(&runtime_for_snapshot.composition().registry)
        }));
        let runtime_for_publisher = Arc::clone(&runtime);
        settings.set_pool_publisher(Arc::new(move |registry| {
            runtime_for_publisher.update_registry(registry).map(|_| ())
        }));

        Ok(Self {
            settings,
            scheduler,
            history,
            telemetry,
            log_tail: LogTail::default(),
            usage_samples: crate::usage::UsageSampleStore::load(
                config.config_path.with_file_name("usage-samples.json"),
            ),
            usage_state,
            shutdown: Arc::new(tokio::sync::Notify::new()),
            usage_poll_backoff: std::sync::Mutex::default(),
            router,
            runtime,
            catalog,
            pool,
            models_env: config.models_env.clone(),
            http_client,
            metrics,
            monitor,
            api_keys,
            scoped_keys,
            refresh_url,
            auth_refresh_enabled,
            refreshed: AtomicU64::new(0),
            max_failover: if config.max_failover == 0 {
                3
            } else {
                config.max_failover
            },
            model_restrictions: AtomicBool::new(false),
        })
    }

    pub fn find_member(&self, id: &str) -> Option<Arc<AccountMember>> {
        self.pool
            .load()
            .members
            .iter()
            .find(|m| m.id == id)
            .cloned()
    }

    /// Rebuilds the pool from the auth directory so credentials written after
    /// startup (imports, OAuth onboarding) become live without a restart.
    pub fn rescan_pool(&self) -> anyhow::Result<usize> {
        let auth_dir = self.settings.current().auth_dir.clone();
        let members = load_account_members(std::path::Path::new(&auth_dir))?;
        // Surviving accounts keep their runtime state (health, counters, cached
        // usage): reloading them fresh would wipe cooldowns and quota caches on
        // every import or delete.
        let previous: std::collections::BTreeMap<String, Arc<AccountMember>> = self
            .pool
            .load()
            .members
            .iter()
            .map(|m| (m.id.clone(), m.clone()))
            .collect();
        let members: Vec<Arc<AccountMember>> = members
            .into_iter()
            .map(|m| match previous.get(&m.id) {
                Some(previous_member) => {
                    // fresh parse wins (new tokens/project), runtime state transfers
                    adopt_runtime_state(&m, previous_member);
                    m
                }
                None => m,
            })
            .collect();
        let count = members.len();
        let new_snapshot = self.runtime.reload_accounts(members)?;
        self.scheduler.reconcile(&new_snapshot.members);
        Ok(count)
    }

    pub fn runtime_state(&self) -> Arc<UnifiedRuntimeState> {
        Arc::clone(&self.runtime)
    }

    pub fn composition(&self) -> Arc<PoolSnapshot> {
        self.runtime.composition()
    }

    pub fn force_health(&self, id: &str, health: Health) {
        if let Some(m) = self.find_member(id) {
            m.set_health(health);
        }
    }

    pub async fn refresh_member(
        &self,
        member: &AccountMember,
        presented_token: Option<&str>,
    ) -> Result<bool, mahoquot_providers::refresh_exec::RefreshError> {
        let did_refresh = member
            .refresh(&self.http_client, &self.refresh_url, presented_token)
            .await?;
        if did_refresh {
            self.refreshed.fetch_add(1, Ordering::Relaxed);
        }
        Ok(did_refresh)
    }

    pub fn get_stats(&self) -> AdminStatsResponse {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        let accounts = self
            .pool
            .load()
            .members
            .iter()
            .map(|m| {
                let health = m.health();
                let (input_tokens, output_tokens) = self.telemetry.account_tokens(&m.id);
                let reset_at_unix_ms = match health {
                    Health::Cooldown { until_unix_ms } => Some(until_unix_ms),
                    _ => None,
                };
                crate::metrics::AccountStats {
                    id: m.id.clone(),
                    provider: m.provider_name(),
                    plan: m.relay_plan(),
                    health: health.into(),
                    ok: m.ok_count.load(Ordering::Relaxed),
                    fails: m.fail_count.load(Ordering::Relaxed),
                    input_tokens,
                    output_tokens,
                    total_tokens: input_tokens.saturating_add(output_tokens),
                    reset_at_unix_ms,
                    last_error: self.monitor.last_error(&m.id),
                    ttft: self.monitor.account_ttft(&m.id),
                    usage: m.usage_snapshot(),
                }
            })
            .collect();

        AdminStatsResponse {
            uptime_secs: self.monitor.uptime_secs(now_ms),
            in_flight: self.monitor.in_flight(),
            served: self.metrics.served.load(Ordering::Relaxed),
            failed_over: self.metrics.failed_over.load(Ordering::Relaxed),
            refreshed: self.refreshed.load(Ordering::Relaxed),
            exposed_errors: self.metrics.exposed_errors.load(Ordering::Relaxed),
            exposed_client_errors: self.metrics.exposed_client_errors.load(Ordering::Relaxed),
            ttft: self.monitor.ttft_percentiles(),
            accounts,
            history: self.telemetry.snapshot(),
        }
    }
}
