//! Native Grok credential policy. The caller supplies its account-scoped HTTP client
//! (including proxy and redirect policy) to every credential/catalog operation.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const API_BASE: &str = "https://api.x.ai/v1";
pub const CHAT_BASE: &str = "https://cli-chat-proxy.grok.com/v1";
pub const DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
pub const CLIENT_VERSION: &str = "1.0.46";

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct XaiAccount {
    #[serde(default)]
    pub identity_slug: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub auth_mode: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expired: String,
    #[serde(default)]
    pub token_url: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub using_api: Option<bool>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub static_headers: BTreeMap<String, String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl std::fmt::Debug for XaiAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XaiAccount")
            .field("identity_slug", &self.identity_slug)
            .field("auth_mode", &self.auth_mode)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Chat,
    Media,
    Websocket,
    Compact,
    Speech,
    Catalog,
}

impl XaiAccount {
    pub fn from_record(record: &Value) -> Result<Self, serde_json::Error> {
        let mut record = record.clone();
        normalize_record(&mut record);
        serde_json::from_value(record)
    }

    pub fn oauth(&self) -> bool {
        self.auth_mode.eq_ignore_ascii_case("oauth")
    }

    pub fn token(&self) -> &str {
        if self.api_key.is_empty() {
            &self.access_token
        } else {
            &self.api_key
        }
    }

    pub fn is_expired(&self, now_unix: i64) -> bool {
        self.oauth()
            && crate::parse_expired_unix(&self.expired).is_none_or(|expiry| now_unix >= expiry)
    }

    pub fn base_url_for<'a>(
        &'a self,
        transport: Transport,
        upstream_override: Option<&'a str>,
    ) -> &'a str {
        if let Some(base) = upstream_override {
            return base.trim_end_matches('/');
        }
        let base = self.base_url.trim().trim_end_matches('/');
        match transport {
            Transport::Chat | Transport::Media | Transport::Catalog => {
                if self.using_api.unwrap_or(!self.oauth()) {
                    if base.is_empty() {
                        API_BASE
                    } else {
                        base
                    }
                } else if base.is_empty() || base == API_BASE {
                    CHAT_BASE
                } else {
                    base
                }
            }
            Transport::Websocket | Transport::Compact | Transport::Speech => {
                if base.is_empty() || base == CHAT_BASE {
                    API_BASE
                } else {
                    base
                }
            }
        }
    }

    /// Starts with a fresh header set; ingress routing headers are never accepted.
    pub fn headers(
        &self,
        transport: Transport,
        base: &str,
        stream: bool,
        version: &str,
    ) -> Vec<(String, String)> {
        let mut headers = BTreeMap::from([
            ("content-type".to_owned(), "application/json".to_owned()),
            (
                "accept".to_owned(),
                if stream {
                    "text/event-stream"
                } else {
                    "application/json"
                }
                .to_owned(),
            ),
        ]);
        if !self.token().is_empty() {
            headers.insert(
                "authorization".to_owned(),
                format!("Bearer {}", self.token()),
            );
        }
        if matches!(
            transport,
            Transport::Chat | Transport::Media | Transport::Catalog
        ) && !self.using_api.unwrap_or(!self.oauth())
            && base.trim_end_matches('/') == CHAT_BASE
        {
            headers.extend([
                ("x-xai-token-auth".to_owned(), "xai-grok-cli".to_owned()),
                ("x-grok-client-version".to_owned(), version.to_owned()),
                (
                    "user-agent".to_owned(),
                    format!("xai-grok-workspace/{version}"),
                ),
                (
                    "x-grok-client-identifier".to_owned(),
                    "grok-shell".to_owned(),
                ),
                (
                    "x-authenticateresponse".to_owned(),
                    "authenticate-response".to_owned(),
                ),
            ]);
        }
        for (name, value) in &self.static_headers {
            let name = name.to_ascii_lowercase();
            if !matches!(
                name.as_str(),
                "x-session-id"
                    | "x-client-request-id"
                    | "x-grok-conv-id"
                    | "authorization"
                    | "host"
            ) {
                headers.insert(name, value.clone());
            }
        }
        headers.into_iter().collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum XaiError {
    #[error("invalid xAI OAuth endpoint")]
    Endpoint,
    #[error("xAI response omitted required credential/catalog fields")]
    Payload,
    #[error("xAI HTTP request failed")]
    Http(#[from] reqwest::Error),
    #[error("xAI credential JSON is invalid")]
    Json(#[from] serde_json::Error),
    #[error("xAI refresh failed: {0}")]
    Refresh(#[from] crate::refresh_exec::RefreshError),
}

/// Only the explicit mock override admits HTTP, and only on reserved loopback ports.
pub fn validate_endpoint(raw: &str, upstream_override: Option<&str>) -> Result<(), XaiError> {
    let url = reqwest::Url::parse(raw).map_err(|_| XaiError::Endpoint)?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(XaiError::Endpoint);
    }
    let host = url.host_str().ok_or(XaiError::Endpoint)?;
    if url.scheme() == "https" && (host == "x.ai" || host.ends_with(".x.ai")) {
        return Ok(());
    }
    if let Some(base) = upstream_override {
        let base = reqwest::Url::parse(base).map_err(|_| XaiError::Endpoint)?;
        if url.origin() == base.origin()
            && url.scheme() == "http"
            && matches!(host, "127.0.0.1" | "localhost" | "[::1]")
            && url
                .port()
                .is_some_and(|port| (18840..=18899).contains(&port))
        {
            return Ok(());
        }
    }
    Err(XaiError::Endpoint)
}

#[derive(Debug, Deserialize)]
pub struct Discovery {
    pub device_authorization_endpoint: String,
    pub token_endpoint: String,
}

#[derive(Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    #[serde(default)]
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: String,
    pub expires_in: u64,
    #[serde(default)]
    pub interval: u64,
}

pub async fn request_device_code(
    client: &reqwest::Client,
    discovery: &Discovery,
    upstream_override: Option<&str>,
) -> Result<DeviceCode, XaiError> {
    validate_endpoint(&discovery.device_authorization_endpoint, upstream_override)?;
    let code: DeviceCode = client
        .post(&discovery.device_authorization_endpoint)
        .form(&[("client_id", CLIENT_ID), ("scope", SCOPE)])
        .header("accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if code.device_code.is_empty()
        || code.user_code.is_empty()
        || code.expires_in == 0
        || (code.verification_uri.is_empty() && code.verification_uri_complete.is_empty())
    {
        return Err(XaiError::Payload);
    }
    Ok(code)
}

pub async fn exchange_device_code(
    client: &reqwest::Client,
    discovery: &Discovery,
    device_code: &str,
    upstream_override: Option<&str>,
) -> Result<crate::Tokens, XaiError> {
    validate_endpoint(&discovery.token_endpoint, upstream_override)?;
    let tokens: crate::Tokens = client
        .post(&discovery.token_endpoint)
        .form(&[
            ("client_id", CLIENT_ID),
            ("device_code", device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ])
        .header("accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if tokens.access_token.is_empty() {
        return Err(XaiError::Payload);
    }
    Ok(tokens)
}

pub async fn discover(
    client: &reqwest::Client,
    upstream_override: Option<&str>,
) -> Result<Discovery, XaiError> {
    let url = upstream_override
        .map(|base| {
            format!(
                "{}/.well-known/openid-configuration",
                base.trim_end_matches('/')
            )
        })
        .unwrap_or_else(|| DISCOVERY_URL.to_owned());
    validate_endpoint(&url, upstream_override)?;
    let discovery: Discovery = client
        .get(url)
        .header("accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    validate_endpoint(&discovery.device_authorization_endpoint, upstream_override)?;
    validate_endpoint(&discovery.token_endpoint, upstream_override)?;
    Ok(discovery)
}

pub fn refresh_request(refresh_token: &str, token_endpoint: &str) -> crate::RefreshRequest {
    crate::RefreshRequest {
        url: token_endpoint.to_owned(),
        form_fields: vec![
            ("grant_type".to_owned(), "refresh_token".to_owned()),
            ("refresh_token".to_owned(), refresh_token.to_owned()),
            ("client_id".to_owned(), CLIENT_ID.to_owned()),
        ],
        json_body: None,
        headers: vec![("accept".to_owned(), "application/json".to_owned())],
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Model {
    pub id: String,
    #[serde(flatten)]
    pub details: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct Catalog {
    data: Vec<Model>,
}

pub fn parse_catalog(value: Value) -> Result<Vec<Model>, XaiError> {
    let catalog: Catalog = serde_json::from_value(value)?;
    if catalog.data.iter().any(|model| model.id.trim().is_empty()) {
        return Err(XaiError::Payload);
    }
    let mut seen = std::collections::BTreeSet::new();
    Ok(catalog
        .data
        .into_iter()
        .filter(|model| seen.insert(model.id.clone()))
        .collect())
}

pub async fn fetch_models(
    client: &reqwest::Client,
    account: &XaiAccount,
    upstream_override: Option<&str>,
) -> Result<Vec<Model>, XaiError> {
    let base = account.base_url_for(Transport::Catalog, upstream_override);
    let mut request = client.get(format!("{base}/models"));
    for (name, value) in account.headers(Transport::Catalog, base, false, CLIENT_VERSION) {
        request = request.header(name, value);
    }
    parse_catalog(request.send().await?.error_for_status()?.json().await?)
}

/// Atomic in-memory result: failed refresh/catalog leaves the original record untouched.
/// The integrator persists this value to the existing path, never creating a second account.
pub async fn refresh_record(
    client: &reqwest::Client,
    record: &Value,
    now_unix: i64,
    upstream_override: Option<&str>,
) -> Result<Value, XaiError> {
    let mut account = XaiAccount::from_record(record)?;
    let mut updated = record.clone();
    if account.oauth() && !account.refresh_token.is_empty() {
        let endpoint = if account.token_url.is_empty() {
            discover(client, upstream_override).await?.token_endpoint
        } else {
            account.token_url.clone()
        };
        validate_endpoint(&endpoint, upstream_override)?;
        let spec = refresh_request(&account.refresh_token, &endpoint);
        let tokens = crate::refresh_exec::execute_refresh_spec(client, &endpoint, &spec).await?;
        if tokens.access_token.trim().is_empty() {
            return Err(XaiError::Payload);
        }
        account.api_key = tokens.access_token.clone();
        account.access_token = tokens.access_token.clone();
        updated["api_key"] = Value::String(tokens.access_token.clone());
        updated["access_token"] = Value::String(tokens.access_token);
        updated["token_url"] = Value::String(endpoint.clone());
        updated["token_endpoint"] = Value::String(endpoint);
        if let Some(token) = tokens.refresh_token.filter(|token| !token.is_empty()) {
            updated["refresh_token"] = Value::String(token);
        }
        if let Some(token) = tokens.id_token.filter(|token| !token.is_empty()) {
            updated["id_token"] = Value::String(token);
        }
        if let Some(seconds) = tokens.expires_in.filter(|seconds| *seconds > 0) {
            let expiry = now_unix.checked_add(seconds).ok_or(XaiError::Payload)?;
            updated["expired"] = Value::String(crate::refresh_exec::format_expired_rfc3339(expiry));
        }
        updated["last_refresh"] =
            Value::String(crate::refresh_exec::format_expired_rfc3339(now_unix));
    }
    let models = fetch_models(client, &account, upstream_override).await?;
    updated["models"] =
        serde_json::to_value(models.iter().map(|model| &model.id).collect::<Vec<_>>())?;
    updated["xai_catalog"] = serde_json::to_value(models)?;
    normalize_record(&mut updated);
    Ok(updated)
}

/// In-place migration preserves filename identity, unknown fields, quota and user settings.
pub fn normalize_record(record: &mut Value) -> bool {
    let provider = record
        .get("provider")
        .or_else(|| record.get("type"))
        .and_then(Value::as_str);
    if !provider.is_some_and(|provider| provider.eq_ignore_ascii_case("xai")) {
        return false;
    }
    record["provider"] = Value::String("xai".to_owned());
    record["adapter"] = Value::String("xai".to_owned());
    if record.get("auth_mode").is_none() {
        record["auth_mode"] = record
            .get("auth_kind")
            .cloned()
            .unwrap_or_else(|| Value::String("key".to_owned()));
    }
    if record
        .get("api_key")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        record["api_key"] = record
            .get("access_token")
            .cloned()
            .unwrap_or_else(|| Value::String(String::new()));
    }
    if record
        .get("token_url")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        record["token_url"] = record
            .get("token_endpoint")
            .cloned()
            .unwrap_or_else(|| Value::String(String::new()));
    }
    if record
        .get("base_url")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        record["base_url"] = Value::String(API_BASE.to_owned());
    }
    record["client_id"] = Value::String(CLIENT_ID.to_owned());
    true
}

pub async fn enroll_record(
    client: &reqwest::Client,
    record: &Value,
    upstream_override: Option<&str>,
) -> Result<Value, XaiError> {
    let account = XaiAccount::from_record(record)?;
    let models = fetch_models(client, &account, upstream_override).await?;
    let mut record = record.clone();
    normalize_record(&mut record);
    record["models"] =
        serde_json::to_value(models.iter().map(|model| &model.id).collect::<Vec<_>>())?;
    record["xai_catalog"] = serde_json::to_value(models)?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn migration_when_generic_preserves_identity_and_unknown_quota() {
        let mut record = json!({"type":"generic","provider":"xai","identity_slug":"existing","auth_mode":"oauth","api_key":"old","token_endpoint":"https://auth.x.ai/token","quota":null,"disabled":false});
        assert!(normalize_record(&mut record));
        let once = record.clone();
        assert!(normalize_record(&mut record));
        assert_eq!(once, record);
        assert_eq!(record["identity_slug"], "existing");
        assert!(record["quota"].is_null());
        assert_eq!(
            XaiAccount::from_record(&record).unwrap().token_url,
            "https://auth.x.ai/token"
        );
    }

    #[test]
    fn transport_when_oauth_uses_native_bases_and_isolated_headers() {
        let account = XaiAccount {
            auth_mode: "oauth".into(),
            access_token: "secret".into(),
            static_headers: BTreeMap::from([("X-Session-ID".into(), "raw".into())]),
            ..Default::default()
        };
        assert_eq!(account.base_url_for(Transport::Chat, None), CHAT_BASE);
        for transport in [Transport::Websocket, Transport::Compact, Transport::Speech] {
            assert_eq!(account.base_url_for(transport, None), API_BASE);
        }
        let headers: BTreeMap<_, _> = account
            .headers(Transport::Chat, CHAT_BASE, true, CLIENT_VERSION)
            .into_iter()
            .collect();
        assert_eq!(headers["x-xai-token-auth"], "xai-grok-cli");
        assert_eq!(headers["x-grok-client-version"], CLIENT_VERSION);
        assert!(!headers.contains_key("x-session-id"));
        assert!(!format!("{account:?}").contains("secret"));
        assert_eq!(
            account.base_url_for(Transport::Media, Some("http://127.0.0.1:18841/v1")),
            "http://127.0.0.1:18841/v1"
        );
    }

    #[test]
    fn endpoint_when_untrusted_or_unreserved_is_rejected() {
        for endpoint in [
            "https://x.ai.evil/token",
            "http://auth.x.ai/token",
            "https://u:p@auth.x.ai/token",
            "http://127.0.0.1:18839/token",
        ] {
            assert!(validate_endpoint(endpoint, Some(endpoint)).is_err());
        }
        assert!(validate_endpoint("http://127.0.0.1:18841/token", None).is_err());
        assert!(validate_endpoint(
            "http://127.0.0.1:18841/token",
            Some("http://127.0.0.1:18841")
        )
        .is_ok());
        assert!(validate_endpoint("https://auth.x.ai/token", None).is_ok());
    }

    #[test]
    fn catalog_when_remote_contains_new_model_preserves_metadata() {
        let models = parse_catalog(
            json!({"data":[{"id":"remote-new","context_window":123},{"id":"remote-new"}]}),
        )
        .unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].details["context_window"], 123);
        assert!(parse_catalog(json!({"data":[{"id":""}]})).is_err());
        assert!(parse_catalog(json!({})).is_err());
    }

    #[test]
    fn expiry_when_oauth_is_missing_or_at_boundary_requires_refresh() {
        let account = XaiAccount {
            auth_mode: "oauth".into(),
            expired: "1970-01-01T00:00:10Z".into(),
            ..Default::default()
        };
        assert!(!account.is_expired(9));
        assert!(account.is_expired(10));
        assert!(!XaiAccount::default().is_expired(10));
        let request = refresh_request("refresh", "https://auth.x.ai/token");
        assert!(request
            .form_fields
            .contains(&("client_id".into(), CLIENT_ID.into())));
    }

    async fn refresh_fixture(
        reject: bool,
    ) -> (Result<Value, XaiError>, Value, Vec<(String, String)>) {
        use axum::{
            extract::State,
            routing::{get, post},
            Json, Router,
        };
        use std::sync::{Arc, Mutex};
        type Captures = Arc<Mutex<Vec<(String, String)>>>;
        let captures: Captures = Arc::new(Mutex::new(Vec::new()));
        let routes = Router::new()
            .route(
                "/token",
                post(
                    move |State(captures): State<Captures>, body: String| async move {
                        captures.lock().unwrap().push(("token".into(), body));
                        if reject {
                            (
                                axum::http::StatusCode::BAD_REQUEST,
                                Json(json!({"error":"invalid_grant"})),
                            )
                        } else {
                            (
                                axum::http::StatusCode::OK,
                                Json(json!({"access_token":"rotated","expires_in":60})),
                            )
                        }
                    },
                ),
            )
            .route(
                "/v1/models",
                get(
                    |State(captures): State<Captures>, headers: axum::http::HeaderMap| async move {
                        captures.lock().unwrap().push((
                            "catalog".into(),
                            headers["authorization"].to_str().unwrap().into(),
                        ));
                        Json(json!({"data":[{"id":"remote-fresh","context_window":4096}]}))
                    },
                ),
            )
            .with_state(Arc::clone(&captures));
        let mut listener = None;
        for port in 18842..=18899 {
            match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                Ok(bound) => {
                    listener = Some(bound);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
                Err(error) => panic!("fixture bind failed: {error}"),
            }
        }
        let listener = listener.expect("reserved fixture port available");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (shutdown, stop) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            axum::serve(listener, routes)
                .with_graceful_shutdown(async {
                    stop.await.unwrap();
                })
                .await
                .unwrap();
        });
        let original = json!({"provider":"xai","type":"generic","identity_slug":"existing","auth_mode":"oauth","api_key":"old","refresh_token":"retain","token_url":format!("{base}/token"),"base_url":format!("{base}/v1"),"upstream_override":format!("{base}/v1"),"quota":null});
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let result = refresh_record(&client, &original, 10, Some(&format!("{base}/v1"))).await;
        shutdown.send(()).unwrap();
        server.await.unwrap();
        let captured = captures.lock().unwrap().clone();
        (result, original, captured)
    }

    #[tokio::test]
    async fn refresh_when_success_fetches_catalog_with_rotated_credential() {
        let (result, original, captures) = refresh_fixture(false).await;
        let record = result.unwrap();
        assert_eq!(record["api_key"], "rotated");
        assert_eq!(record["refresh_token"], "retain");
        assert_eq!(record["models"], json!(["remote-fresh"]));
        assert_eq!(record["identity_slug"], original["identity_slug"]);
        assert!(record["quota"].is_null());
        assert_eq!(captures[1], ("catalog".into(), "Bearer rotated".into()));
    }

    #[tokio::test]
    async fn refresh_when_invalid_grant_does_not_fetch_catalog_or_mutate_record() {
        let (result, original, captures) = refresh_fixture(true).await;
        assert!(result.is_err());
        assert_eq!(original["api_key"], "old");
        assert_eq!(captures.len(), 1);
        assert_eq!(captures[0].0, "token");
    }
}
