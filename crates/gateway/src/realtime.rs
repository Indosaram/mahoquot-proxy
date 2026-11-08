//! The `/v1/realtime`, `/v1/live` and SIP-control surface.
//!
//! CLIProxyAPI fronts these with the ChatGPT/Codex OAuth upstream, which has no
//! SIP, translation or transcription capability, so several endpoints are
//! permanent `501`s rather than relays. The bodies below were captured from
//! CLIProxyAPI v7.2.140 against this credential pool.
//!
//! Request validation order was measured, not assumed: `sdp` is checked before
//! `session`, and both are checked before the model is resolved, so a request
//! with no model at all still fails on `sdp` first.

use base64::Engine;
use rand::RngCore;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

pub mod media;

pub const DEFAULT_EPHEMERAL_TTL_SECS: u64 = 600;
const MIN_EPHEMERAL_TTL_SECS: u64 = 10;
const MAX_EPHEMERAL_TTL_SECS: u64 = 7200;
const MAX_SECRETS: usize = 1024;
const MAX_SECRETS_PER_ISSUER: usize = 64;
pub const MAX_SECRET_BODY_BYTES: usize = 64 * 1024;
pub const MAX_SDP_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_REALTIME_MODEL: &str = "gpt-realtime";

fn token(prefix: &str, bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    format!(
        "{prefix}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&buf)
    )
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Clone)]
pub struct RealtimeSecret {
    pub issuer: String,
    pub model: String,
    pub scope: Option<Arc<crate::management::settings::ScopedApiKey>>,
    pub expires_at: u64,
    expires_instant: Instant,
    pub member_id: Option<String>,
}

pub struct RealtimeStore {
    secrets: RwLock<HashMap<String, RealtimeSecret>>,
    // Private dependency: HTTP callers and process configuration cannot alter
    // credential time. Tests replace both clocks on their own isolated store.
    clock: Arc<dyn Fn() -> (Instant, u64) + Send + Sync>,
}

impl Default for RealtimeStore {
    fn default() -> Self {
        Self {
            secrets: RwLock::new(HashMap::new()),
            clock: Arc::new(|| (Instant::now(), now_unix())),
        }
    }
}

impl RealtimeStore {
    pub async fn mint(
        &self,
        issuer: String,
        model: String,
        scope: Option<Arc<crate::management::settings::ScopedApiKey>>,
        member_id: Option<String>,
        lifetime_secs: u64,
    ) -> Result<(String, u64), &'static str> {
        let lifetime = lifetime_secs.clamp(MIN_EPHEMERAL_TTL_SECS, MAX_EPHEMERAL_TTL_SECS);
        let (now, unix) = (self.clock)();
        let expires_at = unix.saturating_add(lifetime);
        let mut secrets = self.secrets.write().await;
        secrets.retain(|_, secret| secret.expires_instant > now);
        if secrets.len() >= MAX_SECRETS {
            return Err("realtime secret capacity reached");
        }
        if secrets
            .values()
            .filter(|secret| secret.issuer == issuer)
            .count()
            >= MAX_SECRETS_PER_ISSUER
        {
            return Err("realtime issuer secret capacity reached");
        }
        let value = token("ek_", 32);
        secrets.insert(
            value.clone(),
            RealtimeSecret {
                issuer,
                model,
                scope,
                member_id,
                expires_at,
                expires_instant: now + Duration::from_secs(lifetime),
            },
        );
        Ok((value, expires_at))
    }

    pub async fn lookup(&self, value: &str) -> Option<RealtimeSecret> {
        if !value.starts_with("ek_") {
            return None;
        }
        let (now, _) = (self.clock)();
        let mut secrets = self.secrets.write().await;
        secrets.retain(|_, secret| secret.expires_instant > now);
        secrets.get(value).cloned()
    }
}

/// `{"error":{"code":..,"message":..,"param":null,"type":"not_supported_error"}}`
pub fn capability_not_supported(what: &str) -> Value {
    json!({"error": {
        "code": "realtime_capability_not_supported",
        "message": format!("{what} are not supported by the ChatGPT/Codex OAuth upstream"),
        "param": null,
        "type": "not_supported_error",
    }})
}

pub fn call_not_found() -> Value {
    json!({"error": {
        "code": "realtime_call_not_found",
        "message": "Realtime call not found",
        "param": null,
        "type": "invalid_request_error",
    }})
}

/// 426 body for `/v1/realtime/calls/:call_id`, which nests under `error`.
pub fn upgrade_required_nested() -> Value {
    json!({"error": {
        "code": "realtime_request_failed",
        "message": "WebSocket upgrade required",
        "param": null,
        "type": "invalid_request_error",
    }})
}

/// 426 body for `/v1/live/:call_id`, which is flat rather than nested.
pub fn upgrade_required_flat() -> Value {
    json!({"error": "WebSocket upgrade required"})
}

fn detail(msg: &str) -> Value {
    json!({ "detail": msg })
}

pub fn validate_offer(body: &Value) -> Option<Value> {
    if !body.get("sdp").map(Value::is_string).unwrap_or(false) {
        return Some(detail("Field `sdp` must be a string"));
    }
    if !body.get("session").map(Value::is_object).unwrap_or(false) {
        return Some(detail("Field `session` must be an object"));
    }
    None
}

fn session_object(model: &str, expires_at: u64) -> Value {
    json!({
        "expires_at": expires_at,
        "id": token("sess_", 18),
        "model": model,
        "object": "realtime.session",
        "type": "realtime",
    })
}

fn requested_model(body: &Value) -> String {
    body.get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_REALTIME_MODEL)
        .to_string()
}

pub fn client_secret(body: &Value, value: String, expires_at: u64) -> Value {
    json!({
        "value": value,
        "expires_at": expires_at,
        "session": session_object(&requested_model(body), expires_at),
    })
}

pub fn legacy_session(body: &Value, secret: String, expires_at: u64) -> Value {
    let mut out = session_object(&requested_model(body), expires_at);
    out.as_object_mut()
        .expect("session_object builds a map")
        .insert(
            "client_secret".into(),
            json!({"expires_at": expires_at, "value": secret}),
        );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn http_minted_secret_expires_through_full_router() {
        use axum::{
            body::Body,
            http::{Request, StatusCode},
        };
        use http_body_util::BodyExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        use tower::ServiceExt;

        struct Directory(std::path::PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).expect("remove isolated realtime fixture");
            }
        }
        let directory = Directory(std::env::temp_dir().join(token("realtime-router-", 16)));
        std::fs::create_dir(&directory.0).unwrap();
        std::fs::write(
            directory.0.join("codex-expiry.json"),
            json!({
                "type": "plus", "identity_slug": "expiry-router", "account_id": "fixture-account",
                "access_token": "fixture-token", "email": "expiry@example.invalid",
                "expired": "2099-01-01T00:00:00Z", "id_token": "fixture-id",
                "refresh_token": "fixture-refresh", "last_refresh": "2098-01-01T00:00:00Z",
                "models": ["gpt-realtime"]
            })
            .to_string(),
        )
        .unwrap();
        let config_path = directory.0.join("config.yaml");
        std::fs::write(&config_path, "logging-to-file: false\n").unwrap();
        let config = crate::config::GatewayConfig {
            auth_dir: directory.0.clone(),
            config_path,
            api_keys: crate::inbound::ApiKeys::new(vec!["issuer-key".into()]),
            auth_refresh_enabled: false,
            ..crate::config::GatewayConfig::default()
        };
        let elapsed = Arc::new(AtomicU64::new(0));
        let controlled = elapsed.clone();
        let origin = Instant::now();
        let mut state = crate::state::AppState::new(&config).unwrap();
        state.realtime = Arc::new(RealtimeStore {
            secrets: RwLock::new(HashMap::new()),
            clock: Arc::new(move || {
                let seconds = controlled.load(Ordering::SeqCst);
                (origin + Duration::from_secs(seconds), 1_000 + seconds)
            }),
        });
        let app = crate::routes::create_app(Arc::new(state));
        let request = |path: &str, key: &str, body: &str| {
            Request::builder()
                .method("POST")
                .uri(path)
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        let minted = app
            .clone()
            .oneshot(request(
                "/v1/realtime/client_secrets",
                "issuer-key",
                r#"{"session":{"model":"gpt-realtime"},"expires_after":1}"#,
            ))
            .await
            .unwrap();
        assert_eq!(minted.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&minted.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["expires_at"], 1_010); // Production minimum remains ten seconds.
        let secret = body["value"].as_str().unwrap();
        assert!(secret.starts_with("ek_"));
        let path = "/v1/realtime/calls/expiry-router-missing/hangup";
        // A missing call reaches the actual handler only after successful auth.
        assert_eq!(
            app.clone()
                .oneshot(request(path, secret, "{}"))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        elapsed.store(9, Ordering::SeqCst);
        assert_eq!(
            app.clone()
                .oneshot(request(path, secret, "{}"))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        elapsed.store(10, Ordering::SeqCst);
        assert_eq!(
            app.clone()
                .oneshot(request(path, secret, "{}"))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        // Issuer remains valid at the same instant; it is the minted secret that expired.
        assert_eq!(
            app.oneshot(request(path, "issuer-key", "{}"))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn minted_secret_expires_at_exact_monotonic_deadline() {
        use axum::{
            body::Body,
            http::{Request, StatusCode},
            middleware,
            routing::post,
            Extension, Router,
        };
        use std::sync::atomic::{AtomicU64, Ordering};
        use tower::ServiceExt;
        let elapsed = Arc::new(AtomicU64::new(0));
        let clock_elapsed = elapsed.clone();
        let origin = Instant::now();
        let store = Arc::new(RealtimeStore {
            secrets: RwLock::new(HashMap::new()),
            clock: Arc::new(move || {
                let seconds = clock_elapsed.load(Ordering::SeqCst);
                (origin + Duration::from_secs(seconds), 1_000 + seconds)
            }),
        });
        let app = Router::new()
            .route(
                "/v1/realtime/calls",
                post(|| async { StatusCode::NO_CONTENT }),
            )
            .layer(middleware::from_fn_with_state(
                Arc::new(crate::inbound::ApiKeys::new(vec!["issuer-key".into()])),
                crate::inbound::require_api_key,
            ))
            .layer(Extension(store.clone()));
        let authenticate = |value: &str| {
            let request = Request::builder()
                .method("POST")
                .uri("/v1/realtime/calls")
                .header("authorization", format!("Bearer {value}"))
                .body(Body::empty())
                .unwrap();
            app.clone().oneshot(request)
        };
        let (value, expiry) = store
            .mint("issuer".into(), "gpt-realtime".into(), None, None, 10)
            .await
            .unwrap();
        assert_eq!(expiry, 1_010);
        let issued = store
            .lookup(&value)
            .await
            .expect("minted credential accepted");
        assert_eq!(issued.issuer, "issuer");
        assert_eq!(issued.model, "gpt-realtime");
        assert_eq!(
            authenticate(&value).await.unwrap().status(),
            StatusCode::NO_CONTENT
        );
        elapsed.store(9, Ordering::SeqCst);
        assert!(store.lookup(&value).await.is_some());
        assert_eq!(
            authenticate(&value).await.unwrap().status(),
            StatusCode::NO_CONTENT
        );
        elapsed.store(10, Ordering::SeqCst);
        // Authentication itself must expire it; do not pre-prune with lookup.
        assert_eq!(
            authenticate(&value).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        assert!(!store.secrets.read().await.contains_key(&value));
        elapsed.store(11, Ordering::SeqCst);
        assert!(store.lookup(&value).await.is_none());
        let (fresh, _) = store
            .mint("issuer".into(), "gpt-realtime".into(), None, None, 10)
            .await
            .unwrap();
        assert_ne!(fresh, value);
        assert!(store.lookup(&fresh).await.is_some());
        assert_eq!(
            authenticate(&fresh).await.unwrap().status(),
            StatusCode::NO_CONTENT
        );
    }

    #[test]
    fn sdp_is_rejected_before_session() {
        let v = validate_offer(&json!({})).expect("missing sdp");
        assert_eq!(v["detail"], "Field `sdp` must be a string");

        let v = validate_offer(&json!({"model": "x"})).expect("missing sdp");
        assert_eq!(v["detail"], "Field `sdp` must be a string");
    }

    #[test]
    fn session_is_rejected_once_sdp_is_present() {
        let v = validate_offer(&json!({"sdp": "v=0"})).expect("missing session");
        assert_eq!(v["detail"], "Field `session` must be an object");
    }

    #[test]
    fn a_well_formed_offer_passes_validation() {
        assert!(validate_offer(&json!({"sdp": "v=0", "session": {}})).is_none());
    }

    #[test]
    fn capability_message_matches_captured_upstream_text() {
        let v = capability_not_supported("Realtime SIP accept");
        assert_eq!(
            v["error"]["message"],
            "Realtime SIP accept are not supported by the ChatGPT/Codex OAuth upstream"
        );
        assert_eq!(v["error"]["type"], "not_supported_error");
        assert!(v["error"]["param"].is_null());
    }

    #[test]
    fn client_secret_carries_prefixed_token_and_nested_session() {
        let v = client_secret(&json!({}), "ek_test".into(), 100);
        assert!(v["value"].as_str().unwrap().starts_with("ek_"));
        assert!(v["session"]["id"].as_str().unwrap().starts_with("sess_"));
        assert_eq!(v["session"]["model"], DEFAULT_REALTIME_MODEL);
        assert_eq!(v["session"]["object"], "realtime.session");
        assert_eq!(v["expires_at"], v["session"]["expires_at"]);
    }

    #[test]
    fn legacy_session_nests_the_secret_instead_of_the_session() {
        let v = legacy_session(
            &json!({"model": "gpt-realtime-mini"}),
            "ek_test".into(),
            100,
        );
        assert!(v["client_secret"]["value"]
            .as_str()
            .unwrap()
            .starts_with("ek_"));
        assert_eq!(v["model"], "gpt-realtime-mini");
        assert_eq!(v["object"], "realtime.session");
        assert_eq!(v["expires_at"], v["client_secret"]["expires_at"]);
    }

    #[tokio::test]
    async fn store_binds_issuer_and_clamps_lifetime() {
        let origin = Instant::now();
        let store = RealtimeStore {
            secrets: RwLock::new(HashMap::new()),
            clock: Arc::new(move || (origin, 1_000)),
        };
        let (secret, expiry) = store
            .mint("issuer".into(), "gpt-realtime".into(), None, None, 1)
            .await
            .unwrap();
        assert!(secret.starts_with("ek_"));
        assert_eq!(expiry, 1_000 + MIN_EPHEMERAL_TTL_SECS);
        assert_eq!(store.lookup(&secret).await.unwrap().issuer, "issuer");
    }
}
