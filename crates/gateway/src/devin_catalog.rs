//! Account-scoped model discovery, cache, and atomic routing integration for Devin.
//!
//! Protocol & transport invariants (see docs/devin-wire-contract.md):
//! - Unary RPC: `POST /exa.api_server_pb.ApiServerService/GetCascadeModelConfigs`
//! - Framing: Direct, raw Protocol Buffer message bytes without Connect envelope (`application/proto`).
//! - Headers:
//!   - `Content-Type: application/proto`
//!   - `Accept: application/proto`
//!   - `Authorization: Basic <token>-<token>` (literal repeated token, never base64)
//!   - `Connect-Protocol-Version: 1`
//! - Request body: `GetCascadeModelConfigsRequest` protobuf message with `metadata.api_key = <token>`.
//! - Response body: `GetCascadeModelConfigsResponse` containing repeated `ClientModelConfig`.
//! - Incremental bounded read: streaming body read chunk-by-chunk up to 16 MiB limit; aborts immediately on overflow.
//! - Cache:
//!   - State-owned `ArcSwap` map, zero mutable globals, lock-free management read path.
//!   - Key: exact `(identity_slug, credential_revision, base_url)`.
//!   - TTL: 5 minutes (300 seconds).
//!   - Stale reads: if expired, stale data is returned while async refresh triggers.
//!   - Transient failure: preserves previous valid model list, marked `stale: true`, records safe fixed `last_error`.
//!   - First failure: empty model list, does not introduce routing.
//!   - Credential rotation & races: generation-validated publication ensures out-of-order in-flight RPCs
//!     never publish obsolete authorization if credentials or identity rotated during the call.
//! - Model semantics:
//!   - Public IDs: `devin/<exact model_uid>` (e.g. `devin/glm-5-2`, `devin/swe-1-7`).
//!   - Exact upstream UIDs validated without mutating/trimming; variants/suffixes preserved verbatim.
//!   - Exclude disabled models (`disabled == true`).
//!   - `supports_images: true` indicates vision input capability, NOT `ModelCapability::Image` (image generation).
//!   - Raw proto metadata (`credit_multiplier` as original float, premium, promo, capacity) preserved as
//!     observed metadata, never as confirmed quotas or billing pricing claims.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use prost::Message;
use serde::{Deserialize, Serialize};

use crate::account::AccountMember;
use crate::compat::devin_proto::{GetCascadeModelConfigsRequest, GetCascadeModelConfigsResponse, Metadata};

/// 5-minute TTL for model discovery cache.
pub const DEVIN_CATALOG_TTL_SECS: u64 = 300;
pub const DEVIN_CATALOG_TTL: Duration = Duration::from_secs(DEVIN_CATALOG_TTL_SECS);

/// 16 MiB maximum payload limit for discovery response bodies.
pub const MAX_DISCOVERY_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Computes a stable, non-secret credential revision string from the session token.
pub fn compute_credential_revision(token: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    token.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Cache key uniquely identified by account identity slug, credential revision, and base URL.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct DevinCacheKey {
    pub identity_slug: String,
    pub credential_revision: String,
    pub credential_token: String,
    pub base_url: String,
}

impl DevinCacheKey {
    pub fn new(identity_slug: impl Into<String>, token: &str, base_url: impl Into<String>) -> Self {
        Self {
            identity_slug: identity_slug.into(),
            credential_revision: compute_credential_revision(token),
            credential_token: token.to_string(),
            base_url: base_url.into(),
        }
    }
}

/// Discovered model metadata from upstream `ClientModelConfig`.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, Default)]
pub struct DiscoveredDevinModel {
    /// Upstream model identifier (e.g., `"glm-5-2"`), preserved verbatim without trimming.
    pub model_uid: String,
    /// Canonical public gateway identifier (e.g., `"devin/glm-5-2"`).
    pub public_id: String,
    /// Display label from upstream.
    pub label: String,
    /// Multimodal vision input support (NOT image generation capability).
    pub supports_images: bool,
    /// Premium account entitlement flag (observed metadata).
    pub is_premium: bool,
    /// Beta model flag (observed metadata).
    pub is_beta: bool,
    /// Recommended model flag (observed metadata).
    pub is_recommended: bool,
    /// New model flag (observed metadata).
    pub is_new: bool,
    /// Capacity limit flag (observed metadata).
    pub is_capacity_limited: bool,
    /// Promotional status active flag (observed metadata).
    pub promo_active: bool,
    /// Context / token threshold declared by upstream (observed metadata, not server max).
    pub max_tokens: Option<i32>,
    /// Billing credit multiplier if reported, preserving original float metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credit_multiplier: Option<f32>,
    /// Model description if reported.
    pub description: Option<String>,
}

impl DiscoveredDevinModel {
    pub fn new(model_uid: impl Into<String>, label: impl Into<String>) -> Self {
        let uid = model_uid.into();
        Self {
            public_id: format!("devin/{uid}"),
            model_uid: uid,
            label: label.into(),
            supports_images: false,
            is_premium: false,
            is_beta: false,
            is_recommended: false,
            is_new: false,
            is_capacity_limited: false,
            promo_active: false,
            max_tokens: Some(200_000),
            credit_multiplier: Some(1.0),
            description: None,
        }
    }
}

/// Per-account cached discovery state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DevinAccountCatalogState {
    pub key: DevinCacheKey,
    pub sequence: u64,
    pub models: Vec<DiscoveredDevinModel>,
    #[serde(skip)]
    pub cached_at: Option<Instant>,
    pub cached_at_unix: u64,
    pub expires_at_unix: u64,
    pub last_refresh_at: Option<u64>,
    pub last_error: Option<String>,
    pub stale: bool,
    pub has_succeeded: bool,
}

impl DevinAccountCatalogState {
    pub fn new_success(
        key: DevinCacheKey,
        models: Vec<DiscoveredDevinModel>,
        now_unix: u64,
    ) -> Self {
        Self {
            key,
            sequence: 0,
            models,
            cached_at: Some(Instant::now()),
            cached_at_unix: now_unix,
            expires_at_unix: now_unix + DEVIN_CATALOG_TTL_SECS,
            last_refresh_at: Some(now_unix),
            last_error: None,
            stale: false,
            has_succeeded: true,
        }
    }

    pub fn with_sequence(mut self, sequence: u64) -> Self {
        self.sequence = sequence;
        self
    }

    pub fn initial_failure(key: DevinCacheKey, err_msg: &'static str, now_unix: u64) -> Self {
        Self {
            key,
            sequence: 0,
            models: Vec::new(),
            cached_at: Some(Instant::now()),
            cached_at_unix: now_unix,
            expires_at_unix: now_unix,
            last_refresh_at: Some(now_unix),
            last_error: Some(err_msg.to_string()),
            stale: true,
            has_succeeded: false,
        }
    }

    pub fn is_stale(&self, now_unix: u64) -> bool {
        self.stale || now_unix >= self.expires_at_unix
    }

    pub fn record_transient_failure(&mut self, err_msg: &'static str, now_unix: u64) {
        self.stale = true;
        self.last_error = Some(err_msg.to_string());
        self.last_refresh_at = Some(now_unix);
    }
}

/// Typed discovery error codes avoiding credential-bearing endpoint reflection.
#[derive(Debug, thiserror::Error)]
pub enum DevinDiscoveryError {
    #[error("upstream network connection failed")]
    Network,
    #[error("upstream request timed out")]
    Timeout,
    #[error("upstream returned invalid content type; expected application/proto")]
    InvalidContentType,
    #[error("upstream response body exceeded 16 MiB limit")]
    BodyTooLarge,
    #[error("upstream returned non-success HTTP status {status}")]
    HttpStatus { status: reqwest::StatusCode },
    #[error("upstream returned malformed protobuf response")]
    Decode,
    #[error("discovery publication aborted due to concurrent credential rotation")]
    StalePublication,
    #[error("failed to build upstream HTTP client")]
    ClientBuild,
    #[error("internal discovery error")]
    Internal(&'static str),
}

impl DevinDiscoveryError {
    pub fn safe_message(&self) -> &'static str {
        match self {
            Self::Network => "upstream network connection failed",
            Self::Timeout => "upstream request timed out",
            Self::InvalidContentType => "upstream returned invalid content type; expected application/proto",
            Self::BodyTooLarge => "upstream response body exceeded 16 MiB limit",
            Self::HttpStatus { status } => match status.as_u16() {
                401 => "upstream authentication failed (HTTP 401)",
                403 => "upstream access forbidden (HTTP 403)",
                404 => "upstream endpoint not found (HTTP 404)",
                429 => "upstream rate limit exceeded (HTTP 429)",
                500..=599 => "upstream server error",
                _ => "upstream returned HTTP error status",
            },
            Self::Decode => "upstream returned malformed protobuf response",
            Self::StalePublication => "discovery publication aborted due to concurrent credential change",
            Self::ClientBuild => "failed to build upstream HTTP client",
            Self::Internal(_) => "internal discovery error",
        }
    }
}

impl From<reqwest::Error> for DevinDiscoveryError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            Self::Timeout
        } else {
            Self::Network
        }
    }
}

impl From<prost::DecodeError> for DevinDiscoveryError {
    fn from(_: prost::DecodeError) -> Self {
        Self::Decode
    }
}

/// Reads response stream chunk-by-chunk with bounded allocation up to `max_bytes`.
pub async fn read_bounded_body(
    resp: &mut reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, DevinDiscoveryError> {
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|err| {
        if err.is_timeout() {
            DevinDiscoveryError::Timeout
        } else {
            DevinDiscoveryError::Network
        }
    })? {
        if buf.len().saturating_add(chunk.len()) > max_bytes {
            return Err(DevinDiscoveryError::BodyTooLarge);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Unframed protobuf discovery client making unary RPC to `GetCascadeModelConfigs`.
/// Returns true if the Content-Type header has MIME essence `application/proto` (case-insensitive),
/// allowing optional MIME parameters (e.g. `; charset=utf-8`).
pub fn is_proto_content_type(content_type: &str) -> bool {
    let essence = content_type.split(';').next().map(str::trim).unwrap_or("");
    essence.eq_ignore_ascii_case("application/proto")
}

pub async fn fetch_devin_model_catalog(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
) -> Result<Vec<DiscoveredDevinModel>, DevinDiscoveryError> {
    tokio::time::timeout(
        DEVIN_CATALOG_TTL.min(Duration::from_secs(10)),
        fetch_devin_model_catalog_inner(client, base_url, token),
    )
    .await
    .map_err(|_| DevinDiscoveryError::Timeout)?
}

async fn fetch_devin_model_catalog_inner(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
) -> Result<Vec<DiscoveredDevinModel>, DevinDiscoveryError> {
    let clean_base = base_url.trim_end_matches('/');
    let target_url = format!("{clean_base}/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs");

    let req = GetCascadeModelConfigsRequest {
        metadata: Some(Metadata {
            api_key: Some(token.to_string()),
            ..Default::default()
        }),
    };
    let mut req_body = Vec::new();
    req.encode(&mut req_body)
        .map_err(|_| DevinDiscoveryError::Internal("failed to encode protobuf request"))?;

    let auth_header = format!("Basic {token}-{token}");

    let mut resp = client
        .post(&target_url)
        .header("Content-Type", "application/proto")
        .header("Accept", "application/proto")
        .header("Connect-Protocol-Version", "1")
        .header("Authorization", auth_header)
        .body(req_body)
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        return Err(DevinDiscoveryError::HttpStatus { status });
    }

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok());
    if !content_type.is_some_and(is_proto_content_type) {
        return Err(DevinDiscoveryError::InvalidContentType);
    }

    let bytes = read_bounded_body(&mut resp, MAX_DISCOVERY_RESPONSE_BYTES).await?;

    let parsed_resp = GetCascadeModelConfigsResponse::decode(bytes.as_slice())?;

    let mut models = Vec::new();
    for config in parsed_resp.client_model_configs {
        if config.disabled.unwrap_or(false) {
            continue;
        }
        // Exact UID validation without mutating or trimming
        let Some(raw_uid) = &config.model_uid else {
            continue;
        };
        if raw_uid.is_empty()
            || raw_uid.contains(char::is_whitespace)
            || raw_uid.contains('/')
            || raw_uid.contains('\0')
        {
            continue;
        }

        let promo_active = config
            .promo_status
            .as_ref()
            .and_then(|p| p.is_active)
            .unwrap_or(false);

        models.push(DiscoveredDevinModel {
            model_uid: raw_uid.clone(),
            public_id: format!("devin/{raw_uid}"),
            label: config.label.unwrap_or_else(|| raw_uid.clone()),
            supports_images: config.supports_images.unwrap_or(false),
            is_premium: config.is_premium.unwrap_or(false),
            is_beta: config.is_beta.unwrap_or(false),
            is_recommended: config.is_recommended.unwrap_or(false),
            is_new: config.is_new.unwrap_or(false),
            is_capacity_limited: config.is_capacity_limited.unwrap_or(false),
            promo_active,
            max_tokens: config.max_tokens,
            credit_multiplier: config.credit_multiplier,
            description: config.description,
        });
    }

    Ok(models)
}

/// The set of account ids currently present in the pool. The cache keeps this
/// allowlist inside the same swapped value as its entries, so a prune can never
/// be undone by an insert that raced the pool deletion.
pub type ActiveIds = std::collections::BTreeSet<String>;

/// One `ArcSwap` payload: the live-id allowlist plus the cached catalogs. The
/// allowlist lives inside the swapped value so a prune can never be undone by an
/// insert that raced the pool deletion. `active` is `None` until
/// `retain_accounts` activates membership, so a standalone cache keeps accepting
/// inserts until the pool is explicit.
#[derive(Default)]
struct DevinCacheState {
    active: Option<ActiveIds>,
    entries: HashMap<DevinCacheKey, Arc<DevinAccountCatalogState>>,
}

/// State-owned discovery cache backed by `ArcSwap` for lock-free reads on the management hot-path.
pub struct DevinDiscoveryCache {
    state: ArcSwap<DevinCacheState>,
}

impl Default for DevinDiscoveryCache {
    fn default() -> Self {
        Self::new()
    }
}

impl DevinDiscoveryCache {
    pub fn new() -> Self {
        Self {
            state: ArcSwap::from_pointee(DevinCacheState::default()),
        }
    }

    pub fn get(&self, key: &DevinCacheKey) -> Option<Arc<DevinAccountCatalogState>> {
        self.state.load().entries.get(key).cloned()
    }

    /// Publishes `state` under `key` in one `rcu` swap: the membership check and
    /// the write share a single closure, so a concurrent `retain_accounts` can
    /// never be interleaved between them.
    ///
    /// The swap keeps only the newest revision per identity slug (an older
    /// revision of the same slug is dropped) and skips keys whose slug is not in
    /// the active allowlist. Before `retain_accounts` has activated membership
    /// the allowlist is `None`, so the entry is accepted instead of silently
    /// disabling discovery until the first reconcile.
    ///
    /// Out-of-order publication is rejected before it reaches here, by the
    /// request-sequence guard in `RuntimeState::publish_devin_member_catalog`.
    pub fn insert(&self, key: DevinCacheKey, state: Arc<DevinAccountCatalogState>) {
        self.state.rcu(|current| {
            let mut next = DevinCacheState {
                active: current.active.clone(),
                entries: current.entries.clone(),
            };
            let slug = key.identity_slug.clone();
            if next
                .active
                .as_ref()
                .is_some_and(|active| !active.contains(&slug))
            {
                return Arc::new(next);
            }
            next.entries.retain(|k, _| k.identity_slug != slug);
            next.entries.insert(key.clone(), Arc::clone(&state));
            Arc::new(next)
        });
    }

    /// Publishes the live-id allowlist and prunes every catalog whose identity
    /// slug is no longer live, all in one `rcu` swap. This is the membership
    /// activation point. Returns the number of accounts pruned.
    pub fn retain_accounts(&self, active: &ActiveIds) -> usize {
        let pruned = std::cell::Cell::new(0usize);
        self.state.rcu(|current| {
            let mut entries = current.entries.clone();
            entries.retain(|k, _| active.contains(&k.identity_slug));
            pruned.set(current.entries.len() - entries.len());
            Arc::new(DevinCacheState {
                active: Some(active.clone()),
                entries,
            })
        });
        pruned.get()
    }

    /// Identity slugs that currently hold a cached catalog (sorted, deduped).
    pub fn tracked_accounts(&self) -> Vec<String> {
        let state = self.state.load();
        let mut slugs: Vec<String> = state
            .entries
            .keys()
            .map(|k| k.identity_slug.clone())
            .collect();
        slugs.sort();
        slugs.dedup();
        slugs
    }
}

/// Refreshes model discovery for an AccountMember with generation-validated publication.
///
/// Ensures concurrent credential rotations or account renames during in-flight RPCs
/// discard stale results rather than writing obsolete authorization.
pub async fn refresh_account_models(
    member: &AccountMember,
    client: &reqwest::Client,
    state_cache: Option<&DevinDiscoveryCache>,
) -> Result<Arc<DevinAccountCatalogState>, DevinDiscoveryError> {
    if member.kind() != crate::account::ProviderKind::Devin {
        return Err(DevinDiscoveryError::Internal("account is not a Devin account"));
    }

    // Capture pre-await snapshot of credentials and identity
    let pre_identity = member.id.clone();
    let (pre_token, pre_base_url) = {
        let guard = member.inner.read().unwrap_or_else(|p| p.into_inner());
        match &*guard {
            crate::account::ProviderAccount::Devin(acct) => {
                let base = member
                    .upstream_override
                    .as_deref()
                    .unwrap_or(&acct.api_server_url)
                    .to_string();
                (acct.access_token.clone(), base)
            }
            _ => return Err(DevinDiscoveryError::Internal("not a devin account")),
        }
    };
    let pre_revision = compute_credential_revision(&pre_token);
    let key = DevinCacheKey {
        identity_slug: pre_identity.clone(),
        credential_revision: pre_revision.clone(),
        credential_token: pre_token.clone(),
        base_url: pre_base_url.clone(),
    };

    // Sample strictly monotonic request sequence before issuing upstream RPC
    let request_seq = member.devin_discovery_seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;

    let fetch_res = fetch_devin_model_catalog(client, &pre_base_url, &pre_token).await;

    // Generation-validated publication: verify credentials and identity have NOT changed
    let post_identity = member.id.clone();
    let (post_token, post_base_url) = {
        let guard = member.inner.read().unwrap_or_else(|p| p.into_inner());
        match &*guard {
            crate::account::ProviderAccount::Devin(acct) => {
                let base = member
                    .upstream_override
                    .as_deref()
                    .unwrap_or(&acct.api_server_url)
                    .to_string();
                (acct.access_token.clone(), base)
            }
            _ => return Err(DevinDiscoveryError::Internal("not a devin account")),
        }
    };
    let post_revision = compute_credential_revision(&post_token);

    if member.is_manually_disabled()
        || pre_identity != post_identity
        || pre_token != post_token
        || pre_revision != post_revision
        || pre_base_url != post_base_url
    {
        return Err(DevinDiscoveryError::StalePublication);
    }

    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    match fetch_res {
        Ok(models) => {
            let state = Arc::new(DevinAccountCatalogState::new_success(key.clone(), models, now_unix).with_sequence(request_seq));
            Ok(state)
        }
        Err(err) => {
            let safe_err = err.safe_message();
            let existing = member.devin_catalog_state().or_else(|| {
                state_cache.and_then(|c| c.get(&key))
            });
            if let Some(existing) = existing {
                if existing.key == key && existing.has_succeeded {
                    let mut updated = (*existing).clone();
                    updated.record_transient_failure(safe_err, now_unix);
                    updated.sequence = request_seq;
                    let arc_updated = Arc::new(updated);
                    return Ok(arc_updated);
                }
            }
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::ProviderAccount;

    fn cache_key(slug: &str, token: &str) -> DevinCacheKey {
        DevinCacheKey::new(slug, token, "http://127.0.0.1:18899")
    }

    /// Deterministic fixture port: the project reserves 18840-18899 for tests, so
    /// the stub never binds an arbitrary ephemeral port (and never a production one).
    async fn bind_test_listener() -> tokio::net::TcpListener {
        for port in 18840..=18899 {
            match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
                Ok(listener) => return listener,
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
                Err(error) => panic!("fixture bind failed on {port}: {error}"),
            }
        }
        panic!("no available fixture port in 18840-18899");
    }

    fn catalog(key: &DevinCacheKey, now_unix: u64) -> Arc<DevinAccountCatalogState> {
        Arc::new(DevinAccountCatalogState::new_success(
            key.clone(),
            vec![DiscoveredDevinModel::new("glm-5-2", "GLM 5.2")],
            now_unix,
        ))
    }

    #[test]
    fn devin_cache_insert_replaces_older_revisions_and_skips_inactive_slugs() {
        let cache = DevinDiscoveryCache::new();
        let live: ActiveIds = ["devin-live".to_string()].into_iter().collect();
        assert_eq!(cache.retain_accounts(&live), 0);

        let older = cache_key("devin-live", "token-1");
        cache.insert(older.clone(), catalog(&older, 1_726_000_000));
        assert!(cache.get(&older).is_some());

        // Credential rotation: the newer revision replaces the older revision of
        // the same identity slug instead of accumulating a second entry.
        let newer = cache_key("devin-live", "token-2");
        cache.insert(newer.clone(), catalog(&newer, 1_726_000_100));
        assert!(cache.get(&older).is_none(), "older revision must be dropped");
        assert!(cache.get(&newer).is_some());
        assert_eq!(cache.tracked_accounts(), vec!["devin-live".to_string()]);

        // A slug that is not in the allowlist is refused outright.
        let gone = cache_key("devin-gone", "token-3");
        cache.insert(gone.clone(), catalog(&gone, 1_726_000_200));
        assert!(cache.get(&gone).is_none(), "inactive slug must be skipped");
        assert_eq!(cache.tracked_accounts(), vec!["devin-live".to_string()]);

        // Once the account leaves the pool, its catalog is pruned under the
        // same swap that publishes the new allowlist.
        assert_eq!(cache.retain_accounts(&ActiveIds::new()), 1);
        assert!(cache.get(&newer).is_none());
        assert!(cache.tracked_accounts().is_empty());
    }

    #[tokio::test]
    async fn devin_cache_keys_are_pool_account_ids() {
        // Local stub only: this test never contacts a real upstream.
        let app = axum::Router::new().route(
            "/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs",
            axum::routing::post(|| async {
                let resp = GetCascadeModelConfigsResponse {
                    client_model_configs: Vec::new(),
                };
                let mut body = Vec::new();
                resp.encode(&mut body).expect("encode proto response");
                axum::http::Response::builder()
                    .status(axum::http::StatusCode::OK)
                    .header("Content-Type", "application/proto")
                    .body(axum::body::Body::from(body))
                    .expect("build stub response")
            }),
        );
        let listener = bind_test_listener().await;
        let addr = listener.local_addr().expect("stub listener addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let account = mahoquot_providers::DevinAccount {
            provider_type: "devin".to_string(),
            identity_slug: "devin-pool-42".to_string(),
            label: Some("devin-pool-42".to_string()),
            email: None,
            access_token: "token-42".to_string(),
            api_server_url: format!("http://{addr}"),
            disabled: false,
        };
        let member = Arc::new(AccountMember::for_test_with_id(
            "devin-pool-42",
            ProviderAccount::Devin(account),
        ));
        let client = reqwest::Client::builder()
            .http1_only()
            .no_proxy()
            .build()
            .expect("build stub client");

        let cache = DevinDiscoveryCache::new();
        let state = refresh_account_models(&member, &client, Some(&cache))
            .await
            .expect("refresh against the local stub");

        // §9 identity evidence: the slug the refresh path produces is the pool
        // account id, never a user-supplied identity or display label.
        assert_eq!(state.key.identity_slug, member.id);
        assert_eq!(state.key.identity_slug, "devin-pool-42");

        cache.insert(state.key.clone(), Arc::clone(&state));
        let live: ActiveIds = [member.id.clone()].into_iter().collect();
        assert_eq!(cache.retain_accounts(&live), 0);
        assert_eq!(cache.tracked_accounts(), vec![member.id.clone()]);
        assert!(cache.get(&state.key).is_some());

        // The allowlist is keyed by pool account id, so any other slug is refused.
        let other = cache_key("user-supplied-slug", "token-9");
        cache.insert(other.clone(), catalog(&other, 1_726_000_000));
        assert!(cache.get(&other).is_none());

        server.abort();
        let _ = server.await;
    }
}
