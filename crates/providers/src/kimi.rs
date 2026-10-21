//! Native Kimi device authorization and catalog-backed credential refresh.
//! The caller owns persistence and the existing account refresh singleflight.
use reqwest::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Domain {
    Com,
    Ai,
}

impl Domain {
    pub fn from_provider(provider: &str) -> Option<Self> {
        match provider.trim().to_ascii_lowercase().as_str() {
            "kimi" | "kimi.com" | "com" => Some(Self::Com),
            "kimi-ai" | "kimi.ai" | "ai" => Some(Self::Ai),
            _ => None,
        }
    }

    pub const fn oauth_host(self) -> &'static str {
        match self {
            Self::Com => "https://auth.kimi.com",
            Self::Ai => "https://auth.kimi.ai",
        }
    }

    pub const fn api_base(self) -> &'static str {
        match self {
            Self::Com => "https://api.kimi.com/coding",
            Self::Ai => "https://api.kimi.ai/coding",
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Com => "kimi.com",
            Self::Ai => "kimi.ai",
        }
    }
}

/// Explicit metadata wins over provider name. Account IDs are not provider identity.
pub fn credential_domain(value: &Value) -> Domain {
    for key in ["domain", "base_url", "provider", "type"] {
        if let Some(raw) = value.get(key).and_then(Value::as_str) {
            if let Some(domain) = Domain::from_provider(raw) {
                return domain;
            }
            if key == "base_url" {
                if let Ok(url) = reqwest::Url::parse(raw) {
                    if let Some(host) = url.host_str() {
                        if host == "kimi.ai" || host.ends_with(".kimi.ai") {
                            return Domain::Ai;
                        }
                        if host == "kimi.com" || host.ends_with(".kimi.com") {
                            return Domain::Com;
                        }
                    }
                }
            }
        }
    }
    Domain::Com
}

pub fn api_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    format!("{base}/v1/{}", path.trim_start_matches('/'))
}

pub fn is_provider(provider: &str) -> bool {
    matches!(
        provider.trim().to_ascii_lowercase().as_str(),
        "kimi" | "kimi-ai" | "kimi.com" | "kimi.ai"
    )
}

pub fn normalize_credential(value: &mut Value) -> Result<bool, Error> {
    let provider = value
        .get("provider")
        .and_then(Value::as_str)
        .or_else(|| value.get("type").and_then(Value::as_str))
        .unwrap_or("");
    if !is_provider(provider) {
        return Ok(false);
    }
    let domain = credential_domain(value);
    let identity = device_identity(value);
    let object = value.as_object_mut().ok_or(Error::Credential)?;
    object
        .entry("provider")
        .or_insert(json!(if domain == Domain::Ai {
            "kimi-ai"
        } else {
            "kimi"
        }));
    object.insert("adapter".into(), json!("kimi"));
    object
        .entry("base_url")
        .or_insert(json!(api_url(domain.api_base(), "").trim_end_matches('/')));
    object.entry("auth_mode").or_insert(json!("oauth"));
    object
        .entry("token_url")
        .or_insert(json!(format!("{}/api/oauth/token", domain.oauth_host())));
    object.entry("client_id").or_insert(json!(CLIENT_ID));
    if !object.contains_key("api_key") {
        if let Some(token) = object.get("access_token").cloned() {
            object.insert("api_key".into(), token);
        }
    }
    if !object.contains_key("api_key") {
        return Err(Error::MissingToken);
    }
    let headers = object
        .entry("static_headers")
        .or_insert(json!({}))
        .as_object_mut()
        .ok_or(Error::Credential)?;
    headers.retain(|key, _| {
        !matches!(
            key.to_ascii_lowercase().as_str(),
            "x-session-id" | "x-client-request-id"
        )
    });
    for (name, value) in identity.headers() {
        headers.insert(name, json!(value));
    }
    Ok(true)
}

pub fn device_identity(value: &Value) -> DeviceIdentity {
    let headers = value.get("static_headers");
    DeviceIdentity {
        id: value
            .get("device_id")
            .and_then(Value::as_str)
            .or_else(|| {
                headers
                    .and_then(|headers| headers.get("x-msh-device-id"))
                    .and_then(Value::as_str)
            })
            .unwrap_or("")
            .to_string(),
        name: value
            .get("device_name")
            .and_then(Value::as_str)
            .unwrap_or("mahoquot")
            .to_string(),
        model: value
            .get("device_model")
            .and_then(Value::as_str)
            .unwrap_or(concat!("mahoquot/", env!("CARGO_PKG_VERSION")))
            .to_string(),
    }
}

/// Provider-owned identity only; never accepts inbound client headers.
#[derive(Clone, Debug)]
pub struct DeviceIdentity {
    pub id: String,
    pub name: String,
    pub model: String,
}

impl DeviceIdentity {
    pub fn headers(&self) -> Vec<(String, String)> {
        [
            ("x-msh-platform", "CLIProxyAPI"),
            ("x-msh-version", env!("CARGO_PKG_VERSION")),
            ("x-msh-device-id", self.id.as_str()),
            ("x-msh-device-name", self.name.as_str()),
            ("x-msh-device-model", self.model.as_str()),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
    }

    fn decorate(&self, mut request: RequestBuilder) -> RequestBuilder {
        for (name, value) in self.headers() {
            request = request.header(name, value);
        }
        request.header("accept", "application/json")
    }
}

// Device codes and token responses deliberately have no Debug implementation.
#[derive(Clone, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: String,
    pub expires_in: u64,
    #[serde(default = "default_interval")]
    pub interval: u64,
}

const fn default_interval() -> u64 {
    5
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Token {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub token_type: String,
    #[serde(default)]
    pub expires_in: Option<f64>,
    #[serde(default)]
    pub scope: String,
}

pub enum PollResult {
    Pending,
    SlowDown,
    Authorized(CredentialUpdate),
}

#[derive(Debug, PartialEq, Eq)]
pub enum PollState {
    Pending,
    SlowDown,
    Authorized,
}

#[derive(Serialize, Deserialize)]
pub struct PollSchedule {
    deadline: u64,
    next_poll: u64,
    interval: u64,
}

impl PollSchedule {
    pub fn new(code: &DeviceCode, now_seconds: u64) -> Self {
        let interval = code.interval.max(5);
        Self {
            deadline: now_seconds.saturating_add(code.expires_in.min(900)),
            next_poll: now_seconds.saturating_add(interval),
            interval,
        }
    }

    pub fn ready(&self, now_seconds: u64) -> Result<bool, Error> {
        if now_seconds >= self.deadline {
            return Err(Error::Expired);
        }
        Ok(now_seconds >= self.next_poll)
    }

    pub fn pending(&mut self, slow_down: bool, now_seconds: u64) {
        if slow_down {
            self.interval = self.interval.saturating_add(5);
        }
        self.next_poll = now_seconds.saturating_add(self.interval);
    }
}

pub fn poll_state(body: &Value) -> Result<PollState, Error> {
    match body
        .get("error")
        .and_then(Value::as_str)
        .filter(|error| !error.is_empty())
    {
        Some("authorization_pending") => Ok(PollState::Pending),
        Some("slow_down") => Ok(PollState::SlowDown),
        Some("access_denied") => Err(Error::Denied),
        Some("expired_token") => Err(Error::Expired),
        Some(_) => Err(Error::OAuth),
        None if body
            .get("access_token")
            .and_then(Value::as_str)
            .is_some_and(|token| !token.trim().is_empty()) =>
        {
            Ok(PollState::Authorized)
        }
        None => Err(Error::MissingToken),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Kimi transport failed")]
    Transport(#[source] reqwest::Error),
    #[error("Kimi returned invalid JSON")]
    Json(#[source] serde_json::Error),
    #[error("Kimi endpoint rejected request ({0})")]
    Http(u16),
    #[error("Kimi authorization denied")]
    Denied,
    #[error("Kimi device code expired")]
    Expired,
    #[error("Kimi OAuth grant rejected")]
    OAuth,
    #[error("Kimi response has no access token")]
    MissingToken,
    #[error("Kimi returned an empty or invalid model catalog")]
    Catalog,
    #[error("invalid Kimi credential")]
    Credential,
}

/// Published only after both the token exchange and remote catalog succeed.
pub struct CredentialUpdate {
    pub token: Token,
    pub models: Vec<String>,
}

impl CredentialUpdate {
    /// Retain IDs, order, disabled state, overrides and all unrecognized settings.
    pub fn apply(self, credential: &mut Value, now_unix: i64) -> Result<(), Error> {
        let object = credential.as_object_mut().ok_or(Error::Credential)?;
        object.insert("api_key".into(), json!(self.token.access_token));
        if object.contains_key("access_token") {
            object.insert("access_token".into(), object["api_key"].clone());
        }
        if !self.token.refresh_token.is_empty() {
            object.insert("refresh_token".into(), json!(self.token.refresh_token));
        }
        // Absent expiry is not an invented one-hour lifetime.
        let expires_at = self
            .token
            .expires_in
            .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
            .and_then(|seconds| std::time::Duration::try_from_secs_f64(seconds).ok())
            .and_then(|duration| i64::try_from(duration.as_secs()).ok())
            .and_then(|seconds| now_unix.checked_add(seconds));
        let expired = expires_at
            .and_then(|unix| chrono::DateTime::<chrono::Utc>::from_timestamp(unix, 0))
            .map(|date| date.to_rfc3339())
            .unwrap_or_default();
        object.insert("expired".into(), json!(expired));
        object.insert("models".into(), json!(self.models));
        Ok(())
    }
}

/// One poll per caller scheduling event. Pending/slow_down are explicit states,
/// including Kimi's HTTP 200 error envelopes. No hidden retry or sleep loop.
pub struct KimiClient {
    client: Client,
    device_url: String,
    token_url: String,
    api_base: String,
    identity: DeviceIdentity,
}

impl KimiClient {
    /// Use the gateway's proxy-aware, redirect-controlled HTTP client.
    pub fn new(client: Client, domain: Domain, identity: DeviceIdentity) -> Self {
        Self::with_endpoints(client, domain.oauth_host(), domain.api_base(), identity)
    }

    pub fn with_endpoints(
        client: Client,
        oauth_host: &str,
        api_base: &str,
        identity: DeviceIdentity,
    ) -> Self {
        let oauth_host = oauth_host.trim_end_matches('/');
        Self {
            client,
            device_url: format!("{oauth_host}/api/oauth/device_authorization"),
            token_url: format!("{oauth_host}/api/oauth/token"),
            api_base: api_base.into(),
            identity,
        }
    }

    /// Caller-supplied device/token endpoints (management `device_url`/`token_url`)
    /// replace the domain defaults verbatim, so a configured override can never
    /// silently fall back to the live Kimi host.
    pub fn with_oauth_urls(mut self, device_url: Option<&str>, token_url: Option<&str>) -> Self {
        if let Some(url) = device_url.filter(|url| !url.is_empty()) {
            self.device_url = url.into();
        }
        if let Some(url) = token_url.filter(|url| !url.is_empty()) {
            self.token_url = url.into();
        }
        self
    }

    pub async fn start_device(&self) -> Result<DeviceCode, Error> {
        let response = self
            .identity
            .decorate(
                self.client
                    .post(&self.device_url)
                    .form(&[("client_id", CLIENT_ID)]),
            )
            .send()
            .await
            .map_err(Error::Transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Http(status.as_u16()));
        }
        let code: DeviceCode = response.json().await.map_err(Error::Transport)?;
        if code.device_code.trim().is_empty() || code.expires_in == 0 {
            return Err(Error::Credential);
        }
        Ok(code)
    }

    pub async fn poll_device(&self, device_code: &str) -> Result<PollResult, Error> {
        let body = self
            .exchange(&[
                ("client_id", CLIENT_ID),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .await?;
        match poll_state(&body)? {
            PollState::Pending => Ok(PollResult::Pending),
            PollState::SlowDown => Ok(PollResult::SlowDown),
            PollState::Authorized => Ok(PollResult::Authorized(self.complete(body).await?)),
        }
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<CredentialUpdate, Error> {
        if refresh_token.trim().is_empty() {
            return Err(Error::Credential);
        }
        let body = self
            .exchange(&[
                ("client_id", CLIENT_ID),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
            ])
            .await?;
        if body
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|error| !error.is_empty())
        {
            return Err(Error::OAuth);
        }
        self.complete(body).await
    }

    async fn exchange(&self, form: &[(&str, &str)]) -> Result<Value, Error> {
        let response = self
            .identity
            .decorate(self.client.post(&self.token_url).form(form))
            .send()
            .await
            .map_err(Error::Transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Http(status.as_u16()));
        }
        response.json().await.map_err(Error::Transport)
    }

    async fn complete(&self, body: Value) -> Result<CredentialUpdate, Error> {
        let token: Token = serde_json::from_value(body).map_err(Error::Json)?;
        if token.access_token.trim().is_empty() {
            return Err(Error::MissingToken);
        }
        let response = self
            .identity
            .decorate(self.client.get(api_url(&self.api_base, "models")))
            .bearer_auth(&token.access_token)
            .send()
            .await
            .map_err(Error::Transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Http(status.as_u16()));
        }
        let catalog: Value = response.json().await.map_err(Error::Transport)?;
        Ok(CredentialUpdate {
            token,
            models: model_ids(&catalog)?,
        })
    }
}

pub fn model_ids(catalog: &Value) -> Result<Vec<String>, Error> {
    let rows = catalog
        .get("data")
        .and_then(Value::as_array)
        .ok_or(Error::Catalog)?;
    let mut ids = Vec::with_capacity(rows.len());
    for row in rows {
        let id = row
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or(Error::Catalog)?;
        if !ids.iter().any(|existing| existing == id) {
            ids.push(id.to_string());
        }
    }
    if ids.is_empty() {
        return Err(Error::Catalog);
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_uses_metadata_when_identity_is_unrelated() {
        // Given / When
        let domain = credential_domain(
            &json!({"domain":"kimi.ai", "provider":"kimi", "identity_slug":"kimi.com"}),
        );
        // Then
        assert_eq!(domain, Domain::Ai);
        assert_eq!(
            api_url(Domain::Ai.api_base(), "models"),
            "https://api.kimi.ai/coding/v1/models"
        );
        assert_eq!(
            api_url("http://127.0.0.1:18841/coding/v1/", "responses"),
            "http://127.0.0.1:18841/coding/v1/responses"
        );
    }

    #[test]
    fn catalog_rejects_unknown_when_remote_has_no_models() {
        // Given / When / Then
        for value in [json!({}), json!({"data":[]}), json!({"data":[{"id":""}]})] {
            assert!(matches!(model_ids(&value), Err(Error::Catalog)));
        }
    }

    #[test]
    fn update_preserves_settings_when_refresh_rotates_token_and_catalog() {
        // Given
        let mut credential = json!({"type":"generic","provider":"kimi","identity_slug":"retained", "disabled":false,
            "refresh_token":"old-refresh", "models":["old"],"quota":null,"upstream_override":"http://127.0.0.1:18841", "request_retry":0});
        let update = CredentialUpdate {
            token: serde_json::from_value(json!({"access_token":"new", "expires_in":60})).unwrap(),
            models: vec!["remote-model".into()],
        };
        // When
        update.apply(&mut credential, 0).unwrap();
        // Then
        assert_eq!(credential["api_key"], "new");
        assert_eq!(credential["refresh_token"], "old-refresh");
        assert_eq!(credential["models"], json!(["remote-model"]));
        assert_eq!(credential["identity_slug"], "retained");
        assert_eq!(credential["request_retry"], 0);
        assert!(credential["quota"].is_null());
        assert_eq!(credential["expired"], "1970-01-01T00:01:00+00:00");
    }

    #[test]
    fn headers_are_provider_owned_when_device_identity_is_supplied() {
        // Given
        let identity = DeviceIdentity {
            id: "device".into(),
            name: "test".into(),
            model: "test".into(),
        };
        // When
        let headers = identity.headers();
        // Then
        assert!(headers
            .iter()
            .any(|(key, value)| key == "x-msh-device-id" && value == "device"));
        assert!(!headers
            .iter()
            .any(|(key, _)| key == "x-session-id" || key == "x-client-request-id"));
    }

    #[test]
    fn poll_is_not_authorized_when_http_success_envelope_reports_oauth_error() {
        // Given / When / Then
        assert_eq!(
            poll_state(&json!({"error":"authorization_pending"})).unwrap(),
            PollState::Pending
        );
        assert_eq!(
            poll_state(&json!({"error":"slow_down"})).unwrap(),
            PollState::SlowDown
        );
        assert!(matches!(
            poll_state(&json!({"error":"access_denied"})),
            Err(Error::Denied)
        ));
        assert!(matches!(
            poll_state(&json!({"error":"expired_token"})),
            Err(Error::Expired)
        ));
        assert!(matches!(
            poll_state(&json!({"access_token":" "})),
            Err(Error::MissingToken)
        ));
    }

    #[test]
    fn slow_down_extends_interval_when_schedule_uses_injected_clock() {
        let code: DeviceCode = serde_json::from_value(json!({"device_code":"d","user_code":"u", "verification_uri":"http://127.0.0.1:18841/verify", "expires_in":20,"interval":1})).unwrap();
        let mut schedule = PollSchedule::new(&code, 0);
        assert!(!schedule.ready(4).unwrap());
        assert!(schedule.ready(5).unwrap());
        schedule.pending(true, 5);
        assert!(!schedule.ready(14).unwrap());
        assert!(schedule.ready(15).unwrap());
        assert!(matches!(schedule.ready(20), Err(Error::Expired)));
    }

    #[test]
    fn native_credential_preserves_identity_when_normalized_to_existing_generic_storage() {
        let mut value = json!({"type":"kimi-ai", "access_token":"secret", "device_id":"device", "identity_slug":"retain", "models":["remote"],
            "static_headers":{"X-Session-Id":"private","x-client-request-id":"private"}});
        assert!(normalize_credential(&mut value).unwrap());
        assert_eq!(value["provider"], "kimi-ai");
        assert_eq!(value["identity_slug"], "retain");
        assert_eq!(value["api_key"], "secret");
        assert_eq!(value["models"], json!(["remote"]));
        assert_eq!(value["base_url"], "https://api.kimi.ai/coding/v1");
        assert_eq!(value["static_headers"]["x-msh-device-id"], "device");
        assert!(value["static_headers"].get("X-Session-Id").is_none());
    }

    async fn mock_client(
        catalog_status: u16,
    ) -> (
        KimiClient,
        tokio::task::JoinHandle<()>,
        tokio::sync::oneshot::Sender<()>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use axum::{
            routing::{get, post},
            Json, Router,
        };
        let mut bound = None;
        for port in 18842..=18899 {
            match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
                Ok(listener) => {
                    bound = Some(listener);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {}
                Err(error) => panic!("fixture bind failed: {error}"),
            }
        }
        let listener = bound.expect("available fixture port");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let captured = count.clone();
        let app = Router::new()
            .route(
                "/api/oauth/token",
                post(|| async {
                    Json(json!({"access_token":"new", "refresh_token":"rotated", "expires_in":60}))
                }),
            )
            .route(
                "/v1/models",
                get(move || {
                    let captured = captured.clone();
                    async move {
                        captured.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        (
                            axum::http::StatusCode::from_u16(catalog_status).unwrap(),
                            Json(json!({"data":[{"id":"remote-fresh"}]})),
                        )
                    }
                }),
            );
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    stopped.await.unwrap();
                })
                .await
                .unwrap();
        });
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        (
            KimiClient::with_endpoints(
                client,
                &base,
                &base,
                DeviceIdentity {
                    id: "mock-device".into(),
                    name: "mock".into(),
                    model: "mock".into(),
                },
            ),
            task,
            shutdown,
            count,
        )
    }

    #[tokio::test]
    async fn refresh_fetches_catalog_when_token_exchange_succeeds() {
        let (client, task, shutdown, count) = mock_client(200).await;
        let result = client.refresh("old").await;
        shutdown.send(()).unwrap();
        task.await.unwrap();
        let update = result.unwrap();
        assert_eq!(update.token.access_token, "new");
        assert_eq!(update.token.refresh_token, "rotated");
        assert_eq!(update.models, vec!["remote-fresh"]);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refresh_returns_no_update_when_catalog_rejects_new_token() {
        let (client, task, shutdown, count) = mock_client(403).await;
        let result = client.refresh("old").await;
        shutdown.send(()).unwrap();
        task.await.unwrap();
        assert!(matches!(result, Err(Error::Http(403))));
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
