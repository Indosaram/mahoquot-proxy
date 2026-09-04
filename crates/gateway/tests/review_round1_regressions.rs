//! Regressions for the blocking findings verified in review round 1.
//!
//! Each test names the verified blocker it pins (V1..V8 in
//! `.omo/ulw-loop/codereview-rounds-20260904/verified-blockers.md`) and fails
//! against the unfixed code for that specific defect.

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

const TEST_API_KEY: &str = "test-review-round1-key";

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
        "mahoquot-review-round1-{}-{}-{}",
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
        max_failover: 3,
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

/// V1: a scoped key whose first 10 bytes end inside a multi-byte character
/// must not panic while `mutate_lock` is held. "abcdefghi" is 9 bytes, so the
/// following 3-byte character straddles byte offset 10.
#[tokio::test]
async fn v1_scoped_key_prefix_survives_multibyte_raw_key() {
    let ctx = setup_test_context("scoped-key-multibyte");
    let state = Arc::new(AppState::new(&ctx.config).expect("app state"));
    let app = create_app(state);

    let (status, body) = send_request(
        &app,
        Method::POST,
        "/v0/management/scoped-keys",
        Some(json!({"name": "multibyte"})),
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::CREATED,
        "create scoped key answered {status}: {body}"
    );

    let id = body
        .get("key")
        .and_then(|key| key.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    assert!(!id.is_empty(), "created key has no id: {body}");

    let (status, body) = send_request(
        &app,
        Method::PATCH,
        &format!("/v0/management/scoped-keys/{id}"),
        Some(json!({"raw_key": "abcdefghi\u{20ac}-patched"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "patching with a multibyte raw key answered {status}: {body}"
    );

    let (status, _) = send_request(
        &app,
        Method::GET,
        "/v0/management/scoped-keys",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "listing keys after the patch failed");
}


