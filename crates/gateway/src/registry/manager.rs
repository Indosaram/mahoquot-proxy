use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use mahoquot_registry::{
    verify_catalog_envelope, CatalogEnvelope, CatalogSource, CatalogVersion, Keyring,
    RegistrySnapshot,
};
use serde::{Deserialize, Serialize};

use super::cache::LkgCache;
use super::error::CatalogError;
use crate::metrics::GatewayMetrics;

pub const DEFAULT_REMOTE_CATALOG_URL: &str =
    "https://raw.githubusercontent.com/Indosaram/mahoquot-proxy/model-catalog-v1/models-v1.json";
pub const DEFAULT_REMOTE_SIGNATURE_URL: &str =
    "https://raw.githubusercontent.com/Indosaram/mahoquot-proxy/model-catalog-v1/models-v1.json.sig";

/// Configuration for offline-first catalog loading, caching, and remote updates.
#[derive(Debug, Clone)]
pub struct CatalogConfig {
    /// Explicit path or directory for the Last-Known-Good (LKG) disk cache.
    pub cache_path: Option<PathBuf>,
    /// URL to fetch remote catalog payload (JSON).
    pub remote_catalog_url: Option<String>,
    /// URL to fetch remote detached signature envelope (.sig JSON).
    pub remote_signature_url: Option<String>,
    /// Keyring of trusted public keys for signature verification.
    pub keyring: Keyring,
    /// Allowed clock skew for timestamp checks in seconds.
    pub allowed_clock_skew_secs: u64,
    /// Timeout for remote HTTP operations.
    pub request_timeout: Duration,
    /// Maximum allowed HTTP response body size in bytes.
    pub max_response_bytes: usize,
}

impl Default for CatalogConfig {
    fn default() -> Self {
        Self {
            cache_path: None,
            remote_catalog_url: None,
            remote_signature_url: None,
            keyring: Keyring::embedded_default(),
            allowed_clock_skew_secs: 300,
            request_timeout: Duration::from_secs(10),
            max_response_bytes: 5 * 1024 * 1024,
        }
    }
}

/// Observable runtime status of the catalog manager.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogStatus {
    pub active_version: CatalogVersion,
    pub active_source: CatalogSource,
    pub lkg_version: Option<CatalogVersion>,
    pub generated_at: Option<u64>,
    pub loaded_at: u64,
    pub stale: bool,
    pub last_refresh_at: Option<u64>,
    pub last_refresh_success: bool,
    pub last_refresh_duration_ms: Option<u64>,
    pub last_rejection_reason: Option<String>,
    pub last_error: Option<String>,
    pub model_count: usize,
    pub provider_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshEnqueue {
    Accepted,
    Coalesced,
}

/// Manages the offline-first catalog lifecycle:
/// embedded -> LKG disk cache -> background remote signed update.
pub struct CatalogManager {
    config: CatalogConfig,
    active_snapshot: ArcSwap<RegistrySnapshot>,
    lkg_cache: LkgCache,
    status: Arc<RwLock<CatalogStatus>>,
    http_client: reqwest::Client,
    unified_runtime:
        Arc<RwLock<Option<std::sync::Weak<crate::runtime_state::UnifiedRuntimeState>>>>,
    last_sig_etag: Arc<RwLock<Option<String>>>,
    last_cat_etag: Arc<RwLock<Option<String>>>,
    refresh_in_flight: AtomicBool,
    refresh_completion_seq: AtomicU64,
    refresh_completed: tokio::sync::Notify,
    metrics: Arc<GatewayMetrics>,
}

pub type RuntimeCatalog = CatalogManager;

impl CatalogManager {
    /// Boot the catalog manager synchronously following the offline-first loading order:
    /// 1. Embedded fallback catalog is loaded.
    /// 2. If valid LKG exists on disk with version >= embedded, LKG is adopted.
    /// 3. If LKG is missing or corrupted, falls back gracefully to embedded catalog without startup failure.
    pub fn boot(config: CatalogConfig, now: u64) -> Self {
        Self::boot_with_metrics(config, now, Arc::new(GatewayMetrics::default()))
    }

    pub fn boot_with_metrics(
        config: CatalogConfig,
        now: u64,
        metrics: Arc<GatewayMetrics>,
    ) -> Self {
        let embedded = mahoquot_registry::embedded_registry_snapshot()
            .expect("embedded catalog must be valid JSON and pass domain invariants");

        let lkg_path = config
            .cache_path
            .clone()
            .unwrap_or_else(LkgCache::default_path);
        let lkg_cache = LkgCache::new(lkg_path);

        let mut active = embedded.clone();
        let mut lkg_version = None;
        let mut generated_at = None;
        let mut last_error = None;

        match lkg_cache.load_with_generated_at(&config.keyring, now, config.allowed_clock_skew_secs)
        {
            Ok((lkg_snapshot, lkg_generated_at)) => {
                let lkg_ver = lkg_snapshot.version();
                generated_at = Some(lkg_generated_at);
                lkg_version = Some(lkg_ver);
                if lkg_ver >= embedded.version() {
                    active = lkg_snapshot;
                } else {
                    tracing::warn!(
                        "LKG catalog version ({lkg_ver}) is older than embedded version ({}); using embedded",
                        embedded.version()
                    );
                }
            }
            Err(CatalogError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(
                    "No LKG catalog cache found at {}; booting with embedded catalog",
                    lkg_cache.path().display()
                );
            }
            Err(err) => {
                tracing::warn!(
                    "LKG catalog cache at {} is invalid or corrupted: {err}; falling back gracefully to embedded catalog",
                    lkg_cache.path().display()
                );
                last_error = Some(format!("LKG cache corrupted: {err}"));
            }
        }

        let status = CatalogStatus {
            active_version: active.version(),
            active_source: active.source(),
            lkg_version,
            generated_at,
            loaded_at: now,
            stale: last_error.is_some(),
            last_refresh_at: None,
            last_refresh_success: last_error.is_none(),
            last_refresh_duration_ms: None,
            last_rejection_reason: None,
            last_error,
            model_count: active.models().len(),
            provider_count: active.providers().len(),
        };
        record_cache_source(&metrics, active.source());

        let http_client = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .unwrap_or_default();

        Self {
            config,
            active_snapshot: ArcSwap::from_pointee(active),
            lkg_cache,
            status: Arc::new(RwLock::new(status)),
            http_client,
            unified_runtime: Arc::new(RwLock::new(None)),
            last_sig_etag: Arc::new(RwLock::new(None)),
            last_cat_etag: Arc::new(RwLock::new(None)),
            refresh_in_flight: AtomicBool::new(false),
            refresh_completion_seq: AtomicU64::new(0),
            refresh_completed: tokio::sync::Notify::new(),
            metrics,
        }
    }

    /// Bind this catalog manager to the unified runtime state so updates are published atomically.
    pub fn bind_runtime(&self, runtime: &Arc<crate::runtime_state::UnifiedRuntimeState>) {
        if let Ok(mut slot) = self.unified_runtime.write() {
            *slot = Some(Arc::downgrade(runtime));
        }
    }

    /// Read the currently active catalog snapshot lock-free.
    pub fn current_snapshot(&self) -> Arc<RegistrySnapshot> {
        if let Ok(slot) = self.unified_runtime.read() {
            if let Some(ref weak) = *slot {
                if let Some(runtime) = weak.upgrade() {
                    return runtime.load().registry.clone();
                }
            }
        }
        self.active_snapshot.load_full()
    }

    /// Return the active catalog version.
    pub fn active_version(&self) -> CatalogVersion {
        self.current_snapshot().version()
    }

    /// Return the active catalog source.
    pub fn active_source(&self) -> CatalogSource {
        self.current_snapshot().source()
    }

    /// Return a copy of the current status.
    pub fn status(&self) -> CatalogStatus {
        self.status
            .read()
            .map(|s| s.clone())
            .unwrap_or_else(|_| CatalogStatus {
                active_version: self.active_version(),
                active_source: self.active_source(),
                lkg_version: None,
                generated_at: None,
                loaded_at: 0,
                stale: true,
                last_refresh_at: None,
                last_refresh_success: false,
                last_refresh_duration_ms: None,
                last_rejection_reason: Some("internal".to_string()),
                last_error: Some("poisoned status lock".to_string()),
                model_count: 0,
                provider_count: 0,
            })
    }

    /// Path to the LKG cache file on disk.
    pub fn lkg_path(&self) -> &Path {
        self.lkg_cache.path()
    }

    /// Apply a verified remote catalog update:
    /// 1. Cryptographically verify signature, anti-downgrade threshold, timestamp skew, and domain invariants.
    /// 2. Write verified payload and envelope atomically to LKG disk cache via tempfile + sync + rename with 0600 mode.
    /// 3. Publish the new snapshot atomically into `active_snapshot`.
    /// 4. Update status.
    ///
    /// If verification or write fails, active snapshot and LKG cache remain unchanged.
    pub fn apply_verified_update(
        &self,
        envelope: &CatalogEnvelope,
        canonical_payload: &[u8],
        now: u64,
    ) -> Result<Arc<RegistrySnapshot>, CatalogError> {
        let current_active = self.active_snapshot.load();
        let lkg_ver = self.status().lkg_version;

        let mut verified_snapshot = verify_catalog_envelope(
            envelope,
            canonical_payload,
            &self.config.keyring,
            Some(current_active.version()),
            lkg_ver,
            now,
            self.config.allowed_clock_skew_secs,
        )?;

        // Write verified LKG cache atomically
        self.lkg_cache
            .write_atomically(envelope, canonical_payload)?;

        verified_snapshot.source = CatalogSource::RemoteSigned;
        let new_snapshot = Arc::new(verified_snapshot);

        // Atomic swap
        self.active_snapshot.store(new_snapshot.clone());

        // Publish to unified runtime state atomically if bound
        if let Ok(slot) = self.unified_runtime.read() {
            if let Some(ref weak) = *slot {
                if let Some(runtime) = weak.upgrade() {
                    if let Err(err) = runtime.update_registry(new_snapshot.clone()) {
                        tracing::warn!(
                            "Failed to publish verified catalog to unified runtime state: {err}"
                        );
                    }
                }
            }
        }

        // Update status
        if let Ok(mut status) = self.status.write() {
            status.active_version = new_snapshot.version();
            status.active_source = CatalogSource::RemoteSigned;
            status.lkg_version = Some(new_snapshot.version());
            status.generated_at = Some(envelope.generated_at);
            status.loaded_at = now;
            status.stale = false;
            status.last_refresh_at = Some(now);
            status.last_refresh_success = true;
            status.last_rejection_reason = None;
            status.last_error = None;
            status.model_count = new_snapshot.models().len();
            status.provider_count = new_snapshot.providers().len();
        }

        Ok(new_snapshot)
    }

    pub fn refresh_in_flight(&self) -> bool {
        self.refresh_in_flight.load(Ordering::SeqCst)
    }

    pub fn refresh_completion_seq(&self) -> u64 {
        self.refresh_completion_seq.load(Ordering::SeqCst)
    }

    pub async fn wait_for_refresh_after(&self, sequence: u64) {
        loop {
            let notified = self.refresh_completed.notified();
            if self.refresh_completion_seq.load(Ordering::SeqCst) > sequence {
                return;
            }
            notified.await;
        }
    }

    pub fn enqueue_refresh(self: &Arc<Self>) -> RefreshEnqueue {
        if self
            .refresh_in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            self.metrics
                .registry_refresh
                .rejected_coalesced
                .fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                reason = "coalesced",
                "model registry refresh trigger rejected"
            );
            return RefreshEnqueue::Coalesced;
        }

        let manager = Arc::clone(self);
        tokio::spawn(async move {
            let _ = manager.fetch_and_update().await;
            manager.refresh_in_flight.store(false, Ordering::SeqCst);
            manager
                .refresh_completion_seq
                .fetch_add(1, Ordering::SeqCst);
            manager.refresh_completed.notify_waiters();
        });
        RefreshEnqueue::Accepted
    }

    /// Fetch remote signed catalog and detached signature, verify them, and apply the update.
    pub async fn fetch_and_update(&self) -> Result<Arc<RegistrySnapshot>, CatalogError> {
        self.metrics
            .registry_refresh
            .attempts
            .fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let catalog_url = self
            .config
            .remote_catalog_url
            .as_deref()
            .unwrap_or(DEFAULT_REMOTE_CATALOG_URL);
        let signature_url = self
            .config
            .remote_signature_url
            .as_deref()
            .unwrap_or(DEFAULT_REMOTE_SIGNATURE_URL);

        validate_url(catalog_url)?;
        validate_url(signature_url)?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();

        let result = self
            .fetch_and_apply_inner(catalog_url, signature_url, now)
            .await;
        let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        self.metrics
            .registry_refresh
            .duration_milliseconds
            .fetch_add(duration_ms, Ordering::Relaxed);
        match &result {
            Ok(snapshot) => {
                self.metrics
                    .registry_refresh
                    .successes
                    .fetch_add(1, Ordering::Relaxed);
                record_cache_source(&self.metrics, snapshot.source());
                if let Ok(mut status) = self.status.write() {
                    status.last_refresh_duration_ms = Some(duration_ms);
                    status.last_refresh_at = Some(now);
                    status.last_refresh_success = true;
                    status.stale = false;
                    status.last_rejection_reason = None;
                    status.last_error = None;
                }
                tracing::info!(
                    outcome = "success",
                    duration_ms,
                    cache_source = %snapshot.source(),
                    "model registry refresh completed"
                );
            }
            Err(err) => {
                self.metrics
                    .registry_refresh
                    .errors
                    .fetch_add(1, Ordering::Relaxed);
                let reason = rejection_reason(err);
                if let Ok(mut status) = self.status.write() {
                    status.stale = true;
                    status.last_refresh_at = Some(now);
                    status.last_refresh_success = false;
                    status.last_refresh_duration_ms = Some(duration_ms);
                    status.last_rejection_reason = Some(reason.to_string());
                    status.last_error = Some(err.to_string());
                }
                tracing::warn!(
                    outcome = "error",
                    rejection_reason = reason,
                    duration_ms,
                    "model registry refresh failed"
                );
            }
        }
        result
    }

    async fn fetch_and_apply_inner(
        &self,
        catalog_url: &str,
        signature_url: &str,
        now: u64,
    ) -> Result<Arc<RegistrySnapshot>, CatalogError> {
        // 1. Fetch detached signature envelope with optional conditional GET
        let mut sig_req = self.http_client.get(signature_url);
        if let Ok(guard) = self.last_sig_etag.read() {
            if let Some(ref etag) = *guard {
                sig_req = sig_req.header(reqwest::header::IF_NONE_MATCH, etag);
            }
        }

        let sig_resp = sig_req
            .send()
            .await
            .map_err(|e| CatalogError::Http(format!("failed to fetch signature: {e}")))?;

        if sig_resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(self.current_snapshot());
        }

        let sig_resp = sig_resp
            .error_for_status()
            .map_err(|e| CatalogError::Http(format!("signature HTTP status error: {e}")))?;

        let sig_etag = sig_resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string());

        let sig_bytes = read_stream_bounded_body(sig_resp, self.config.max_response_bytes, "signature")
            .await?;

        let sig_text = String::from_utf8(sig_bytes)
            .map_err(|e| CatalogError::Http(format!("signature not UTF-8: {e}")))?;
        let envelope = CatalogEnvelope::from_json(&sig_text)?;

        // 2. Fetch catalog payload with optional conditional GET
        let mut cat_req = self.http_client.get(catalog_url);
        if let Ok(guard) = self.last_cat_etag.read() {
            if let Some(ref etag) = *guard {
                cat_req = cat_req.header(reqwest::header::IF_NONE_MATCH, etag);
            }
        }

        let cat_resp = cat_req
            .send()
            .await
            .map_err(|e| CatalogError::Http(format!("failed to fetch catalog: {e}")))?;

        if cat_resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(self.current_snapshot());
        }

        let cat_resp = cat_resp
            .error_for_status()
            .map_err(|e| CatalogError::Http(format!("catalog HTTP status error: {e}")))?;

        let cat_etag = cat_resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string());

        let payload_bytes = read_stream_bounded_body(cat_resp, self.config.max_response_bytes, "catalog")
            .await?;

        // 3. Verify and atomically apply
        if envelope.catalog_version == self.active_version() {
            // Equal version re-fetch: verify signature and treat as successful up-to-date no-op
            verify_catalog_envelope(
                &envelope,
                &payload_bytes,
                &self.config.keyring,
                None,
                None,
                now,
                self.config.allowed_clock_skew_secs,
            )?;
            if let (Ok(mut s_guard), Ok(mut c_guard)) =
                (self.last_sig_etag.write(), self.last_cat_etag.write())
            {
                *s_guard = sig_etag;
                *c_guard = cat_etag;
            }
            return Ok(self.current_snapshot());
        }

        let result = self.apply_verified_update(&envelope, &payload_bytes, now);
        if result.is_ok() {
            if let (Ok(mut s_guard), Ok(mut c_guard)) =
                (self.last_sig_etag.write(), self.last_cat_etag.write())
            {
                *s_guard = sig_etag;
                *c_guard = cat_etag;
            }
        }
        result
    }
}

async fn read_stream_bounded_body(
    mut resp: reqwest::Response,
    max_bytes: usize,
    context: &str,
) -> Result<Vec<u8>, CatalogError> {
    if let Some(content_length) = resp.content_length() {
        if content_length > max_bytes as u64 {
            return Err(CatalogError::Http(format!(
                "{context} body size {content_length} exceeds cap {max_bytes}"
            )));
        }
    }

    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| CatalogError::Http(format!("failed to read {context} bytes: {e}")))?
    {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(CatalogError::Http(format!(
                "{context} body size {} exceeds cap {max_bytes}",
                body.len().saturating_add(chunk.len())
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn rejection_reason(error: &CatalogError) -> &'static str {
    match error {
        CatalogError::Io(_) => "io",
        CatalogError::Serialization(_) => "serialization",
        CatalogError::Registry(_) => "registry",
        CatalogError::Verification(_) => "verification",
        CatalogError::Http(_) => "http",
        CatalogError::InvalidState(_) => "invalid_state",
    }
}

fn record_cache_source(metrics: &GatewayMetrics, source: CatalogSource) {
    let counter = match source {
        CatalogSource::EmbeddedFallback => &metrics.registry_refresh.source_embedded_fallback,
        CatalogSource::LkgCache => &metrics.registry_refresh.source_lkg_cache,
        CatalogSource::RemoteSigned => &metrics.registry_refresh.source_remote_signed,
        CatalogSource::Discovered => &metrics.registry_refresh.source_discovered,
        CatalogSource::LocalOverride => &metrics.registry_refresh.source_local_override,
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

fn validate_url(url_str: &str) -> Result<(), CatalogError> {
    let parsed = reqwest::Url::parse(url_str)
        .map_err(|e| CatalogError::Http(format!("invalid catalog URL '{url_str}': {e}")))?;

    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(CatalogError::Http(
            "catalog URL must not contain credentials".to_string(),
        ));
    }

    match parsed.scheme() {
        "https" => Ok(()),
        "http" => {
            // Allow loopback / localhost for local test environments
            let host = parsed.host_str().unwrap_or_default();
            if host == "127.0.0.1" || host == "localhost" || host == "::1" {
                Ok(())
            } else {
                Err(CatalogError::Http(format!(
                    "insecure HTTP URL not allowed for remote catalog: {url_str}"
                )))
            }
        }
        other => Err(CatalogError::Http(format!(
            "unsupported URL scheme '{other}' in {url_str}"
        ))),
    }
}
