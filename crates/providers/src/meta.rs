//! Meta Muse device authorization and DCA-to-API-key lifecycle.
//! Network clients are supplied by the caller so account proxy policy is shared.
use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const API_BASE: &str = "https://api.meta.ai/v1";
pub const DEVICE_URL: &str = "https://auth.meta.com/oidc/device/authorization/";
pub const TOKEN_URL: &str = "https://auth.meta.com/oidc/device/token/";
pub const MINT_URL: &str = "https://api.meta.ai/muse-code/key";
pub const CLIENT_ID: &str = "1031625952748946";
pub const AUTH_USER_AGENT: &str = "muse-code/1.0.2";
pub const API_USER_AGENT: &str =
    "muse-build/1.3.0 (interactive; macos-aarch64; build ac7280f2aca67769d1455a8847bb502b617d50f6)";
pub const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const ACQUISITION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_AUTHORIZATION: Duration = Duration::from_secs(900);

#[derive(Debug, thiserror::Error)]
pub enum MetaError {
    #[error("Meta transport failure: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Meta endpoint returned HTTP {0}")]
    Status(u16),
    #[error("Meta response missing or invalid {0}")]
    Invalid(&'static str),
    #[error("Meta device authorization denied")]
    Denied,
    #[error("Meta device authorization expired")]
    Expired,
    #[error("Meta authorization server rejected the grant")]
    Grant,
}

// Deliberately no Debug for types containing credentials.
#[derive(Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct MetaAccount {
    pub identity_slug: String,
    #[serde(rename = "type")]
    pub provider_type: String,
    pub auth_kind: String,
    pub access_token: String,
    pub api_key: String,
    pub dca_token: String,
    pub token_type: String,
    pub expires_in: u64,
    pub dca_expires_at: i64,
    pub dca_expired: String,
    pub expired: String,
    pub last_refresh: String,
    pub base_url: String,
    pub email: String,
    pub name: String,
    pub disabled: bool,
    pub models: Vec<String>,
    #[serde(flatten)]
    pub metadata: BTreeMap<String, Value>,
}

impl MetaAccount {
    pub fn api_token(&self) -> Option<&str> {
        [&self.api_key, &self.access_token]
            .into_iter()
            .map(|s| s.trim())
            .find(|s| !s.is_empty() && !s.starts_with("dca:"))
    }

    pub fn dca(&self) -> Option<&str> {
        if self.auth_kind == "api_key" {
            return None;
        }
        let explicit = self.dca_token.trim();
        if !explicit.is_empty() {
            return Some(explicit);
        }
        let legacy = self.access_token.trim();
        legacy.starts_with("dca:").then_some(legacy)
    }

    pub fn effective_base(&self) -> &str {
        if self.base_url.trim().is_empty() {
            API_BASE
        } else {
            self.base_url.trim()
        }
    }

    pub fn needs_mint(&self) -> bool {
        self.api_token().is_none() && self.dca().is_some()
    }

    pub fn is_expired(&self, now: i64) -> bool {
        // A minted key does not expire with the DCA token.
        self.api_token().is_none()
            && (self.dca().is_none() || (self.dca_expires_at > 0 && now >= self.dca_expires_at))
    }

    pub fn headers(&self, stream: bool) -> Result<Vec<(String, String)>, MetaError> {
        api_headers(
            self.api_token().ok_or(MetaError::Invalid("api_key"))?,
            stream,
        )
    }
}

pub fn api_headers(token: &str, stream: bool) -> Result<Vec<(String, String)>, MetaError> {
    if token.trim().is_empty() || token.trim().starts_with("dca:") {
        return Err(MetaError::Invalid("api_key"));
    }
    // No ingress header map is accepted: routing hints cannot enter this set.
    let mut headers = vec![
        ("authorization".into(), format!("Bearer {}", token.trim())),
        ("user-agent".into(), API_USER_AGENT.into()),
        ("x-client-id".into(), "tbh:tui".into()),
        ("content-type".into(), "application/json".into()),
        (
            "accept".into(),
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            }
            .into(),
        ),
    ];
    if stream {
        headers.push(("cache-control".into(), "no-cache".into()));
    }
    Ok(headers)
}

#[derive(Clone)]
pub struct MetaEndpoints {
    pub device: String,
    pub token: String,
    pub mint: String,
    /// Explicit test override shared by mint and catalog; never inferred from a token.
    pub upstream_override: Option<String>,
}

impl Default for MetaEndpoints {
    fn default() -> Self {
        Self {
            device: DEVICE_URL.into(),
            token: TOKEN_URL.into(),
            mint: MINT_URL.into(),
            upstream_override: None,
        }
    }
}

#[derive(Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    #[serde(default)]
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: String,
    #[serde(default)]
    pub expires_in: u64,
    #[serde(default)]
    pub interval: u64,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct DeviceToken {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: u64,
    pub expires_at: i64,
    pub error: String,
}

pub enum PollResult {
    Authorized(DeviceToken),
    Pending,
    SlowDown,
}

pub fn parse_poll(status: StatusCode, token: DeviceToken) -> Result<PollResult, MetaError> {
    if status == StatusCode::OK {
        if token.access_token.trim().is_empty() {
            return Err(MetaError::Invalid("access_token"));
        }
        return Ok(PollResult::Authorized(token));
    }
    match token.error.as_str() {
        "authorization_pending" => Ok(PollResult::Pending),
        "slow_down" => Ok(PollResult::SlowDown),
        "access_denied" => Err(MetaError::Denied),
        "expired_token" => Err(MetaError::Expired),
        "" => Err(MetaError::Status(status.as_u16())),
        _ => Err(MetaError::Grant),
    }
}

pub async fn start_device(
    client: &Client,
    endpoints: &MetaEndpoints,
) -> Result<DeviceCode, MetaError> {
    let response = client
        .post(&endpoints.device)
        .timeout(ACQUISITION_TIMEOUT)
        .header("user-agent", AUTH_USER_AGENT)
        .header("accept", "application/json")
        .form(&[("client_id", CLIENT_ID)])
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(MetaError::Status(response.status().as_u16()));
    }
    let device: DeviceCode = response.json().await?;
    if device.device_code.trim().is_empty() || device.user_code.trim().is_empty() {
        return Err(MetaError::Invalid("device_code/user_code"));
    }
    Ok(device)
}

pub async fn poll_device(
    client: &Client,
    endpoints: &MetaEndpoints,
    device: &DeviceCode,
) -> Result<PollResult, MetaError> {
    if device.device_code.trim().is_empty() {
        return Err(MetaError::Invalid("device_code"));
    }
    let response = client
        .post(&endpoints.token)
        .timeout(ACQUISITION_TIMEOUT)
        .header("user-agent", AUTH_USER_AGENT)
        .header("accept", "application/json")
        .form(&[
            ("grant_type", DEVICE_GRANT),
            ("device_code", device.device_code.as_str()),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .await?;
    let status = response.status();
    parse_poll(status, response.json().await?)
}

pub async fn wait_for_authorization(
    client: &Client,
    endpoints: &MetaEndpoints,
    device: &DeviceCode,
) -> Result<DeviceToken, MetaError> {
    let duration = if device.expires_in == 0 {
        MAX_AUTHORIZATION
    } else {
        Duration::from_secs(device.expires_in).min(MAX_AUTHORIZATION)
    };
    let deadline = tokio::time::Instant::now() + duration;
    let mut interval = Duration::from_secs(if device.interval == 0 {
        5
    } else {
        device.interval
    });
    loop {
        // Protocol-required RFC 8628 cadence, bounded by the device deadline.
        tokio::time::timeout_at(deadline, tokio::time::sleep(interval))
            .await
            .map_err(|_| MetaError::Expired)?;
        let result = tokio::time::timeout_at(deadline, poll_device(client, endpoints, device))
            .await
            .map_err(|_| MetaError::Expired)??;
        match result {
            PollResult::Authorized(token) => return Ok(token),
            PollResult::Pending => {}
            PollResult::SlowDown => interval = interval.saturating_add(Duration::from_secs(5)),
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct MintedKey {
    pub api_key: String,
    pub base_url: String,
    pub user_email: String,
    pub user_full_name: String,
    pub subs_tier_name: String,
    pub subs_tier_id: String,
    pub is_subs_active: bool,
    pub has_payment_method: bool,
    pub require_payment: bool,
    pub can_subscribe: bool,
}

pub async fn mint_key(
    client: &Client,
    endpoints: &MetaEndpoints,
    dca: &str,
) -> Result<MintedKey, MetaError> {
    if dca.trim().is_empty() {
        return Err(MetaError::Invalid("dca_token"));
    }
    let response = client
        .post(&endpoints.mint)
        .timeout(ACQUISITION_TIMEOUT)
        .bearer_auth(dca.trim())
        .header("user-agent", AUTH_USER_AGENT)
        .header("accept", "application/json")
        .json(&json!({"dca_token": dca.trim()}))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(MetaError::Status(response.status().as_u16()));
    }
    let minted: MintedKey = response.json().await?;
    if minted.api_key.trim().is_empty() || minted.api_key.trim().starts_with("dca:") {
        return Err(MetaError::Invalid("api_key"));
    }
    Ok(minted)
}

#[derive(Deserialize)]
struct Catalog {
    data: Vec<CatalogModel>,
}
#[derive(Deserialize)]
struct CatalogModel {
    id: String,
}

pub fn parse_catalog(raw: &[u8]) -> Result<Vec<String>, MetaError> {
    let catalog: Catalog =
        serde_json::from_slice(raw).map_err(|_| MetaError::Invalid("models.data"))?;
    let mut models = Vec::with_capacity(catalog.data.len());
    for model in catalog.data {
        if model.id.trim().is_empty() {
            return Err(MetaError::Invalid("models.data.id"));
        }
        if !models.contains(&model.id) {
            models.push(model.id);
        }
    }
    Ok(models)
}

pub async fn fetch_catalog(
    client: &Client,
    base: &str,
    token: &str,
) -> Result<Vec<String>, MetaError> {
    let mut request = client
        .get(format!("{}/models", base.trim_end_matches('/')))
        .timeout(ACQUISITION_TIMEOUT);
    for (name, value) in api_headers(token, false)? {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    if !response.status().is_success() {
        return Err(MetaError::Status(response.status().as_u16()));
    }
    parse_catalog(&response.bytes().await?)
}

pub fn apply_minted(
    account: &MetaAccount,
    minted: MintedKey,
    models: Vec<String>,
    now: i64,
) -> Result<MetaAccount, MetaError> {
    if minted.api_key.trim().is_empty() || minted.api_key.trim().starts_with("dca:") {
        return Err(MetaError::Invalid("api_key"));
    }
    let mut candidate = account.clone();
    candidate.dca_token = account.dca().unwrap_or_default().to_string();
    candidate.provider_type = "meta".into();
    candidate.auth_kind = "oauth".into();
    candidate.access_token = minted.api_key.clone();
    candidate.api_key = minted.api_key;
    candidate.expired.clear();
    candidate.last_refresh = chrono::DateTime::from_timestamp(now, 0)
        .ok_or(MetaError::Invalid("refresh timestamp"))?
        .to_rfc3339();
    if !minted.base_url.trim().is_empty() {
        candidate.base_url = minted.base_url;
    }
    if !minted.user_email.is_empty() {
        candidate.email = minted.user_email;
    }
    if !minted.user_full_name.is_empty() {
        candidate.name = minted.user_full_name;
    }
    for (key, value) in [
        ("subs_tier_name", minted.subs_tier_name),
        ("subs_tier_id", minted.subs_tier_id),
    ] {
        if value.is_empty() {
            candidate.metadata.remove(key);
        } else {
            candidate.metadata.insert(key.into(), json!(value));
        }
    }
    for (key, value) in [
        ("is_subs_active", minted.is_subs_active),
        ("has_payment_method", minted.has_payment_method),
        ("require_payment", minted.require_payment),
        ("can_subscribe", minted.can_subscribe),
    ] {
        candidate.metadata.insert(key.into(), json!(value));
    }
    candidate.models = models;
    Ok(candidate)
}

/// Returns an isolated candidate only after mint AND remote discovery succeed.
/// The gateway owns the per-account refresh lock and atomic persistence/publication.
pub async fn refresh(
    client: &Client,
    endpoints: &MetaEndpoints,
    account: &MetaAccount,
    now: i64,
) -> Result<MetaAccount, MetaError> {
    if let Some(dca) = account.dca() {
        let minted = mint_key(client, endpoints, dca).await?;
        let base = endpoints.upstream_override.as_deref().unwrap_or_else(|| {
            if minted.base_url.trim().is_empty() {
                account.effective_base()
            } else {
                minted.base_url.trim()
            }
        });
        let models = fetch_catalog(client, base, &minted.api_key).await?;
        let mut candidate = apply_minted(account, minted, models, now)?;
        if let Some(base) = &endpoints.upstream_override {
            candidate.base_url = base.clone();
        }
        Ok(candidate)
    } else {
        let token = account
            .api_token()
            .ok_or(MetaError::Invalid("api_key/dca_token"))?;
        let base = endpoints
            .upstream_override
            .as_deref()
            .unwrap_or(account.effective_base());
        let models = fetch_catalog(client, base, token).await?;
        let mut candidate = account.clone();
        candidate.models = models;
        Ok(candidate)
    }
}

pub async fn complete_device(
    client: &Client,
    endpoints: &MetaEndpoints,
    token: DeviceToken,
    now: i64,
) -> Result<MetaAccount, MetaError> {
    if token.access_token.trim().is_empty() {
        return Err(MetaError::Invalid("access_token"));
    }
    let expires = if token.expires_in > 0 {
        now.saturating_add(
            i64::try_from(token.expires_in).map_err(|_| MetaError::Invalid("expires_in"))?,
        )
    } else {
        token.expires_at
    };
    let account = MetaAccount {
        provider_type: "meta".into(),
        auth_kind: "oauth".into(),
        dca_token: token.access_token,
        token_type: token.token_type,
        expires_in: token.expires_in,
        dca_expires_at: expires,
        dca_expired: chrono::DateTime::from_timestamp(expires, 0)
            .filter(|_| expires > 0)
            .map(|d| d.to_rfc3339())
            .unwrap_or_default(),
        ..Default::default()
    };
    refresh(client, endpoints, &account, now).await
}

/// Projects native storage into the existing generic routing carrier without changing IDs.
pub fn routing_record(account: &MetaAccount) -> Result<Value, MetaError> {
    let mut value = serde_json::to_value(account).map_err(|_| MetaError::Invalid("account"))?;
    let object = value.as_object_mut().ok_or(MetaError::Invalid("account"))?;
    object.insert("provider".into(), json!("meta"));
    object.insert("adapter".into(), json!("meta"));
    object.insert("auth_mode".into(), json!("api_key"));
    object.insert("base_url".into(), json!(account.effective_base()));
    object.insert(
        "api_key".into(),
        json!(account.api_token().unwrap_or_default()),
    );
    object.insert(
        "refresh_token".into(),
        json!(account.dca().unwrap_or_default()),
    );
    object.insert(
        "static_headers".into(),
        json!({"user-agent":API_USER_AGENT,"x-client-id":"tbh:tui"}),
    );
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denied_and_expired_grants_are_terminal() {
        for (error, denied) in [("access_denied", true), ("expired_token", false)] {
            let token = DeviceToken {
                error: error.into(),
                ..Default::default()
            };
            let result = parse_poll(StatusCode::BAD_REQUEST, token);
            if denied {
                assert!(matches!(result, Err(MetaError::Denied)));
            } else {
                assert!(matches!(result, Err(MetaError::Expired)));
            }
        }
    }

    #[test]
    fn pending_and_slow_down_are_distinct() {
        let pending = DeviceToken {
            error: "authorization_pending".into(),
            ..Default::default()
        };
        let slow = DeviceToken {
            error: "slow_down".into(),
            ..Default::default()
        };
        assert!(matches!(
            parse_poll(StatusCode::BAD_REQUEST, pending),
            Ok(PollResult::Pending)
        ));
        assert!(matches!(
            parse_poll(StatusCode::BAD_REQUEST, slow),
            Ok(PollResult::SlowDown)
        ));
    }

    #[test]
    fn minted_key_does_not_expire_with_dca() {
        let account = MetaAccount {
            api_key: "key".into(),
            dca_token: "dca:expired".into(),
            dca_expires_at: 1,
            ..Default::default()
        };
        assert!(!account.is_expired(2));
        assert_eq!(account.api_token(), Some("key"));
    }

    #[test]
    fn dca_only_record_needs_mint_and_cannot_be_sent_to_inference() {
        let account = MetaAccount {
            access_token: "dca:legacy".into(),
            ..Default::default()
        };
        assert!(account.needs_mint());
        assert!(account.headers(true).is_err());
    }

    #[test]
    fn config_key_never_enables_dca_minting() {
        let account = MetaAccount {
            auth_kind: "api_key".into(),
            api_key: "dca:invalid".into(),
            dca_token: "dca:t".into(),
            ..Default::default()
        };
        assert_eq!(account.dca(), None);
        assert!(!account.needs_mint());
        assert!(account.headers(false).is_err());
    }

    #[test]
    fn api_headers_pin_client_identity_without_routing_hints() {
        let headers: BTreeMap<_, _> = api_headers("key", true).unwrap().into_iter().collect();
        assert_eq!(headers["user-agent"], API_USER_AGENT);
        assert_eq!(headers["x-client-id"], "tbh:tui");
        assert_eq!(headers["accept"], "text/event-stream");
        assert!(!headers.contains_key("x-session-id"));
        assert!(!headers.contains_key("x-client-request-id"));
    }

    #[test]
    fn remint_replaces_catalog_and_preserves_identity_and_unknown_fields() {
        let original = MetaAccount {
            identity_slug: "existing".into(),
            access_token: "dca:t".into(),
            models: vec!["removed".into()],
            disabled: false,
            metadata: BTreeMap::from([
                ("quota".into(), Value::Null),
                ("subs_tier_name".into(), json!("old")),
                (
                    "upstream_override".into(),
                    json!("http://127.0.0.1:18841/v1"),
                ),
            ]),
            ..Default::default()
        };
        let candidate = apply_minted(
            &original,
            MintedKey {
                api_key: "new".into(),
                is_subs_active: true,
                ..Default::default()
            },
            vec!["fresh".into()],
            100,
        )
        .unwrap();
        assert_eq!(candidate.identity_slug, "existing");
        assert_eq!(candidate.models, ["fresh"]);
        assert_eq!(candidate.metadata["quota"], Value::Null);
        assert!(!candidate.metadata.contains_key("subs_tier_name"));
        assert_eq!(candidate.metadata["is_subs_active"], true);
        assert_eq!(candidate.dca_token, "dca:t");
        assert_eq!(original.models, ["removed"]);
    }

    #[test]
    fn catalog_preserves_remote_order_and_rejects_malformed_entries() {
        let models =
            parse_catalog(br#"{"data":[{"id":"remote-b"},{"id":"remote-a"},{"id":"remote-b"}]}"#)
                .unwrap();
        assert_eq!(models, ["remote-b", "remote-a"]);
        assert!(parse_catalog(br#"{"data":[{"id":""}]}"#).is_err());
        assert!(parse_catalog(br#"{"models":["invented"]}"#).is_err());
    }

    #[tokio::test]
    async fn refresh_fetches_changed_catalog_and_keeps_original_on_failure() {
        use axum::{
            routing::{get, post},
            Json, Router,
        };
        let listener = {
            let mut found = None;
            for port in 18840..=18899 {
                if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                    found = Some(listener);
                    break;
                }
            }
            found.expect("reserved QA range available")
        };
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route("/device", post(|| async {
            Json(json!({"device_code":"dc","user_code":"uc","verification_uri":"http://127.0.0.1:18841/approve"}))
        })).route("/token", post(|headers: axum::http::HeaderMap, body: String| async move {
            assert_eq!(headers["user-agent"], AUTH_USER_AGENT);
            assert!(body.contains("device_code=dc"));
            assert!(body.contains("client_id=1031625952748946"));
            Json(json!({"access_token":"dca:t","expires_in":10}))
        })).route("/mint", post(|headers: axum::http::HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers["authorization"], "Bearer dca:t");
            assert_eq!(body["dca_token"], "dca:t");
            Json(json!({"api_key":"new-key","subs_tier_name":"Pro","is_subs_active":true}))
        })).route("/v1/models", get(|headers: axum::http::HeaderMap| async move {
            assert_eq!(headers["authorization"], "Bearer new-key");
            Json(json!({"data":[{"id":"changed-model"}]}))
        })).route("/broken/models", get(|| async { StatusCode::UNAUTHORIZED }));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    stopped.await.unwrap();
                })
                .await
                .unwrap();
        });
        let original = MetaAccount {
            identity_slug: "stable".into(),
            api_key: "old".into(),
            dca_token: "dca:t".into(),
            models: vec!["old-model".into()],
            ..Default::default()
        };
        let mut endpoints = MetaEndpoints {
            device: format!("{base}/device"),
            token: format!("{base}/token"),
            mint: format!("{base}/mint"),
            upstream_override: Some(format!("{base}/v1")),
        };
        let device = start_device(&Client::new(), &endpoints).await.unwrap();
        let PollResult::Authorized(token) = poll_device(&Client::new(), &endpoints, &device)
            .await
            .unwrap()
        else {
            panic!("authorized fixture");
        };
        let created = complete_device(&Client::new(), &endpoints, token, 100)
            .await
            .unwrap();
        assert_eq!(created.models, ["changed-model"]);
        assert_eq!(created.dca_expires_at, 110);
        let candidate = refresh(&Client::new(), &endpoints, &original, 100)
            .await
            .unwrap();
        assert_eq!(candidate.models, ["changed-model"]);
        assert_eq!(candidate.identity_slug, "stable");
        endpoints.upstream_override = Some(format!("{base}/broken"));
        assert!(matches!(
            refresh(&Client::new(), &endpoints, &original, 100).await,
            Err(MetaError::Status(401))
        ));
        assert_eq!(original.api_key, "old");
        assert_eq!(original.models, ["old-model"]);
        stop.send(()).unwrap();
        server.await.unwrap();
    }
}
