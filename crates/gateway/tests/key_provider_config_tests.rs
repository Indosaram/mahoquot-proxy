mod common;

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::inbound::ApiKeys;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use serde_json::{json, Value};
use tower::ServiceExt;

const TEST_API_KEY: &str = "test-mgmt-key";

struct TestContext {
    auth_dir: PathBuf,
    config: GatewayConfig,
}

impl Drop for TestContext {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.auth_dir).ok();
    }
}

fn setup_test_context(tag: &str) -> TestContext {
    let auth_dir = std::env::temp_dir().join(format!(
        "mahoquot-key-provider-test-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&auth_dir).expect("create test auth dir");

    let config = GatewayConfig {
        auth_dir: auth_dir.clone(),
        api_keys: ApiKeys::new(vec![TEST_API_KEY.to_string()]),
        config_path: auth_dir.join("config.yaml"),
        ..GatewayConfig::default()
    };

    TestContext { auth_dir, config }
}

async fn send_request(
    app: &axum::Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req_builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TEST_API_KEY}"));

    let req_body = if let Some(b) = body {
        req_builder = req_builder.header(header::CONTENT_TYPE, "application/json");
        Body::from(serde_json::to_vec(&b).unwrap())
    } else {
        Body::empty()
    };

    let req = req_builder.body(req_body).expect("request build");
    let resp = app.clone().oneshot(req).await.expect("service oneshot");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let val: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, val)
}

async fn test_key_provider_lifecycle(endpoint: &str, key_name: &str) {
    let ctx = setup_test_context(key_name);
    let state = Arc::new(AppState::new(&ctx.config).expect("app state"));
    let app = create_app(state);

    let uri = format!("/v0/management{endpoint}");

    // 1. Initial GET -> empty list
    let (status, body) = send_request(&app, Method::GET, &uri, None).await;
    assert_eq!(status, StatusCode::OK, "GET initial status for {endpoint}");
    assert_eq!(
        body[key_name],
        json!([]),
        "Initial list must be empty for {endpoint}"
    );

    // 2. PUT bare array -> 200
    let (status, body) = send_request(
        &app,
        Method::PUT,
        &uri,
        Some(json!(["key-alpha", "key-beta"])),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "PUT bare array status for {endpoint}"
    );
    assert_eq!(body["status"], "ok");

    // 3. GET after bare PUT -> updated list
    let (status, body) = send_request(&app, Method::GET, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body[key_name],
        json!(["key-alpha", "key-beta"]),
        "GET after bare PUT for {endpoint}"
    );

    // 4. PUT wrapped in {"items": [...]} -> 200
    let (status, body) = send_request(
        &app,
        Method::PUT,
        &uri,
        Some(json!({ "items": ["key-gamma", "key-delta"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "PUT wrapped status for {endpoint}");
    assert_eq!(body["status"], "ok");

    // 5. GET after wrapped PUT -> updated list
    let (status, body) = send_request(&app, Method::GET, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body[key_name],
        json!(["key-gamma", "key-delta"]),
        "GET after wrapped PUT for {endpoint}"
    );

    // 6. PUT invalid body -> 400
    let (status, body) = send_request(
        &app,
        Method::PUT,
        &uri,
        Some(json!({ "invalid_field": 123 })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "PUT invalid body status for {endpoint}"
    );
    assert!(body["error"].is_string());

    // 7. PUT empty items object -> 400 (per upstream contract empty wrapped items is bad body)
    let (status, body) = send_request(&app, Method::PUT, &uri, Some(json!({ "items": [] }))).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "PUT empty items status for {endpoint}"
    );
    assert!(body["error"].is_string());

    // 8. PATCH by index -> 200
    let (status, body) = send_request(
        &app,
        Method::PATCH,
        &uri,
        Some(json!({ "index": 0, "value": "key-gamma-updated" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "PATCH by index status for {endpoint}"
    );
    assert_eq!(body["status"], "ok");

    // 9. GET after PATCH index
    let (status, body) = send_request(&app, Method::GET, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body[key_name],
        json!(["key-gamma-updated", "key-delta"]),
        "GET after PATCH index for {endpoint}"
    );

    // 10. PATCH by old/new -> 200
    let (status, body) = send_request(
        &app,
        Method::PATCH,
        &uri,
        Some(json!({ "old": "key-delta", "new": "key-delta-updated" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "PATCH old/new status for {endpoint}"
    );
    assert_eq!(body["status"], "ok");

    // 11. GET after PATCH old/new
    let (status, body) = send_request(&app, Method::GET, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body[key_name],
        json!(["key-gamma-updated", "key-delta-updated"]),
        "GET after PATCH old/new for {endpoint}"
    );

    // 12. PATCH by old/new with nonexistent old -> appends
    let (status, body) = send_request(
        &app,
        Method::PATCH,
        &uri,
        Some(json!({ "old": "nonexistent", "new": "key-epsilon-appended" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "PATCH append status for {endpoint}");
    assert_eq!(body["status"], "ok");

    // 13. GET after append
    let (status, body) = send_request(&app, Method::GET, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body[key_name],
        json!([
            "key-gamma-updated",
            "key-delta-updated",
            "key-epsilon-appended"
        ]),
        "GET after append for {endpoint}"
    );

    // 14. PATCH invalid body / missing fields -> 400
    let (status, body) =
        send_request(&app, Method::PATCH, &uri, Some(json!({ "foo": "bar" }))).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "PATCH invalid status for {endpoint}"
    );
    assert!(body["error"].is_string());

    // 15. DELETE by index (?index=1 -> removes "key-delta-updated")
    let del_uri_index = format!("{uri}?index=1");
    let (status, body) = send_request(&app, Method::DELETE, &del_uri_index, None).await;
    assert_eq!(status, StatusCode::OK, "DELETE index status for {endpoint}");
    assert_eq!(body["status"], "ok");

    // 16. GET after DELETE index
    let (status, body) = send_request(&app, Method::GET, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body[key_name],
        json!(["key-gamma-updated", "key-epsilon-appended"]),
        "GET after DELETE index for {endpoint}"
    );

    // 17. DELETE by value (?value=key-gamma-updated)
    let del_uri_value = format!("{uri}?value=key-gamma-updated");
    let (status, body) = send_request(&app, Method::DELETE, &del_uri_value, None).await;
    assert_eq!(status, StatusCode::OK, "DELETE value status for {endpoint}");
    assert_eq!(body["status"], "ok");

    // 18. GET after DELETE value
    let (status, body) = send_request(&app, Method::GET, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body[key_name],
        json!(["key-epsilon-appended"]),
        "GET after DELETE value for {endpoint}"
    );

    // 19. DELETE without params -> 400
    let (status, body) = send_request(&app, Method::DELETE, &uri, None).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "DELETE missing params status for {endpoint}"
    );
    assert!(body["error"].is_string());

    // 20. Persistence check across second process / AppState recreation
    let second_state = Arc::new(AppState::new(&ctx.config).expect("second app state"));
    let second_app = create_app(second_state);
    let (status, body) = send_request(&second_app, Method::GET, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body[key_name],
        json!(["key-epsilon-appended"]),
        "Persisted state across restart for {endpoint}"
    );
}

#[tokio::test]
async fn test_claude_api_key_endpoint_lifecycle() {
    test_key_provider_lifecycle("/claude-api-key", "claude-api-key").await;
}

#[tokio::test]
async fn test_codex_api_key_endpoint_lifecycle() {
    test_key_provider_lifecycle("/codex-api-key", "codex-api-key").await;
}

#[tokio::test]
async fn test_gemini_api_key_endpoint_lifecycle() {
    test_key_provider_lifecycle("/gemini-api-key", "gemini-api-key").await;
}

#[tokio::test]
async fn test_xai_api_key_endpoint_lifecycle() {
    test_key_provider_lifecycle("/xai-api-key", "xai-api-key").await;
}

#[tokio::test]
async fn test_vertex_api_key_endpoint_lifecycle() {
    test_key_provider_lifecycle("/vertex-api-key", "vertex-api-key").await;
}

#[tokio::test]
async fn test_interactions_api_key_endpoint_lifecycle() {
    test_key_provider_lifecycle("/interactions-api-key", "interactions-api-key").await;
}

#[tokio::test]
async fn test_unauthenticated_request_is_rejected() {
    let ctx = setup_test_context("unauth");
    let state = Arc::new(AppState::new(&ctx.config).expect("app state"));
    let app = create_app(state);

    let req = Request::builder()
        .method(Method::GET)
        .uri("/v0/management/claude-api-key")
        .body(Body::empty())
        .expect("request");

    let resp = app.oneshot(req).await.expect("service oneshot");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
