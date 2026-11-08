use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;

use crate::credential_file::write_credential_atomically;
use crate::refresh::Tokens;

pub fn format_expired_rfc3339(unix: i64) -> String {
    DateTime::<Utc>::from_timestamp(unix, 0)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[derive(Debug, thiserror::Error, Clone)]
pub enum RefreshError {
    #[error("http: {0}")]
    Http(Arc<reqwest::Error>),
    #[error("status {code}: {body}")]
    Status { code: u16, body: String },
    #[error("parse: {0}")]
    Parse(String),
    #[error("io: {0}")]
    Io(Arc<std::io::Error>),
}

impl From<reqwest::Error> for RefreshError {
    fn from(err: reqwest::Error) -> Self {
        RefreshError::Http(Arc::new(err))
    }
}

impl From<std::io::Error> for RefreshError {
    fn from(err: std::io::Error) -> Self {
        RefreshError::Io(Arc::new(err))
    }
}

impl RefreshError {
    /// Whether this error represents an explicit, permanent authentication rejection by the IdP
    /// (e.g. HTTP 400 invalid_grant, 401 Unauthorized, 403 Forbidden).
    /// Network errors, timeouts, 5xx server errors, or I/O errors are transient transport
    /// problems and MUST NOT cause the account to be marked as AuthFailed.
    pub fn is_auth_failure(&self) -> bool {
        match self {
            RefreshError::Status { code, .. } => (400..=403).contains(code),
            _ => false,
        }
    }

    pub fn is_retryable(&self) -> bool {
        match self {
            RefreshError::Status { code, .. } => *code == 429 || *code >= 500,
            _ => false,
        }
    }
}

struct FlightState {
    done: bool,
    result: Option<Result<Tokens, RefreshError>>,
    wakers: Vec<Waker>,
}

struct SharedFlight {
    state: Mutex<FlightState>,
}

struct FlightWaiter {
    flight: Arc<SharedFlight>,
}

impl Future for FlightWaiter {
    type Output = Result<Tokens, RefreshError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.flight.state.lock().unwrap();
        if state.done {
            if let Some(ref res) = state.result {
                return Poll::Ready(res.clone());
            }
        }
        if !state.wakers.iter().any(|waker| waker.will_wake(cx.waker())) {
            state.wakers.push(cx.waker().clone());
        }
        Poll::Pending
    }
}

static IN_FLIGHT_REFRESHES: LazyLock<Mutex<HashMap<String, Arc<SharedFlight>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static BLOCKED_REFRESH_TOKENS: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static AMBIGUOUS_FAILED_TOKENS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn parse_retry_after(resp: &reqwest::Response) -> std::time::Duration {
    const DEFAULT_BACKOFF: std::time::Duration = std::time::Duration::from_secs(10);
    if let Some(val) = resp.headers().get(reqwest::header::RETRY_AFTER) {
        if let Ok(s) = val.to_str() {
            if let Ok(secs) = s.trim().parse::<u64>() {
                return std::time::Duration::from_secs(secs.clamp(1, 300));
            }
        }
    }
    DEFAULT_BACKOFF
}

pub async fn execute_refresh(
    client: &reqwest::Client,
    url: &str,
    refresh_token: &str,
) -> Result<Tokens, RefreshError> {
    let req_spec = crate::refresh::build_refresh_request(refresh_token);
    execute_refresh_spec(client, url, &req_spec).await
}

/// Cline WorkOS refresh: same transport as `execute_refresh_spec` but the
/// rotated pair arrives nested under `data` (`{data:{accessToken,
/// refreshToken}}`) and there is no `expires_in` — the sibling `expiresAt`
/// RFC3339 field carries expiry. `refresh_url_override` exists only for
/// tests (mock server target); production always passes `None` and hits
/// `CLINE_REFRESH_URL`. Hoists `data` to the top level, maps
/// camelCase, then derives `expires_in` from `expiresAt` so the shared
/// `Tokens` shape and `apply_refresh_to_file` stamping keep working.
pub async fn execute_cline_refresh(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<Tokens, RefreshError> {
    execute_cline_refresh_to(client, refresh_token, None).await
}

pub async fn execute_cline_refresh_to(
    client: &reqwest::Client,
    refresh_token: &str,
    refresh_url_override: Option<&str>,
) -> Result<Tokens, RefreshError> {
    let req_spec = crate::refresh::build_cline_refresh_request(refresh_token);
    let url = refresh_url_override
        .map(str::to_string)
        .unwrap_or(req_spec.url.clone());
    let request = client.post(url).json(
        req_spec
            .json_body
            .as_ref()
            .unwrap_or(&serde_json::Value::Null),
    );
    let resp = request.send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(RefreshError::Status {
            code: status.as_u16(),
            body,
        });
    }
    let mut value: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| RefreshError::Parse(e.to_string()))?;
    if let Some(data) = value.get("data").cloned() {
        value = data;
    }
    let expires_in = value
        .get("expiresAt")
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp() - chrono::Utc::now().timestamp())
        .filter(|secs| *secs > 0);
    let mut json_str =
        serde_json::to_string(&value).map_err(|e| RefreshError::Parse(e.to_string()))?;
    // Re-attach the `workos:` scheme the server strips: the CLI stores
    // `workos:eyJ...` and sends it back verbatim, and the bare JWT alone
    // is rejected with 401 re-authenticate. Verified live 2026-09-10.
    // NOTE: `parse_refresh_response` maps camelCase (`accessToken`) but
    // the prefix check must run on the camelCase key — snake_case
    // `access_token` does not exist yet at this point.
    if let Ok(mut obj) = serde_json::from_str::<serde_json::Value>(&json_str) {
        if let Some(map) = obj.as_object_mut() {
            if let Some(tok) = map.get("accessToken").and_then(|v| v.as_str()) {
                if !tok.starts_with("workos:") {
                    map.insert(
                        "accessToken".to_string(),
                        serde_json::Value::String(format!("workos:{tok}")),
                    );
                    json_str = serde_json::to_string(&obj)
                        .map_err(|e| RefreshError::Parse(e.to_string()))?;
                }
            }
        }
    }
    if let Some(secs) = expires_in {
        let mut obj: serde_json::Value =
            serde_json::from_str(&json_str).map_err(|e| RefreshError::Parse(e.to_string()))?;
        if let Some(map) = obj.as_object_mut() {
            map.insert(
                "expires_in".to_string(),
                serde_json::Value::Number(secs.into()),
            );
        }
        json_str = serde_json::to_string(&obj).map_err(|e| RefreshError::Parse(e.to_string()))?;
    }
    crate::refresh::parse_refresh_response(&json_str).map_err(RefreshError::Parse)
}

pub async fn execute_refresh_spec(
    client: &reqwest::Client,
    url: &str,
    req_spec: &crate::refresh::RefreshRequest,
) -> Result<Tokens, RefreshError> {
    if req_spec.url != crate::refresh::CLAUDE_REFRESH_TOKEN_URL {
        return execute_refresh_spec_direct(client, url, req_spec).await;
    }
    let Some(token) = req_spec.refresh_token() else {
        return execute_refresh_spec_direct(client, url, req_spec).await;
    };
    let existing = IN_FLIGHT_REFRESHES.lock().unwrap().get(&format!("{url}|{token}")).cloned();
    if let Some(flight) = existing {
        return FlightWaiter { flight }.await;
    }

    {
        let ambiguous = AMBIGUOUS_FAILED_TOKENS.lock().unwrap();
        if let Some(reason) = ambiguous.get(token) {
            return Err(RefreshError::Parse(format!(
                "refresh token was already submitted and encountered an ambiguous failure ({reason}); re-submitting risks invalidating the token family"
            )));
        }
    }

    {
        let blocked = BLOCKED_REFRESH_TOKENS.lock().unwrap();
        if let Some(until) = blocked.get(token) {
            let now = Instant::now();
            if now < *until {
                let remaining = until.duration_since(now).as_secs();
                return Err(RefreshError::Status {
                    code: 429,
                    body: format!(
                        "refresh temporarily blocked due to rate limiting (retry after {remaining}s)"
                    ),
                });
            }
        }
    }

    let key = format!("{url}|{token}");
    let (flight, is_leader) = {
        let mut in_flight = IN_FLIGHT_REFRESHES.lock().unwrap();
        if let Some(existing) = in_flight.get(&key) {
            (existing.clone(), false)
        } else {
            let shared = Arc::new(SharedFlight {
                state: Mutex::new(FlightState {
                    done: false,
                    result: None,
                    wakers: Vec::new(),
                }),
            });
            in_flight.insert(key.clone(), shared.clone());
            (shared, true)
        }
    };

    if !is_leader {
        return FlightWaiter { flight }.await;
    }

    struct FlightGuard<'a> {
        key: &'a str,
        flight: Arc<SharedFlight>,
    }
    impl<'a> Drop for FlightGuard<'a> {
        fn drop(&mut self) {
            let mut state = self.flight.state.lock().unwrap_or_else(|p| p.into_inner());
            if !state.done {
                state.done = true;
                state.result = Some(Err(RefreshError::Parse("refresh cancelled after dispatch; outcome unknown".into())));
                let wakers = std::mem::take(&mut state.wakers);
                drop(state);
                for waker in wakers { waker.wake(); }
            }
            let mut in_flight = IN_FLIGHT_REFRESHES.lock().unwrap();
            in_flight.remove(self.key);
        }
    }
    let _guard = FlightGuard { key: &key, flight: Arc::clone(&flight) };
    AMBIGUOUS_FAILED_TOKENS.lock().unwrap().insert(token.to_string(), "request in flight or cancelled".into());

    let result = execute_refresh_spec_direct(client, url, req_spec).await;

    match &result {
        Ok(_) => {
            let mut blocked = BLOCKED_REFRESH_TOKENS.lock().unwrap();
            blocked.remove(token);
            let mut ambiguous = AMBIGUOUS_FAILED_TOKENS.lock().unwrap();
            ambiguous.remove(token);
        }
        Err(RefreshError::Parse(err)) => {
            let mut ambiguous = AMBIGUOUS_FAILED_TOKENS.lock().unwrap();
            ambiguous.insert(token.to_string(), err.clone());
        }
        Err(RefreshError::Status { .. }) => {
            AMBIGUOUS_FAILED_TOKENS.lock().unwrap().remove(token);
        }
        Err(_) => {}
    }

    {
        let mut state = flight.state.lock().unwrap();
        state.done = true;
        state.result = Some(result.clone());
        let wakers = std::mem::take(&mut state.wakers);
        drop(state);
        for waker in wakers {
            waker.wake();
        }
    }

    result
}

async fn execute_refresh_spec_direct(
    client: &reqwest::Client,
    url: &str,
    req_spec: &crate::refresh::RefreshRequest,
) -> Result<Tokens, RefreshError> {
    let request = client.post(url);
    let request = req_spec
        .headers
        .iter()
        .fold(request, |request, (name, value)| {
            request.header(name, value)
        });
    let request = match &req_spec.json_body {
        Some(body) => request.json(body),
        None => request.form(&req_spec.form_fields),
    };
    let resp = request.send().await?;

    let status = resp.status();
    let retry_after = parse_retry_after(&resp);

    if !status.is_success() {
        if status.as_u16() == 429 && req_spec.url == crate::refresh::CLAUDE_REFRESH_TOKEN_URL {
            if let Some(token) = req_spec.refresh_token().filter(|_| req_spec.url == crate::refresh::CLAUDE_REFRESH_TOKEN_URL) {
                let mut blocked = BLOCKED_REFRESH_TOKENS.lock().unwrap();
                blocked.insert(token.to_string(), Instant::now() + retry_after);
            }
        }
        let body = resp.text().await.unwrap_or_default();
        return Err(RefreshError::Status {
            code: status.as_u16(),
            body,
        });
    }

    let body = match resp.text().await {
        Ok(text) => text,
        Err(err) => {
            if let Some(token) = req_spec.refresh_token().filter(|_| req_spec.url == crate::refresh::CLAUDE_REFRESH_TOKEN_URL) {
                let mut ambiguous = AMBIGUOUS_FAILED_TOKENS.lock().unwrap();
                ambiguous.insert(
                    token.to_string(),
                    format!("transport drop during body read: {err}"),
                );
            }
            return Err(RefreshError::Http(Arc::new(err)));
        }
    };

    match crate::refresh::parse_refresh_response(&body) {
        Ok(tokens) => Ok(tokens),
        Err(err) => {
            if let Some(token) = req_spec.refresh_token() {
                let mut ambiguous = AMBIGUOUS_FAILED_TOKENS.lock().unwrap();
                ambiguous.insert(token.to_string(), format!("JSON decode error: {err}"));
            }
            Err(RefreshError::Parse(err))
        }
    }
}

pub fn apply_refresh_to_file(
    path: &Path,
    tokens: &Tokens,
    now_unix: i64,
) -> Result<(), RefreshError> {
    let content = std::fs::read_to_string(path)?;
    let mut root: Value =
        serde_json::from_str(&content).map_err(|e| RefreshError::Parse(e.to_string()))?;

    let obj = root
        .as_object_mut()
        .ok_or_else(|| RefreshError::Parse("root is not a JSON object".to_string()))?;

    let access_token_field = if obj.get("type").and_then(Value::as_str) == Some("generic") {
        "api_key"
    } else {
        "access_token"
    };
    obj.insert(
        access_token_field.to_string(),
        Value::String(tokens.access_token.clone()),
    );

    if let Some(ref rt) = tokens.refresh_token {
        obj.insert("refresh_token".to_string(), Value::String(rt.clone()));
    }

    if let Some(ref idt) = tokens.id_token {
        obj.insert("id_token".to_string(), Value::String(idt.clone()));
    }

    let exp_unix = now_unix + tokens.expires_in.unwrap_or(3600);
    let exp_dt = DateTime::<Utc>::from_timestamp(exp_unix, 0)
        .ok_or_else(|| RefreshError::Parse("invalid timestamp for expired".to_string()))?;
    obj.insert(
        "expired".to_string(),
        Value::String(exp_dt.to_rfc3339_opts(SecondsFormat::Secs, true)),
    );

    let lr_dt = DateTime::<Utc>::from_timestamp(now_unix, 0)
        .ok_or_else(|| RefreshError::Parse("invalid timestamp for last_refresh".to_string()))?;
    let lr_str = lr_dt.to_rfc3339_opts(SecondsFormat::Secs, true);
    obj.insert("last_refresh".to_string(), Value::String(lr_str.clone()));

    // CLIProxyAPI writes `timestamp` as epoch millis for antigravity but as an
    // RFC3339 string for codex; rewriting it with the wrong type makes the
    // credential unloadable on the next start, so mirror whichever is there.
    match obj.get("timestamp") {
        Some(Value::Number(_)) => {
            obj.insert(
                "timestamp".to_string(),
                Value::Number((now_unix * 1000).into()),
            );
        }
        Some(_) => {
            obj.insert("timestamp".to_string(), Value::String(lr_str));
        }
        None => {}
    }

    let serialized =
        serde_json::to_string(&root).map_err(|e| RefreshError::Parse(e.to_string()))?;
    write_credential_atomically(path, serialized.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Router};
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn execute_refresh_success() {
        let app = Router::new().route(
            "/oauth/token",
            post(|body: String| async move {
                assert!(body.contains("grant_type=refresh_token"));
                assert!(body.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
                (
                    axum::http::StatusCode::OK,
                    [("content-type", "application/json")],
                    r#"{"access_token":"new-at","refresh_token":"new-rt","expires_in":3600}"#,
                )
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/oauth/token");
        let tokens = execute_refresh(&client, &url, "my-rt").await.unwrap();

        assert_eq!(tokens.access_token, "new-at");
        assert_eq!(tokens.refresh_token.as_deref(), Some("new-rt"));
        assert_eq!(tokens.expires_in, Some(3600));
    }

    #[tokio::test]
    async fn execute_cline_refresh_hoists_nested_data() {
        // Cline nests the rotated pair under `data` and reports expiry as
        // RFC3339 `expiresAt` instead of `expires_in`; the executor must
        // hoist and derive so `apply_refresh_to_file` keeps working.
        let app = Router::new().route(
            "/api/v1/auth/refresh",
            post(|body: String| async move {
                assert!(body.contains("refresh_token"));
                assert!(body.contains("refreshToken"));
                (
                    axum::http::StatusCode::OK,
                    [("content-type", "application/json")],
                    r#"{"data":{"accessToken":"new-at","refreshToken":"new-rt","expiresAt":"2030-01-01T00:00:00Z"}}"#,
                )
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // Exercise the real executor against the mock: nested `data`,
        // `workos:` re-attach, `expiresAt` derive are all asserted.
        let client = reqwest::Client::new();
        let tokens = crate::refresh_exec::execute_cline_refresh_to(
            &client,
            "old-rt",
            Some(&format!("http://{addr}/api/v1/auth/refresh")),
        )
        .await
        .expect("cline refresh against mock");
        assert_eq!(tokens.access_token, "workos:new-at");
        assert_eq!(tokens.refresh_token.as_deref(), Some("new-rt"));
        assert!(tokens.expires_in.unwrap_or(0) > 0);

        // Builder shape: JSON grant, cline endpoint.
        let spec = crate::refresh::build_cline_refresh_request("old-rt");
        assert_eq!(spec.url, crate::refresh::CLINE_REFRESH_URL);
        let payload = spec.json_body.expect("cline refresh is JSON");
        assert_eq!(
            payload.get("refreshToken").and_then(|v| v.as_str()),
            Some("old-rt")
        );
    }

    #[tokio::test]
    async fn execute_refresh_status_error_429() {
        let app = Router::new().route(
            "/oauth/token",
            post(|| async { (axum::http::StatusCode::TOO_MANY_REQUESTS, "slow down") }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/oauth/token");
        let err = execute_refresh(&client, &url, "my-rt").await.unwrap_err();

        match err {
            RefreshError::Status { code, body } => {
                assert_eq!(code, 429);
                assert_eq!(body, "slow down");
            }
            other => panic!("expected Status error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn claude_refresh_uses_anthropic_json_contract() {
        let app = Router::new().route(
            "/v1/oauth/token",
            post(|headers: axum::http::HeaderMap, body: String| async move {
                assert_eq!(
                    headers.get("content-type").and_then(|v| v.to_str().ok()),
                    Some("application/json")
                );
                let value: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(value["grant_type"], "refresh_token");
                assert_eq!(value["client_id"], "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
                assert_eq!(value["refresh_token"], "claude-rt");
                (
                    axum::http::StatusCode::OK,
                    [("content-type", "application/json")],
                    r#"{"access_token":"new-claude","refresh_token":"new-rt","expires_in":3600}"#,
                )
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let spec = crate::refresh::build_claude_refresh_request("claude-rt");
        let tokens = execute_refresh_spec(
            &reqwest::Client::new(),
            &format!("http://{addr}/v1/oauth/token"),
            &spec,
        )
        .await
        .unwrap();
        assert_eq!(tokens.access_token, "new-claude");
    }

    #[tokio::test]
    async fn cursor_refresh_uses_bearer_empty_json_and_camelcase_response() {
        let app = Router::new().route(
            "/auth/exchange_user_api_key",
            post(|headers: axum::http::HeaderMap, body: String| async move {
                assert_eq!(headers["authorization"], "Bearer cursor-rt");
                assert_eq!(body, "{}");
                (
                    axum::http::StatusCode::OK,
                    [("content-type", "application/json")],
                    r#"{"accessToken":"cursor-new","refreshToken":"cursor-new-rt"}"#,
                )
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let spec = crate::refresh::build_cursor_refresh_request("cursor-rt");
        let tokens = execute_refresh_spec(
            &reqwest::Client::new(),
            &format!("http://{addr}/auth/exchange_user_api_key"),
            &spec,
        )
        .await
        .unwrap();
        assert_eq!(tokens.access_token, "cursor-new");
        assert_eq!(tokens.refresh_token.as_deref(), Some("cursor-new-rt"));
    }

    #[tokio::test]
    async fn kiro_social_refresh_uses_refresh_token_json() {
        let app = Router::new().route(
            "/refreshToken",
            post(|body: String| async move {
                let value: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(value["refreshToken"], "kiro-rt");
                (
                    axum::http::StatusCode::OK,
                    [("content-type", "application/json")],
                    r#"{"accessToken":"kiro-new","refreshToken":"kiro-new-rt","expiresIn":3600}"#,
                )
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let spec = crate::refresh::build_kiro_social_refresh_request("kiro-rt", "us-east-1");
        let tokens = execute_refresh_spec(
            &reqwest::Client::new(),
            &format!("http://{addr}/refreshToken"),
            &spec,
        )
        .await
        .unwrap();
        assert_eq!(tokens.access_token, "kiro-new");
    }

    #[tokio::test]
    async fn execute_refresh_parse_error_malformed_json() {
        let app = Router::new().route(
            "/oauth/token",
            post(|| async { (axum::http::StatusCode::OK, "not-json-content") }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/oauth/token");
        let err = execute_refresh(&client, &url, "my-rt").await.unwrap_err();

        assert!(matches!(err, RefreshError::Parse(_)));
    }

    fn refresh_preserves_numeric_timestamp_type(dir: &std::path::Path) {
        let path = dir.join("antigravity-account.json");
        std::fs::write(
            &path,
            r#"{"access_token":"old","refresh_token":"oldrt","expires_in":3599,
                "timestamp":1787883220105,"email":"a@b.c","disabled":false}"#,
        )
        .unwrap();

        let tokens = Tokens {
            access_token: "new-at".to_string(),
            refresh_token: Some("new-rt".to_string()),
            id_token: None,
            token_type: Some("bearer".to_string()),
            expires_in: Some(3599),
        };
        let now_unix = 1750000000;
        apply_refresh_to_file(&path, &tokens, now_unix).unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            val["timestamp"].is_number(),
            "antigravity timestamp must stay numeric or the credential fails to load: {}",
            val["timestamp"]
        );
        assert_eq!(val["timestamp"], now_unix * 1000);

        let reparsed = apply_refresh_to_file(&path, &tokens, now_unix + 10);
        assert!(
            reparsed.is_ok(),
            "a refreshed file must refresh again cleanly"
        );
    }

    #[test]
    fn apply_refresh_to_file_round_trip() {
        let dir = std::env::temp_dir().join(format!("qprov-apply-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("codex-account.json");

        refresh_preserves_numeric_timestamp_type(&dir);

        let initial_json = r#"{"access_token":"old","refresh_token":"oldrt","expired":"2020-01-01T00:00:00Z","last_refresh":"2019-01-01T00:00:00Z","project_id":"p1","disabled":false,"email":"a@b.c","type":"plus","expires_in":3600}"#;
        std::fs::write(&file_path, initial_json).unwrap();

        let new_tokens = Tokens {
            access_token: "new-at".to_string(),
            refresh_token: Some("new-rt".to_string()),
            id_token: None,
            token_type: Some("bearer".to_string()),
            expires_in: Some(3600),
        };

        let now_unix = 1750000000;
        apply_refresh_to_file(&file_path, &new_tokens, now_unix).unwrap();

        let updated_content = std::fs::read_to_string(&file_path).unwrap();
        let val: serde_json::Value = serde_json::from_str(&updated_content).unwrap();

        assert_eq!(val["access_token"], "new-at");
        assert_eq!(val["refresh_token"], "new-rt");
        assert_eq!(val["project_id"], "p1");
        assert_eq!(val["disabled"], false);
        assert_eq!(val["email"], "a@b.c");
        assert_eq!(val["type"], "plus");
        assert_eq!(val["expires_in"], 3600);

        let expired_str = val["expired"].as_str().unwrap();
        let exp_dt = chrono::DateTime::parse_from_rfc3339(expired_str).unwrap();
        assert!(exp_dt.timestamp() > now_unix);
        assert_eq!(exp_dt.timestamp(), now_unix + 3600);

        let last_refresh_str = val["last_refresh"].as_str().unwrap();
        let lr_dt = chrono::DateTime::parse_from_rfc3339(last_refresh_str).unwrap();
        assert_eq!(lr_dt.timestamp(), now_unix);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_refresh_updates_timestamp_only_when_present() {
        let dir = std::env::temp_dir().join(format!("qprov-apply-ts-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Case 1: timestamp key present
        let with_ts_path = dir.join("with-ts.json");
        std::fs::write(
            &with_ts_path,
            r#"{"access_token":"old","timestamp":"2019-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        let tokens = Tokens {
            access_token: "new-at".to_string(),
            refresh_token: None,
            id_token: Some("new-id".to_string()),
            token_type: None,
            expires_in: None,
        };

        let now_unix = 1750000000;
        apply_refresh_to_file(&with_ts_path, &tokens, now_unix).unwrap();

        let content1 = std::fs::read_to_string(&with_ts_path).unwrap();
        let val1: serde_json::Value = serde_json::from_str(&content1).unwrap();
        let expected_now_rfc = DateTime::<Utc>::from_timestamp(now_unix, 0)
            .unwrap()
            .to_rfc3339_opts(SecondsFormat::Secs, true);
        let expected_exp_rfc = DateTime::<Utc>::from_timestamp(now_unix + 3600, 0)
            .unwrap()
            .to_rfc3339_opts(SecondsFormat::Secs, true);

        assert_eq!(val1["access_token"], "new-at");
        assert_eq!(val1["id_token"], "new-id");
        assert_eq!(val1["timestamp"], expected_now_rfc.as_str());
        assert_eq!(val1["last_refresh"], expected_now_rfc.as_str());
        assert_eq!(val1["expired"], expected_exp_rfc.as_str());

        // Case 2: timestamp key absent
        let without_ts_path = dir.join("without-ts.json");
        std::fs::write(&without_ts_path, r#"{"access_token":"old"}"#).unwrap();

        apply_refresh_to_file(&without_ts_path, &tokens, now_unix).unwrap();

        let content2 = std::fs::read_to_string(&without_ts_path).unwrap();
        let val2: serde_json::Value = serde_json::from_str(&content2).unwrap();
        assert_eq!(val2["access_token"], "new-at");
        assert!(val2.get("timestamp").is_none());
        assert_eq!(val2["last_refresh"], expected_now_rfc.as_str());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_concurrent_refresh_deduplication_singleflight() {
        let hit_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits = hit_count.clone();

        let app = Router::new().route(
            "/v1/oauth/token",
            post(move |body: String| {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
                    assert_eq!(value["grant_type"], "refresh_token");
                    (
                        axum::http::StatusCode::OK,
                        [("content-type", "application/json")],
                        r#"{"access_token":"rotated-claude-token","refresh_token":"rotated-claude-rt","expires_in":3600}"#,
                    )
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/v1/oauth/token");
        let spec = crate::refresh::build_claude_refresh_request("shared-concurrent-rt");

        let mut leader = Box::pin(execute_refresh_spec(&client, &url, &spec));
        std::future::poll_fn(|cx| {
            assert!(leader.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        }).await;
        let mut followers = (0..4).map(|_| Box::pin(execute_refresh_spec(&client, &url, &spec))).collect::<Vec<_>>();
        for follower in &mut followers {
            std::future::poll_fn(|cx| {
                assert!(follower.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            }).await;
        }
        let mut results = vec![tokio::time::timeout(std::time::Duration::from_secs(5), leader).await.unwrap().unwrap()];
        for follower in followers {
            results.push(tokio::time::timeout(std::time::Duration::from_secs(5), follower).await.unwrap().unwrap());
        }

        assert_eq!(results.len(), 5);
        for res in &results {
            assert_eq!(res.access_token, "rotated-claude-token");
            assert_eq!(res.refresh_token.as_deref(), Some("rotated-claude-rt"));
        }
        assert_eq!(hit_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_refresh_does_not_replay_after_response_parse_error() {
        let hit_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits = hit_count.clone();

        let app = Router::new().route(
            "/v1/oauth/token",
            post(move || {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    (
                        axum::http::StatusCode::OK,
                        [("content-type", "application/json")],
                        "malformed-non-json-token-response",
                    )
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/v1/oauth/token");
        let spec = crate::refresh::build_claude_refresh_request("single-use-rt-parse-fail");

        let err1 = execute_refresh_spec(&client, &url, &spec).await.unwrap_err();
        assert!(matches!(err1, RefreshError::Parse(_)));
        assert_eq!(hit_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        let err2 = execute_refresh_spec(&client, &url, &spec).await.unwrap_err();
        match err2 {
            RefreshError::Parse(msg) => {
                assert!(msg.contains("ambiguous failure"));
            }
            other => panic!("expected ambiguous failure Parse error, got {other:?}"),
        }
        assert_eq!(hit_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_refresh_429_blocks_immediate_replay() {
        let hit_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits = hit_count.clone();

        let app = Router::new().route(
            "/v1/oauth/token",
            post(move || {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        [
                            ("content-type", "application/json"),
                            ("retry-after", "60"),
                        ],
                        r#"{"error":"rate_limited"}"#,
                    )
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/v1/oauth/token");
        let spec = crate::refresh::build_claude_refresh_request("rt-429-blocked");

        let err1 = execute_refresh_spec(&client, &url, &spec).await.unwrap_err();
        match err1 {
            RefreshError::Status { code, .. } => assert_eq!(code, 429),
            other => panic!("expected Status 429, got {other:?}"),
        }
        assert_eq!(hit_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        let err2 = execute_refresh_spec(&client, &url, &spec).await.unwrap_err();
        match err2 {
            RefreshError::Status { code, body } => {
                assert_eq!(code, 429);
                assert!(body.contains("rate limiting"));
            }
            other => panic!("expected blocked 429, got {other:?}"),
        }
        assert_eq!(hit_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn test_refresh_error_retryable_classification() {
        assert!(RefreshError::Status { code: 429, body: "".to_string() }.is_retryable());
        assert!(RefreshError::Status { code: 500, body: "".to_string() }.is_retryable());
        assert!(RefreshError::Status { code: 503, body: "".to_string() }.is_retryable());

        assert!(!RefreshError::Status { code: 400, body: "".to_string() }.is_retryable());
        assert!(!RefreshError::Status { code: 401, body: "".to_string() }.is_retryable());
        assert!(!RefreshError::Status { code: 403, body: "".to_string() }.is_retryable());

        assert!(!RefreshError::Parse("malformed".to_string()).is_retryable());
        assert!(!RefreshError::Io(Arc::new(std::io::Error::new(std::io::ErrorKind::Other, "io"))).is_retryable());
    }

    #[tokio::test]
    async fn test_refresh_handles_compressed_responses() {
        const GZIP_PAYLOAD: &[u8] = &[
            31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 171, 86, 74, 76, 78, 78, 45, 46, 142, 47, 201,
            207, 78, 205, 83, 178, 82, 74, 175, 202, 44, 208, 133, 112, 116, 148, 138, 82, 211,
            138, 82, 139, 51, 80, 37, 139, 74, 128, 50, 169, 21, 5, 153, 64, 169, 248, 76, 160,
            176, 177, 153, 129, 65, 45, 0, 164, 10, 112, 251, 73, 0, 0, 0,
        ];

        let app = Router::new().route(
            "/v1/oauth/token",
            post(|| async move {
                (
                    axum::http::StatusCode::OK,
                    [
                        ("content-type", "application/json"),
                        ("content-encoding", "gzip"),
                    ],
                    axum::body::Bytes::from_static(GZIP_PAYLOAD),
                )
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = reqwest::Client::builder().gzip(true).build().unwrap();
        let url = format!("http://{addr}/v1/oauth/token");
        let spec = crate::refresh::build_claude_refresh_request("rt-gzip-test");

        let tokens = execute_refresh_spec(&client, &url, &spec).await.unwrap();
        assert_eq!(tokens.access_token, "gzip-token");
        assert_eq!(tokens.refresh_token.as_deref(), Some("gzip-rt"));
        assert_eq!(tokens.expires_in, Some(3600));
    }
}
