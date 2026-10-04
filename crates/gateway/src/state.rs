use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use mahoquot_router::Router;
use mahoquot_types::{Health, PoolMember, Strategy};

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
    token_used: Arc<AtomicU64>,
}

impl ScopedKeyEntry {
    fn new(key: ScopedApiKey) -> Self {
        let token_used = Arc::new(AtomicU64::new(key.token_used));
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
            let mut entry = ScopedKeyEntry::new(key.clone());
            if let Some(existing) = previous.values().find(|entry| entry.key.id == key.id) {
                entry.token_used = Arc::clone(&existing.token_used);
                entry
                    .token_used
                    .fetch_max(key.token_used, Ordering::Relaxed);
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

/// Concurrent upstream inference requests the gateway admits at once. The
/// permit is owned by the downstream response body, so it is released when the
/// body finishes loading or the client disconnects — never at the headers.
/// This is an intentional overload ceiling (immediate 503 past it), not a
/// request-size limit: the 512 MiB body cap is untouched.
pub const MAX_CONCURRENT_INFERENCE_REQUESTS: usize = 4;

/// Guarded per-account usage-poll backoff.
///
/// The live-id allowlist and the backoff deadlines share one lock, so a
/// `set_poll_backoff` for an account that already left the pool is dropped
/// instead of resurrecting its entry after a reconcile. `lock()` exposes the
/// small subset of `HashMap` operations the quota poller uses, so the existing
/// `quota.rs` call sites keep compiling against the new field type.
#[derive(Debug, Default)]
pub struct PollBackoffStore {
    inner: std::sync::Mutex<PollBackoffInner>,
}

#[derive(Debug, Default)]
struct PollBackoffInner {
    /// Live pool ids; `None` until `retain_accounts` activates the allowlist,
    /// so a standalone store accepts every write until membership is explicit.
    active: Option<std::collections::BTreeSet<String>>,
    entries: std::collections::HashMap<String, i64>,
}

#[derive(Debug)]
pub struct PollBackoffGuard<'a> {
    inner: std::sync::MutexGuard<'a, PollBackoffInner>,
}

impl PollBackoffGuard<'_> {
    pub fn set(&mut self, account: &str, until_unix: i64) -> bool {
        if self.inner.active.as_ref().is_some_and(|active| !active.contains(account)) {
            return false;
        }
        self.inner.entries.insert(account.to_string(), until_unix);
        true
    }

    pub fn get(&self, account: &str) -> Option<&i64> {
        self.inner.entries.get(account)
    }

    /// Inserts only while the account is live: the membership check and the
    /// insert share this guard, so a reconcile cannot be interleaved.
    pub fn insert(&mut self, account: String, until_unix: i64) -> Option<i64> {
        if self
            .inner
            .active
            .as_ref()
            .is_some_and(|active| !active.contains(&account))
        {
            return None;
        }
        self.inner.entries.insert(account, until_unix)
    }

    pub fn remove(&mut self, account: &str) -> Option<i64> {
        self.inner.entries.remove(account)
    }

    pub fn len(&self) -> usize {
        self.inner.entries.len()
    }

    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&String, &mut i64) -> bool,
    {
        self.inner.entries.retain(|account, until| f(account, until));
    }
}

impl PollBackoffStore {
    pub fn lock(&self) -> std::sync::LockResult<PollBackoffGuard<'_>> {
        match self.inner.lock() {
            Ok(inner) => Ok(PollBackoffGuard { inner }),
            Err(poisoned) => Err(std::sync::PoisonError::new(PollBackoffGuard {
                inner: poisoned.into_inner(),
            })),
        }
    }

    /// Publishes the live-id allowlist and drops the backoff of every account
    /// that left the pool, all under one guard. Returns how many were pruned.
    pub fn retain_accounts(&self, active: &std::collections::BTreeSet<String>) -> usize {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.active = Some(active.clone());
        let before = inner.entries.len();
        inner.entries.retain(|id, _| active.contains(id));
        before - inner.entries.len()
    }

    pub fn tracked_accounts(&self) -> Vec<String> {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let mut ids: Vec<String> = inner.entries.keys().cloned().collect();
        ids.sort();
        ids
    }
}

/// Guarded proxy-client cache.
///
/// A sticky TTL proxy mints a new upstream URL every rotation bucket, so the
/// cached clients are keyed by that URL. The cache keeps only the newest URL
/// per provider+member, and the live-id allowlist shares the same lock as the
/// map, so a request that finishes after its account was removed cannot re-cache
/// a client for the departed member.
#[derive(Debug, Default)]
pub struct ProxyClientCache {
    inner: std::sync::Mutex<ProxyClientCacheInner>,
}

#[derive(Debug, Default)]
struct ProxyClientCacheInner {
    /// Live pool ids; `None` until `retain_members` activates the allowlist.
    active: Option<std::collections::BTreeSet<String>>,
    clients: std::collections::HashMap<String, reqwest::Client>,
}

impl ProxyClientCacheInner {
    fn admits(&self, member_id: &str) -> bool {
        self.active
            .as_ref()
            .is_none_or(|active| active.contains(member_id))
    }
}

/// The member id segment of a `{provider}|{member_id}|{url}` cache key.
fn proxy_client_owner(key: &str) -> Option<&str> {
    key.split('|').nth(1)
}

impl ProxyClientCache {
    pub fn get(&self, key: &str) -> Option<reqwest::Client> {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clients
            .get(key)
            .cloned()
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clients
            .len()
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.clients.clear();
    }

    /// Publishes the live-id allowlist and drops every cached client whose
    /// member left the pool, all under one guard. Returns the number pruned.
    pub fn retain_members(&self, active: &std::collections::BTreeSet<String>) -> usize {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.active = Some(active.clone());
        let before = inner.clients.len();
        inner
            .clients
            .retain(|key, _| proxy_client_owner(key).is_some_and(|id| active.contains(id)));
        before - inner.clients.len()
    }

    /// Replaces the member's previous URL client with `client`, unless the
    /// member is no longer live (in which case the fresh client is used for the
    /// request that built it but is never cached).
    pub fn replace_scoped(
        &self,
        key: String,
        scope: &str,
        member_id: &str,
        client: reqwest::Client,
    ) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if !inner.admits(member_id) {
            return;
        }
        inner
            .clients
            .retain(|existing, _| existing == &key || !existing.starts_with(scope));
        inner.clients.insert(key, client);
    }
}

/// Single-flight claim for one account's Devin catalog discovery. Dropping the
/// guard releases the claim, so a cancelled or failed refresh never wedges the
/// account's discovery.
pub struct DevinRefreshGuard {
    gate: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    id: String,
}

impl Drop for DevinRefreshGuard {
    fn drop(&mut self) {
        let mut set = self.gate.lock().unwrap_or_else(|p| p.into_inner());
        set.remove(&self.id);
    }
}

pub struct AppState {
    pub warmup: crate::warmup::WarmupRunner,
    pub router: Router,
    pub pool: Arc<arc_swap::ArcSwap<PoolSnapshot>>,
    pub runtime: Arc<UnifiedRuntimeState>,
    pub catalog: Arc<crate::registry::CatalogManager>,
    pub models_env: Option<String>,
    pub http_client: reqwest::Client,
    /// Never proxied. Serves providers left out of an active `proxy-providers`
    /// allowlist, so a per-provider proxy cannot capture everyone else's egress.
    pub direct_client: reqwest::Client,
    pub proxy_runtime: Arc<arc_swap::ArcSwap<crate::proxy_policy::ProxyRuntime>>,
    pub proxy_clients: Arc<ProxyClientCache>,
    pub metrics: Arc<GatewayMetrics>,
    pub monitor: Arc<MonitorState>,
    pub api_keys: Arc<ApiKeys>,
    /// In-memory scoped-key index: O(1), lock-free authentication and token
    /// accounting for delegated inbound keys.
    pub scoped_keys: Arc<ScopedKeyTracker>,
    pub refresh_url: String,
    pub auth_refresh_enabled: bool,
    /// Captcha scene endpoint + solver sidecar overrides for the zcode plan
    /// gateway (config/env-resolved at startup; tests inject directly).
    pub captcha_config_url: Option<String>,
    pub captcha_solver_bin: Option<PathBuf>,
    /// Verify params are single-use — challenge solves serialize on this gate
    /// (per-state, never a global static).
    pub captcha_solve_gate: tokio::sync::Mutex<()>,
    pub refreshed: AtomicU64,
    pub max_failover: usize,
    pub model_restrictions: AtomicBool,
    pub settings: Arc<SettingsStore>,
    pub scheduler: crate::scheduler::SchedulerRegistry,
    pub history: crate::request_history::HistoryService,
    pub signature_ledger: Arc<crate::compat::signature_ledger::SignatureLedger>,
    pub telemetry: Arc<TelemetryStore>,
    /// Live in-memory log tail, always fed regardless of `logging-to-file`.
    pub log_tail: LogTail,
    pub usage_samples: crate::usage::UsageSampleStore,
    pub usage_state: crate::usage::UsageStateStore,
    pub shutdown: Arc<tokio::sync::Notify>,
    pub devin_http_client: reqwest::Client,
    pub devin_direct_client: reqwest::Client,
    /// Per-account usage-poll backoff (unix secs). A 429 from a usage endpoint
    /// parks the account here so the poller stops keeping the throttle hot.
    /// The live-id allowlist shares this guard (see `PollBackoffStore`).
    pub usage_poll_backoff: PollBackoffStore,
    /// Devin discovery single-flight claims, taken before a refresh task is
    /// spawned.
    pub devin_refresh_in_flight: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    pub devin_cache: Arc<crate::devin_catalog::DevinDiscoveryCache>,
    /// Admission ceiling for upstream inference; see
    /// `MAX_CONCURRENT_INFERENCE_REQUESTS`.
    pub inference_gate: Arc<tokio::sync::Semaphore>,
    /// Serializes membership application for `rescan_pool`.
    rescan_gate: std::sync::Mutex<()>,
    pub finalizer_notifiers: Arc<
        std::sync::Mutex<std::collections::HashMap<String, Vec<tokio::sync::mpsc::Sender<()>>>>,
    >,
}

pub(crate) fn adopt_runtime_state(target: &AccountMember, previous: &Arc<AccountMember>) {
    // Only adopt runtime state if target represents the exact same credential identity.
    // True credential replacement (token change, endpoint change, or credential recreation)
    // creates a clean break, leaving target with fresh, distinct runtime cells.
    if !target.same_credential_identity(previous) {
        return;
    }

    let seq = std::sync::atomic::Ordering::Relaxed;
    let target_health = target.health();
    if target_health != Health::Disabled {
        let prev_health = previous.health();
        if prev_health != Health::Disabled {
            *target
                .health
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = prev_health;
        }
    }
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
    // Per-group cooldowns survive a reload for the same reason account-wide
    // health does: a reload must not hand back an allowance upstream is still
    // rate-limiting.
    *target
        .group_cooldowns
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = previous
        .group_cooldowns
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    target.ok_count.store(previous.ok_count.load(seq), seq);
    *target.last_activity.lock().unwrap() = *previous.last_activity.lock().unwrap();
    target.fail_count.store(previous.fail_count.load(seq), seq);
    if target.kind() == crate::account::ProviderKind::Devin
        && previous.kind() == crate::account::ProviderKind::Devin
    {
        target
            .devin_catalog
            .store(previous.devin_catalog.load_full());
    }
}

impl AppState {
    pub fn new(config: &GatewayConfig) -> anyhow::Result<Self> {
        let members = load_account_members(&config.auth_dir)?;

        let router = Router::new(config.strategy);
        let metrics = Arc::new(GatewayMetrics::default());
        let monitor = Arc::new(MonitorState::default());
        let refresh_url = config.refresh_url.clone();
        let auth_refresh_enabled = config.auth_refresh_enabled;
        let captcha_config_url = config.captcha_config_url.clone();
        let captcha_solver_bin = config.captcha_solver_bin.clone();
        let captcha_solve_gate = tokio::sync::Mutex::new(());
        let settings = Arc::new(SettingsStore::load_or(
            config.config_path.clone(),
            config.as_settings(),
        )?);

        let initial_strategy = match settings.current().routing.strategy.as_str() {
            "fill-first" => Strategy::FillFirst,
            "round-robin" => Strategy::StrictRoundRobin,
            other => crate::management::scalar_table::parse_routing_strategy(other)
                .unwrap_or(config.strategy),
        };
        router.set_strategy(initial_strategy);

        let initial_settings = settings.current();
        let base_proxy = crate::proxy_policy::base_proxy_url(&initial_settings);
        let http_client = crate::proxy_policy::build_http_client(base_proxy)
            .map_err(|e| anyhow::anyhow!("failed to build reqwest client: {}", e))?;
        let direct_client = crate::proxy_policy::build_http_client(None)
            .map_err(|e| anyhow::anyhow!("failed to build reqwest client: {}", e))?;
        let devin_http_client = crate::proxy_policy::build_devin_http_client(base_proxy)
            .map_err(|e| anyhow::anyhow!("failed to build devin reqwest client: {}", e))?;
        let devin_direct_client = crate::proxy_policy::build_devin_http_client(None)
            .map_err(|e| anyhow::anyhow!("failed to build direct devin reqwest client: {}", e))?;

        let proxy_runtime = Arc::new(arc_swap::ArcSwap::from_pointee(
            crate::proxy_policy::ProxyRuntime::from_settings(&initial_settings),
        ));
        let proxy_clients = Arc::new(ProxyClientCache::default());

        let proxy_runtime_obs = Arc::clone(&proxy_runtime);
        let proxy_clients_obs = Arc::clone(&proxy_clients);
        settings.add_observer(Arc::new(move |published| {
            proxy_runtime_obs.store(Arc::new(crate::proxy_policy::ProxyRuntime::from_settings(
                published,
            )));
            proxy_clients_obs.clear();
        }));
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
        let signature_ledger = crate::compat::signature_ledger::SignatureLedger::open(
            config
                .auth_dir
                .join(crate::compat::signature_ledger::DEFAULT_SNAPSHOT_FILE),
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
                let mut usage = snapshot.clone();
                if member.provider_name() == "cline" {
                    let now_unix = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    usage.expire_stale_cline_limits(now_unix);
                }
                member.set_usage(usage);
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
        catalog.bind_settings(&settings);

        let catalog_for_snapshot = Arc::clone(&catalog);
        settings.set_snapshot_provider(Arc::new(move || catalog_for_snapshot.raw_snapshot()));
        let runtime_for_publisher = Arc::clone(&runtime);
        settings.set_pool_publisher(Arc::new(move |registry| {
            runtime_for_publisher.update_registry(registry).map(|_| ())
        }));

        let state = Self {
            settings,
            scheduler,
            history,
            signature_ledger,
            telemetry,
            log_tail: LogTail::default(),
            usage_samples: crate::usage::UsageSampleStore::load(
                config.config_path.with_file_name("usage-samples.json"),
            ),
            usage_state,
            shutdown: Arc::new(tokio::sync::Notify::new()),
            usage_poll_backoff: PollBackoffStore::default(),
            devin_refresh_in_flight: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            inference_gate: Arc::new(tokio::sync::Semaphore::new(
                MAX_CONCURRENT_INFERENCE_REQUESTS,
            )),
            rescan_gate: std::sync::Mutex::new(()),
            router,
            runtime,
            catalog,
            warmup: crate::warmup::WarmupRunner::default(),
            pool,
            models_env: config.models_env.clone(),
            http_client,
            direct_client,
            devin_http_client,
            devin_direct_client,
            proxy_runtime,
            proxy_clients,
            metrics,
            monitor,
            api_keys,
            scoped_keys,
            refresh_url,
            auth_refresh_enabled,
            captcha_config_url,
            captcha_solver_bin,
            captcha_solve_gate,
            refreshed: AtomicU64::new(0),
            max_failover: if config.max_failover == 0 {
                3
            } else {
                config.max_failover
            },
            model_restrictions: AtomicBool::new(false),
            devin_cache: Arc::new(crate::devin_catalog::DevinDiscoveryCache::new()),
            finalizer_notifiers: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        };
        // Bootstrap membership: every store that can outlive an account starts
        // with the initial pool's allowlist, so no later writer can recreate an
        // id the pool never had.
        state.reconcile_account_state();
        Ok(state)
    }

    pub fn set_routing_strategy(&self, strategy: mahoquot_types::Strategy) {
        self.router.set_strategy(strategy);
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
        // Serialize the entire load+apply sequence. Two overlapping rescans
        // must never let the older membership land after the newer one and
        // resurrect accounts the pool has already dropped.
        let _gate = self
            .rescan_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
            .map(|mut m| match previous.get(&m.id) {
                Some(previous_member) => {
                    // fresh parse wins (new tokens/project), runtime state transfers
                    adopt_runtime_state(&m, previous_member);
                    if m.same_credential_identity(previous_member) {
                        let fresh = Arc::get_mut(&mut m).expect("freshly loaded account");
                        fresh.last_activity = previous_member.last_activity.clone();
                        fresh.active_requests = previous_member.active_requests.clone();
                    }
                    m
                }
                None => m,
            })
            .collect();
        let count = members.len();
        let new_snapshot = self.runtime.reload_accounts(members)?;
        self.scheduler.reconcile(&new_snapshot.members);
        self.reconcile_account_state();
        Ok(count)
    }

    /// Publishes the live pool ids into every store that can be written by a
    /// task outliving an account. This is a fan-out of `retain_accounts`; it
    /// never performs a membership check on a writer's behalf — each store
    /// keeps its allowlist inside the same guard as its data.
    pub fn reconcile_account_state(&self) {
        let ids: std::collections::BTreeSet<String> = self
            .pool
            .load()
            .members
            .iter()
            .map(|member| member.id.clone())
            .collect();
        let _ = self.usage_samples.retain_accounts(&ids);
        let _ = self.monitor.retain_accounts(&ids);
        let _ = self.warmup.retain_accounts(&ids);
        let _ = self.router.retain_members(&ids);
        let _ = self.devin_cache.retain_accounts(&ids);
        let _ = self.scheduler.retain_accounts(&ids);
        let _ = self.usage_poll_backoff.retain_accounts(&ids);
        let _ = self.proxy_clients.retain_members(&ids);
    }

    /// Authorization check for management input (scheduler reserve), not a
    /// state-write gate.
    pub fn is_active_account(&self, id: &str) -> bool {
        self.pool.load().members.iter().any(|member| member.id == id)
    }

    /// Admittance gate for upstream inference requests.
    pub fn inference_gate(&self) -> Arc<tokio::sync::Semaphore> {
        Arc::clone(&self.inference_gate)
    }

    /// Number of cached per-target proxy clients (diagnostic; the retained set
    /// is bounded to the latest URL per provider+member).
    pub fn proxy_client_count(&self) -> usize {
        self.proxy_clients.len()
    }

    /// Claims the single-flight slot for an account's Devin discovery. Returns
    /// `None` when a refresh is already in flight, so a burst of stale requests
    /// collapses onto the worker that is already running.
    pub fn begin_devin_refresh(&self, id: &str) -> Option<DevinRefreshGuard> {
        let mut in_flight = self
            .devin_refresh_in_flight
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if !in_flight.insert(id.to_string()) {
            return None;
        }
        Some(DevinRefreshGuard {
            gate: Arc::clone(&self.devin_refresh_in_flight),
            id: id.to_string(),
        })
    }

    pub fn devin_refresh_in_flight_count(&self) -> usize {
        self.devin_refresh_in_flight
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
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
        let client = self.client_for_member(member);
        let did_refresh = member
            .refresh(&client, &self.refresh_url, presented_token)
            .await?;
        if did_refresh {
            self.refreshed.fetch_add(1, Ordering::Relaxed);
        }
        Ok(did_refresh)
    }

    pub fn client_for_member(&self, member: &AccountMember) -> reqwest::Client {
        self.client_for_target(&member.provider_name(), member.id())
    }

    pub fn client_for_target(&self, provider_name: &str, member_id: &str) -> reqwest::Client {
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.client_for_target_at(provider_name, member_id, now_unix)
    }

    /// Time-injected core of `client_for_target`.
    ///
    /// A sticky TTL proxy mints a new upstream URL every rotation bucket. The
    /// cached client is keyed by that URL, so without eviction the map would
    /// keep one reqwest client (and its connection pool) per bucket forever.
    /// On insert we keep only the newest URL per provider+member: the replaced
    /// client is dropped, while a task that already cloned it finishes normally.
    pub(crate) fn client_for_target_at(
        &self,
        provider_name: &str,
        member_id: &str,
        now_unix: u64,
    ) -> reqwest::Client {
        let runtime = self.proxy_runtime.load();

        let Some(proxy_url) = runtime.session_proxy_url(provider_name, member_id, now_unix) else {
            // Toggling a provider on at runtime must not retroactively proxy
            // the providers that were never opted in.
            return if runtime.scoped_routing_active() {
                self.direct_client.clone()
            } else {
                self.http_client.clone()
            };
        };

        let cache_key = format!("{provider_name}|{member_id}|{proxy_url}");
        if let Some(client) = self.proxy_clients.get(&cache_key) {
            return client;
        }

        match crate::proxy_policy::build_http_client(Some(&proxy_url)) {
            Ok(client) => {
                let scope = format!("{provider_name}|{member_id}|");
                self.proxy_clients
                    .replace_scoped(cache_key, &scope, member_id, client.clone());
                client
            }
            Err(err) => {
                tracing::warn!("failed to build proxy client ({proxy_url}): {err}; falling back to default client");
                self.http_client.clone()
            }
        }
    }

    /// Reusable HTTP/1.1 client with no-redirect policy for Devin discovery and Chat relay.
    /// Follows provider-aware outbound proxy configuration and caches clients by target.
    /// Returns typed/safe Result propagating pre-dispatch failure without secret disclosure or unproxied fallback.
    pub fn devin_client_for_member(
        &self,
        member: &AccountMember,
    ) -> Result<reqwest::Client, crate::proxy_policy::DevinClientBuildError> {
        self.devin_client_for_target(member.id())
    }

    pub fn subscribe_finalizer(&self, account_or_event: &str) -> tokio::sync::mpsc::Receiver<()> {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let mut guard = self
            .finalizer_notifiers
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        guard
            .entry(account_or_event.to_string())
            .or_default()
            .push(tx);
        rx
    }

    pub fn notify_finalizer(&self, account: Option<&str>, event_id: &str) {
        let mut guard = self
            .finalizer_notifiers
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(acc) = account {
            if let Some(senders) = guard.remove(acc) {
                for sender in senders {
                    let _ = sender.try_send(());
                }
            }
        }
        if let Some(senders) = guard.remove(event_id) {
            for sender in senders {
                let _ = sender.try_send(());
            }
        }
    }

    /// Alias for devin_client_for_member returning typed/safe Result.
    pub fn try_devin_client_for_member(
        &self,
        member: &AccountMember,
    ) -> Result<reqwest::Client, crate::proxy_policy::DevinClientBuildError> {
        self.devin_client_for_member(member)
    }

    /// Reusable HTTP/1.1 client with no-redirect policy for Devin by member ID.
    pub fn devin_client_for_target(
        &self,
        member_id: &str,
    ) -> Result<reqwest::Client, crate::proxy_policy::DevinClientBuildError> {
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.devin_client_for_target_at(member_id, now_unix)
    }

    /// Time-injected core of `devin_client_for_target`; see
    /// `client_for_target_at` for the retention rule.
    pub(crate) fn devin_client_for_target_at(
        &self,
        member_id: &str,
        now_unix: u64,
    ) -> Result<reqwest::Client, crate::proxy_policy::DevinClientBuildError> {
        let runtime = self.proxy_runtime.load();

        let Some(proxy_url) = runtime.session_proxy_url("devin", member_id, now_unix) else {
            // session_proxy_url(None) must honor global proxy semantics consistently with existing client_for_target
            return Ok(if runtime.scoped_routing_active() {
                self.devin_direct_client.clone()
            } else {
                self.devin_http_client.clone()
            });
        };

        let cache_key = format!("devin|{member_id}|{proxy_url}");
        if let Some(client) = self.proxy_clients.get(&cache_key) {
            return Ok(client);
        }

        match crate::proxy_policy::build_devin_http_client(Some(&proxy_url)) {
            Ok(client) => {
                let scope = format!("devin|{member_id}|");
                self.proxy_clients
                    .replace_scoped(cache_key, &scope, member_id, client.clone());
                Ok(client)
            }
            Err(err) => {
                // Propagate pre-dispatch failure without secret disclosure or unproxied fallback
                tracing::error!(
                    member_id = %member_id,
                    "failed to configure Devin outbound proxy: {err}"
                );
                Err(err)
            }
        }
    }

    pub fn get_stats(&self) -> AdminStatsResponse {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        // Routability mirrors what the relay would do, per account, over the
        // models that account can serve. Candidates are the models generic
        // accounts declare (deduplicated) — data-driven, so a provider adding
        // or retiring models needs no edit here. Non-generic providers (codex,
        // antigravity, ...) keep their own discovery surfaces.
        let mut candidate_models: Vec<String> = Vec::new();
        for member in self.pool.load().members.iter() {
            if let Some((_, declared)) = member.generic_models() {
                for model in declared {
                    if !candidate_models.contains(&model) {
                        candidate_models.push(model);
                    }
                }
            }
        }
        let model_candidates: Vec<_> = candidate_models
            .iter()
            .map(|model| {
                (
                    model.as_str(),
                    crate::relay::eligible_account_ids_for_model(self, model, now_ms),
                )
            })
            .collect();

        let accounts = self
            .pool
            .load()
            .members
            .iter()
            .map(|m| {
                let health = m.health();
                let (input_tokens, output_tokens) = self.telemetry.account_tokens(&m.id);
                // Per-model quota blocks live only in `group_cooldowns`; account-wide
                // health never sees them. Fold the earliest active one in so a benched
                // account stops reporting itself `available`.
                let group_deadline = m.earliest_group_cooldown_ms(now_ms);
                let (effective_health, reset_at_unix_ms) = match health {
                    Health::Cooldown { until_unix_ms } if until_unix_ms <= now_ms => {
                        match group_deadline {
                            Some(until_unix_ms) => (
                                crate::metrics::HealthStats::Cooldown { until_unix_ms },
                                Some(until_unix_ms),
                            ),
                            None => (crate::metrics::HealthStats::Available, None),
                        }
                    }
                    Health::Cooldown { until_unix_ms } => {
                        // Whichever block releases first is the one the operator awaits.
                        let until_unix_ms = group_deadline
                            .filter(|group| *group < until_unix_ms)
                            .unwrap_or(until_unix_ms);
                        (
                            crate::metrics::HealthStats::Cooldown { until_unix_ms },
                            Some(until_unix_ms),
                        )
                    }
                    Health::Available => match group_deadline {
                        Some(until_unix_ms) => (
                            crate::metrics::HealthStats::Cooldown { until_unix_ms },
                            Some(until_unix_ms),
                        ),
                        None => (crate::metrics::HealthStats::Available, None),
                    },
                    Health::AuthFailed => (crate::metrics::HealthStats::AuthFailed, None),
                    Health::Disabled => (crate::metrics::HealthStats::Disabled, None),
                };
                crate::metrics::AccountStats {
                    id: m.id.clone(),
                    provider: m.provider_name(),
                    plan: m.relay_plan(),
                    health: effective_health,
                    ok: m.ok_count.load(Ordering::Relaxed),
                    fails: m.fail_count.load(Ordering::Relaxed),
                    input_tokens,
                    output_tokens,
                    total_tokens: input_tokens.saturating_add(output_tokens),
                    reset_at_unix_ms,
                    model_routability: m.generic_models().map(|(_, declared)| {
                        model_candidates
                            .iter()
                            .filter(|(model, _)| {
                                declared.is_empty() || declared.iter().any(|id| id.as_str() == *model)
                            })
                            .map(|(model, ids)| ((*model).to_string(), ids.contains(&m.id)))
                            .collect()
                    }),
                    last_error: self.monitor.last_error(&m.id),
                    ttft: self.monitor.account_ttft(&m.id),
                    usage: m.usage_snapshot(),
                    models: if m.kind() == crate::account::ProviderKind::Devin {
                        m.devin_models()
                    } else {
                        None
                    },
                    discovery: if m.kind() == crate::account::ProviderKind::Devin {
                        Some(match m.devin_catalog_state() {
                            Some(cat) => crate::metrics::AccountDiscoveryMetadata {
                                status: if m.is_manually_disabled() {
                                    "disabled".to_string()
                                } else if !cat.has_succeeded {
                                    "unknown".to_string()
                                } else if cat.models.is_empty() {
                                    "empty".to_string()
                                } else {
                                    "discovered".to_string()
                                },
                                has_succeeded: cat.has_succeeded,
                                refreshed_at_unix_ms: cat.last_refresh_at,
                                models: if m.is_manually_disabled() {
                                    Vec::new()
                                } else {
                                    cat.models
                                        .iter()
                                        .map(|model| crate::metrics::DiscoveredModelMetadata {
                                            id: model.public_id.clone(),
                                            model_uid: model.model_uid.clone(),
                                            supports_images: model.supports_images,
                                            credit_multiplier: model.credit_multiplier,
                                            is_recommended: model.is_recommended,
                                            is_new: model.is_new,
                                            is_capacity_limited: model.is_capacity_limited,
                                            promo_active: model.promo_active,
                                        })
                                        .collect()
                                },
                            },
                            None => crate::metrics::AccountDiscoveryMetadata {
                                status: if m.is_manually_disabled() {
                                    "disabled".to_string()
                                } else {
                                    "unknown".to_string()
                                },
                                has_succeeded: false,
                                refreshed_at_unix_ms: None,
                                models: Vec::new(),
                            },
                        })
                    } else {
                        None
                    },
                    credits_after_limit: if m.kind() == crate::account::ProviderKind::Codex {
                        Some(self.settings.current().is_codex_account_credit_enabled(&m.id))
                    } else {
                        None
                    },
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
            signature_ledger: self.signature_ledger.stats(),
        }
    }
}

#[cfg(test)]
mod stats_tests {
    use super::*;

    fn state_with_cline_account() -> (Arc<AppState>, Arc<crate::account::AccountMember>) {
        let auth_dir = std::env::temp_dir().join(format!(
            "mahoquot-stats-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&auth_dir).expect("create auth dir");
        let credential = serde_json::json!({
            "type": "generic",
            "identity_slug": "cline-account",
            "provider": "cline",
            "label": "Cline",
            "adapter": "openai-chat",
            "base_url": "http://127.0.0.1:9",
            "api_key": "fixture-cline",
            "models": ["cline-free/gemini-3.8-flash", "cline-free/deepseek-v4.1-flash"],
        })
        .to_string();
        std::fs::write(auth_dir.join("generic-cline.json"), credential).expect("write cline");
        let config = crate::config::GatewayConfig {
            auth_dir: auth_dir.clone(),
            config_path: auth_dir.join("config.yaml"),
            auth_refresh_enabled: false,
            ..crate::config::GatewayConfig::default()
        };
        let state = Arc::new(AppState::new(&config).expect("state"));
        let member = state.pool.load_full().members[0].clone();
        (state, member)
    }

    fn account_json(state: &AppState, id: &str) -> serde_json::Value {
        let stats = state.get_stats();
        let account = stats
            .accounts
            .iter()
            .find(|account| account.id == id)
            .expect("account present in stats");
        serde_json::to_value(account).expect("serialize account stats")
    }

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    #[test]
    fn an_account_benched_for_one_quota_group_reports_cooldown() {
        let (state, member) = state_with_cline_account();
        let deadline = (now_ms() / 1000 + 600) * 1000;
        assert!(
            member.set_group_cooldown("z-ai/glm-5.3-flash", deadline),
            "test premise: cline must split quota groups"
        );

        let value = account_json(&state, &member.id);
        assert_eq!(
            value["health"]["status"], "cooldown",
            "an account blocked for one model must not report itself available while \
             requests for it are being rejected"
        );
        assert_eq!(
            value["reset_at_unix_ms"],
            serde_json::json!(deadline),
            "the earliest per-model deadline is the reset the operator waits on"
        );
    }

    #[test]
    fn an_account_with_no_active_quota_block_stays_available() {
        let (state, member) = state_with_cline_account();
        let expired = (now_ms() / 1000 - 60) * 1000;
        assert!(member.set_group_cooldown("z-ai/glm-5.3-flash", expired));

        let value = account_json(&state, &member.id);
        assert_eq!(
            value["health"]["status"], "available",
            "an expired per-model deadline must not pin the cooldown badge"
        );
        assert_eq!(value["reset_at_unix_ms"], serde_json::json!(null));
    }

    #[test]
    fn cline_stats_separate_estimated_usage_from_model_routing() {
        let (state, member) = state_with_cline_account();
        let now = now_ms();
        crate::relay::record_cline_quota_bucket(
            &member,
            "cline-free/gemini-3.8-flash",
            600,
            now / 1000,
            99.9,
        );
        let before = account_json(&state, &member.id);
        assert_eq!(
            before["model_routability"]["cline-free/gemini-3.8-flash"],
            true
        );
        member.set_group_cooldown("cline-free/gemini-3.8-flash", now + 600_000);
        let stats = account_json(&state, &member.id);
        assert_eq!(
            stats["model_routability"]["cline-free/gemini-3.8-flash"],
            false
        );
        assert_eq!(
            stats["model_routability"]["cline-free/deepseek-v4.1-flash"],
            true
        );

        let (expired_state, expired_member) = state_with_cline_account();
        expired_member.set_group_cooldown("cline-free/gemini-3.8-flash", now - 1_000);
        let expired = account_json(&expired_state, &expired_member.id);
        assert_eq!(
            expired["model_routability"]["cline-free/gemini-3.8-flash"],
            true
        );

        state.force_health(&member.id, Health::Disabled);
        let disabled = account_json(&state, &member.id);
        assert_eq!(
            disabled["model_routability"]["cline-free/gemini-3.8-flash"],
            false
        );
        assert_eq!(
            disabled["model_routability"]["cline-free/deepseek-v4.1-flash"],
            false
        );
    }

    #[test]
    fn a_non_cline_generic_account_reports_routability_for_its_declared_models() {
        let auth_dir = std::env::temp_dir().join(format!(
            "mahoquot-stats-zhipu-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&auth_dir).expect("create auth dir");
        let credential = serde_json::json!({
            "type": "generic",
            "identity_slug": "zhipu-coding",
            "provider": "zhipu-bigmodel-coding",
            "label": "Zhipu",
            "adapter": "openai-chat",
            "base_url": "http://127.0.0.1:9",
            "api_key": "fixture-zhipu",
            "models": ["glm-5.3", "glm-5.3-flash"],
        })
        .to_string();
        std::fs::write(auth_dir.join("generic-zhipu.json"), credential).expect("write zhipu");
        let config = crate::config::GatewayConfig {
            auth_dir: auth_dir.clone(),
            config_path: auth_dir.join("config.yaml"),
            auth_refresh_enabled: false,
            ..crate::config::GatewayConfig::default()
        };
        let state = Arc::new(AppState::new(&config).expect("state"));
        let member = state.pool.load_full().members[0].clone();

        let value = account_json(&state, &member.id);
        assert_eq!(
            value["model_routability"]["glm-5.3"], true,
            "a healthy account must report its declared models routable"
        );
        assert_eq!(value["model_routability"]["glm-5.3-flash"], true);

        // Providers without a quota-group split bench per model through the
        // upstream unsupported-models feedback path, not group cooldowns.
        member.mark_model_unsupported("glm-5.3-flash");
        let value = account_json(&state, &member.id);
        assert_eq!(
            value["model_routability"]["glm-5.3-flash"], false,
            "an upstream-unsupported model must show up as unroutable"
        );
        assert_eq!(
            value["model_routability"]["glm-5.3"], true,
            "the sibling model must stay routable — per-model, not account-wide"
        );

        state.force_health(&member.id, Health::Disabled);
        let value = account_json(&state, &member.id);
        assert_eq!(
            value["model_routability"]["glm-5.3"], false,
            "account-wide health must dominate every per-model entry"
        );
        assert_eq!(value["model_routability"]["glm-5.3-flash"], false);
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::management::settings::ProviderProxyPolicy;
    use crate::proxy_policy::ProxyRuntime;

    fn state_with_one_account() -> (Arc<AppState>, Arc<crate::account::AccountMember>) {
        let auth_dir = std::env::temp_dir().join(format!(
            "mahoquot-shared-integration-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&auth_dir).expect("create auth dir");
        let credential = serde_json::json!({
            "type": "generic",
            "identity_slug": "cline-account",
            "provider": "cline",
            "label": "Cline",
            "adapter": "openai-chat",
            "base_url": "http://127.0.0.1:9",
            "api_key": "fixture-cline",
            "models": ["cline-free/gemini-3.8-flash"],
        })
        .to_string();
        std::fs::write(auth_dir.join("generic-cline.json"), credential).expect("write cline");
        let config = crate::config::GatewayConfig {
            auth_dir: auth_dir.clone(),
            config_path: auth_dir.join("config.yaml"),
            auth_refresh_enabled: false,
            ..crate::config::GatewayConfig::default()
        };
        let state = Arc::new(AppState::new(&config).expect("state"));
        let member = state.pool.load_full().members[0].clone();
        (state, member)
    }

    /// Sticky TTL policy: the session proxy URL changes once per bucket, which is
    /// the rotation that used to grow the client map without bound.
    fn install_sticky_policy(state: &AppState, provider: &str) {
        let mut providers = std::collections::BTreeMap::new();
        providers.insert(
            provider.to_string(),
            ProviderProxyPolicy {
                enabled: true,
                sticky: true,
                ttl_secs: 600,
                url: "http://127.0.0.1:18840".to_string(),
            },
        );
        state.proxy_runtime.store(Arc::new(ProxyRuntime {
            global_proxy_url: String::new(),
            providers,
        }));
    }

    #[test]
    fn proxy_clients_keep_only_the_latest_url_per_member() {
        let (state, member) = state_with_one_account();
        install_sticky_policy(&state, "cline");

        let first = state.client_for_target_at("cline", &member.id, 1_000_000);
        assert_eq!(state.proxy_client_count(), 1);

        let second = state.client_for_target_at("cline", &member.id, 1_000_600);
        assert_eq!(
            state.proxy_client_count(),
            1,
            "a rotation bucket must replace the member's previous client, not accumulate"
        );
        let again = state.client_for_target_at("cline", &member.id, 1_000_600);
        assert_eq!(state.proxy_client_count(), 1, "the newest bucket stays cached");

        drop((first, second, again));
        assert_eq!(state.proxy_client_count(), 1);
    }

    #[test]
    fn proxy_clients_keep_only_the_latest_url_per_devin_member() {
        let (state, member) = state_with_one_account();
        install_sticky_policy(&state, "devin");

        state
            .devin_client_for_target_at(&member.id, 2_000_000)
            .expect("devin client");
        assert_eq!(state.proxy_client_count(), 1);
        state
            .devin_client_for_target_at(&member.id, 2_000_600)
            .expect("devin client");
        assert_eq!(
            state.proxy_client_count(),
            1,
            "Devin rotation must replace, not accumulate"
        );
    }

    #[test]
    fn proxy_clients_keep_distinct_members_separate() {
        let (state, _member) = state_with_one_account();
        install_sticky_policy(&state, "cline");
        state.proxy_clients.retain_members(
            &["member-a".to_string(), "member-b".to_string()].into_iter().collect(),
        );

        state.client_for_target_at("cline", "member-a", 3_000_000);
        state.client_for_target_at("cline", "member-b", 3_000_000);
        assert_eq!(state.proxy_client_count(), 2);

        state.client_for_target_at("cline", "member-a", 3_000_600);
        assert_eq!(
            state.proxy_client_count(),
            2,
            "rotating one member must not evict another member's client"
        );
    }

    #[test]
    fn inference_gate_is_exhausted_after_four_permits_and_recovers_on_drop() {
        let (state, _member) = state_with_one_account();
        let gate = state.inference_gate();

        let mut held = Vec::new();
        for _ in 0..MAX_CONCURRENT_INFERENCE_REQUESTS {
            held.push(
                gate.clone()
                    .try_acquire_owned()
                    .expect("permit within the ceiling"),
            );
        }
        assert!(
            gate.clone().try_acquire_owned().is_err(),
            "the request past the ceiling must be rejected immediately"
        );

        held.pop();
        assert!(
            gate.clone().try_acquire_owned().is_ok(),
            "releasing a permit must admit the next request"
        );
    }

    #[test]
    fn devin_refresh_claim_is_single_flight_and_released_on_drop() {
        let (state, _member) = state_with_one_account();

        let guard = state.begin_devin_refresh("acct").expect("first claim");
        assert!(
            state.begin_devin_refresh("acct").is_none(),
            "a repeated request while a refresh is in flight must coalesce"
        );
        assert_eq!(state.devin_refresh_in_flight_count(), 1);

        drop(guard);
        assert!(
            state.begin_devin_refresh("acct").is_some(),
            "dropping the guard must release the claim"
        );
    }
}
