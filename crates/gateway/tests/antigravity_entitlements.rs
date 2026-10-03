use std::sync::{Arc, Mutex};

use axum::{body::Body, extract::State, http::{Request, StatusCode}, response::IntoResponse, routing::post, Json, Router};
use http_body_util::BodyExt;
use mahoquot_gateway::{config::GatewayConfig, quota::refresh_account_usage, routes::create_app, state::AppState};
use mahoquot_types::{Health, PoolMember, Strategy};
use serde_json::{json, Value};
use tower::ServiceExt;

#[path = "common/mod.rs"]
mod common;

#[derive(Clone, Default)]
struct Upstream {
    catalogs: Arc<Mutex<std::collections::BTreeMap<String, Value>>>,
    calls: Arc<Mutex<Vec<(String, String)>>>,
}

async fn upstream(
    State(state): State<Upstream>,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let account = headers.get("authorization").unwrap().to_str().unwrap()
        .strip_prefix("Bearer ").unwrap().to_owned();
    if uri.path().ends_with(":retrieveUserQuotaSummary") {
        return Json(json!({"groups": []})).into_response();
    }
    if uri.path().ends_with(":loadCodeAssist") {
        return Json(json!({"paidTier": {"id": if account == "ultra" {"g1-ultra-tier"} else {"g1-pro-tier"}}})).into_response();
    }
    if uri.path().ends_with(":fetchAvailableModels") {
        assert_eq!(body["project"], "fixture-project");
        assert!(headers.get("user-agent").unwrap().to_str().unwrap().starts_with("antigravity/"));
        return match state.catalogs.lock().unwrap().get(&account) {
            Some(catalog) => Json(catalog.clone()).into_response(),
            None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        };
    }
    state.calls.lock().unwrap().push((account, body["model"].as_str().unwrap().to_owned()));
    (
        [("content-type", "text/event-stream")],
        "data: {\"response\":{\"responseId\":\"fixture\",\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":1,\"candidatesTokenCount\":1,\"totalTokenCount\":2}}}\n\n",
    ).into_response()
}

struct Fixture {
    dir: std::path::PathBuf,
    server: tokio::task::JoinHandle<()>,
    state: Arc<AppState>,
    upstream: Upstream,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        std::fs::remove_dir_all(&self.dir).unwrap();
    }
}

fn catalog(models: &[&str]) -> Value {
    json!({"models": models.iter().map(|id| ((*id).to_owned(), json!({"maxTokens": 1000000, "maxOutputTokens": 128000, "supportsThinking": true}))).collect::<serde_json::Map<String, Value>>()})
}

impl Fixture {
    async fn new() -> Self {
        let upstream_state = Upstream::default();
        upstream_state.catalogs.lock().unwrap().insert("pro".into(), catalog(&["claude-sonnet-4-6", "gemini-3-flash"]));
        upstream_state.catalogs.lock().unwrap().insert("ultra".into(), catalog(&["claude-opus-5-5-high", "claude-sonnet-5-5-high", "gemini-3-flash"]));
        let mut listener = None;
        for port in 18840..=18899 {
            if let Ok(bound) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                listener = Some(bound);
                break;
            }
        }
        let listener = listener.expect("free test port");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().fallback(post(upstream)).with_state(upstream_state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = common::unique_temp_dir("agy-entitlements");
        for (file, id) in [("a-pro", "pro"), ("z-ultra", "ultra")] {
            std::fs::write(dir.join(format!("{file}.json")), json!({
                "type": "antigravity", "identity_slug": id, "access_token": id,
                "refresh_token": "fixture", "project_id": "fixture-project",
                "email": format!("{id}@example.test"), "expired": "2099-01-01T00:00:00Z",
                "upstream_override": base, "usage_override": base
            }).to_string()).unwrap();
        }
        let state = Arc::new(AppState::new(&GatewayConfig {
            auth_dir: dir.clone(), config_path: dir.join("config.yaml"),
            strategy: Strategy::FillFirst, auth_refresh_enabled: false,
            ..GatewayConfig::default()
        }).unwrap());
        Self { dir, server, state, upstream: upstream_state }
    }

    async fn refresh(&self, id: &str) {
        let member = self.state.pool.load().find_member(id).unwrap();
        refresh_account_usage(&self.state, &member).await.unwrap();
    }

    async fn request(&self, model: &str) -> (StatusCode, Value) {
        let response = create_app(self.state.clone()).oneshot(Request::builder()
            .method("POST").uri("/v1/chat/completions").header("content-type", "application/json")
            .body(Body::from(json!({"model": model, "messages": [{"role": "user", "content": "fixture"}]}).to_string())).unwrap()).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap())
    }
}

#[tokio::test]
async fn ultra_models_skip_pro_while_existing_models_keep_their_accounts() {
    // Given Pro precedes Ultra under fill-first, with disjoint Claude entitlements.
    let fixture = Fixture::new().await;
    fixture.refresh("pro").await;
    fixture.refresh("ultra").await;
    // When each model is requested through the real HTTP router.
    for model in ["claude-opus-5-5-high", "claude-sonnet-5-5-high", "claude-sonnet-4-6", "gemini-3-flash"] {
        let (status, body) = fixture.request(model).await;
        assert_eq!(status, StatusCode::OK, "{model}: {body}");
    }
    // Then only entitled accounts receive traffic; no account is disabled.
    assert_eq!(*fixture.upstream.calls.lock().unwrap(), vec![
        ("ultra".into(), "claude-opus-5-5-high".into()),
        ("ultra".into(), "claude-sonnet-5-5-high".into()),
        ("pro".into(), "claude-sonnet-4-6".into()),
        ("pro".into(), "gemini-3-flash".into()),
    ]);
    assert!(fixture.state.pool.load().members.iter().all(|m| m.health() == Health::Available));
}

#[tokio::test]
async fn undiscovered_accounts_do_not_inherit_another_accounts_new_models() {
    // Given only the Ultra account has a verified catalog.
    let fixture = Fixture::new().await;
    fixture.refresh("ultra").await;
    // When a new model is requested with Pro first in pool order.
    let (status, body) = fixture.request("claude-opus-5-5-high").await;
    // Then the uninitialized Pro account is skipped rather than probed.
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(*fixture.upstream.calls.lock().unwrap(), vec![("ultra".into(), "claude-opus-5-5-high".into())]);
}

#[tokio::test]
async fn downgrade_removes_ultra_models_from_listing_and_routing() {
    // Given a previously entitled Ultra account.
    let fixture = Fixture::new().await;
    fixture.refresh("pro").await;
    fixture.refresh("ultra").await;
    assert!(fixture.state.pool.load().models.iter().any(|m| m.id == "claude-opus-5-5-high"));
    fixture.upstream.catalogs.lock().unwrap().insert("ultra".into(), catalog(&["claude-sonnet-4-6", "gemini-3-flash"]));
    // When the authoritative model set changes on refresh.
    fixture.refresh("ultra").await;
    // Then discovery and routing both revoke the old capability without disabling the account.
    assert!(!fixture.state.pool.load().models.iter().any(|m| m.id.contains("5-5")));
    let (status, _) = fixture.request("claude-opus-5-5-high").await;
    assert!(!status.is_success());
    assert!(fixture.upstream.calls.lock().unwrap().is_empty());
    assert!(fixture.state.pool.load().members.iter().all(|m| m.health() == Health::Available));
}

#[tokio::test]
async fn transient_catalog_failure_preserves_last_verified_entitlements() {
    let fixture = Fixture::new().await;
    fixture.refresh("pro").await;
    fixture.refresh("ultra").await;
    fixture.upstream.catalogs.lock().unwrap().remove("ultra");
    let member = fixture.state.pool.load().find_member("ultra").unwrap();
    let _refresh_result = refresh_account_usage(&fixture.state, &member).await;
    let (status, body) = fixture.request("claude-sonnet-5-5-high").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(*fixture.upstream.calls.lock().unwrap(), vec![("ultra".into(), "claude-sonnet-5-5-high".into())]);
}

#[tokio::test]
async fn ultra_cooldown_never_falls_back_to_unentitled_pro() {
    let fixture = Fixture::new().await;
    fixture.refresh("pro").await;
    fixture.refresh("ultra").await;
    let member = fixture.state.pool.load().find_member("ultra").unwrap();
    assert!(member.set_group_cooldown("claude-opus-5-5-high", i64::MAX));
    let (status, _) = fixture.request("claude-opus-5-5-high").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(fixture.upstream.calls.lock().unwrap().is_empty());
    assert!(member.group_available("gemini-3-flash", 0));
    assert!(!member.is_manually_disabled());
}

#[tokio::test]
async fn pro_scoped_catalog_does_not_advertise_ultra_models() {
    let fixture = Fixture::new().await;
    fixture.refresh("pro").await;
    fixture.refresh("ultra").await;
    let scope: mahoquot_gateway::management::settings::ScopedApiKey = serde_json::from_value(json!({
        "id": "fixture", "name": "fixture", "key_identifier": "fixture", "key_prefix": "fixture",
        "allowed_accounts": ["pro"], "allowed_providers": [], "allowed_models": [],
        "token_limit": 0, "token_used": 0, "is_active": true, "created_at_ms": 0
    })).unwrap();
    let pool = fixture.state.pool.load();
    let models = mahoquot_gateway::models_route::scoped_model_entries(&pool, &scope);
    assert!(models.iter().any(|m| m.id == "claude-sonnet-4-6"));
    assert!(!models.iter().any(|m| m.id.contains("5-5")));
    assert_eq!(pool.routable_accounts_for_model("claude-opus-5-5-high").iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["ultra"]);
}
