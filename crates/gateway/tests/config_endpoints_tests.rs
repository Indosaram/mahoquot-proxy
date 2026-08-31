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

const TEST_API_KEY: &str = "test-config-mgmt-key";

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
        "mahoquot-config-test-{}-{}-{}",
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

async fn send_request_unauthed(app: &axum::Router, method: Method, uri: &str) -> StatusCode {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("request build");
    let resp = app.clone().oneshot(req).await.expect("service oneshot");
    resp.status()
}

// ---------------------------------------------------------------------------
// 1. /force-model-prefix (bool)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_force_model_prefix_lifecycle() {
    let ctx = setup_test_context("force-model-prefix");
    let state = Arc::new(AppState::new(&ctx.config).expect("app state"));
    let app = create_app(state);
    let uri = "/v0/management/force-model-prefix";

    // 1. Initial GET -> false
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["force-model-prefix"], json!(false));

    // 2. POST update to true -> 200
    let (status, body) = send_request(&app, Method::POST, uri, Some(json!({"value": true}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    // 3. GET verify -> true
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["force-model-prefix"], json!(true));

    // 4. PUT update back to false -> 200
    let (status, body) = send_request(&app, Method::PUT, uri, Some(json!({"value": false}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    // 5. GET verify -> false
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["force-model-prefix"], json!(false));

    // 6. Invalid payload -> 400, value unchanged
    let (status, body) = send_request(
        &app,
        Method::POST,
        uri,
        Some(json!({"value": "not-a-bool"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid body");

    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["force-model-prefix"], json!(false));
}

// ---------------------------------------------------------------------------
// 2. /max-retry-credentials (usize, default 3)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_max_retry_credentials_lifecycle() {
    let ctx = setup_test_context("max-retry-credentials");
    let state = Arc::new(AppState::new(&ctx.config).expect("app state"));
    let app = create_app(state);
    let uri = "/v0/management/max-retry-credentials";

    // 1. Initial GET -> 3
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["max-retry-credentials"], json!(3));

    // 2. POST update to 7 -> 200
    let (status, body) = send_request(&app, Method::POST, uri, Some(json!({"value": 7}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    // 3. GET verify -> 7
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["max-retry-credentials"], json!(7));

    // 4. PUT update to 1 -> 200
    let (status, body) = send_request(&app, Method::PUT, uri, Some(json!({"value": 1}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    // 5. GET verify -> 1
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["max-retry-credentials"], json!(1));

    // 6. Invalid payload (negative number or wrong type) -> 400
    let (status, body) = send_request(&app, Method::POST, uri, Some(json!({"value": -5}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid body");

    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["max-retry-credentials"], json!(1));
}

// ---------------------------------------------------------------------------
// 3. /max-retry-interval (i64, default 0)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_max_retry_interval_lifecycle() {
    let ctx = setup_test_context("max-retry-interval");
    let state = Arc::new(AppState::new(&ctx.config).expect("app state"));
    let app = create_app(state);
    let uri = "/v0/management/max-retry-interval";

    // 1. Initial GET -> 0
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["max-retry-interval"], json!(0));

    // 2. POST update to 15 -> 200
    let (status, body) = send_request(&app, Method::POST, uri, Some(json!({"value": 15}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    // 3. GET verify -> 15
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["max-retry-interval"], json!(15));

    // 4. PUT update to 5 -> 200
    let (status, body) = send_request(&app, Method::PUT, uri, Some(json!({"value": 5}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    // 5. GET verify -> 5
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["max-retry-interval"], json!(5));

    // 6. Invalid payload -> 400
    let (status, body) =
        send_request(&app, Method::POST, uri, Some(json!({"value": "five"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid body");

    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["max-retry-interval"], json!(5));
}

// ---------------------------------------------------------------------------
// 4. /logging-to-file (bool, default true)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_logging_to_file_lifecycle() {
    let ctx = setup_test_context("logging-to-file");
    let state = Arc::new(AppState::new(&ctx.config).expect("app state"));
    let app = create_app(state);
    let uri = "/v0/management/logging-to-file";

    // 1. Initial GET -> true
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["logging-to-file"], json!(true));

    // 2. POST update to false -> 200
    let (status, body) = send_request(&app, Method::POST, uri, Some(json!({"value": false}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    // 3. GET verify -> false
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["logging-to-file"], json!(false));

    // 4. PUT update back to true -> 200
    let (status, body) = send_request(&app, Method::PUT, uri, Some(json!({"value": true}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    // 5. GET verify -> true
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["logging-to-file"], json!(true));

    // 6. Invalid payload -> 400
    let (status, body) = send_request(&app, Method::POST, uri, Some(json!({"value": 123}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid body");
}

// ---------------------------------------------------------------------------
// 5. /logs-max-total-size-mb (i64, default 100)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_logs_max_total_size_mb_lifecycle() {
    let ctx = setup_test_context("logs-max-total-size-mb");
    let state = Arc::new(AppState::new(&ctx.config).expect("app state"));
    let app = create_app(state);
    let uri = "/v0/management/logs-max-total-size-mb";

    // 1. Initial GET -> 100
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["logs-max-total-size-mb"], json!(100));

    // 2. POST update to 250 -> 200
    let (status, body) = send_request(&app, Method::POST, uri, Some(json!({"value": 250}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    // 3. GET verify -> 250
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["logs-max-total-size-mb"], json!(250));

    // 4. PUT update to 50 -> 200
    let (status, body) = send_request(&app, Method::PUT, uri, Some(json!({"value": 50}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    // 5. GET verify -> 50
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["logs-max-total-size-mb"], json!(50));

    // 6. Invalid payload -> 400
    let (status, body) =
        send_request(&app, Method::POST, uri, Some(json!({"value": "invalid"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid body");
}

// ---------------------------------------------------------------------------
// 6. /error-logs-max-files (i64, default 0)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_error_logs_max_files_lifecycle() {
    let ctx = setup_test_context("error-logs-max-files");
    let state = Arc::new(AppState::new(&ctx.config).expect("app state"));
    let app = create_app(state);
    let uri = "/v0/management/error-logs-max-files";

    // 1. Initial GET -> 0
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["error-logs-max-files"], json!(0));

    // 2. POST update to 20 -> 200
    let (status, body) = send_request(&app, Method::POST, uri, Some(json!({"value": 20}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    // 3. GET verify -> 20
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["error-logs-max-files"], json!(20));

    // 4. PUT update to 5 -> 200
    let (status, body) = send_request(&app, Method::PUT, uri, Some(json!({"value": 5}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    // 5. GET verify -> 5
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["error-logs-max-files"], json!(5));

    // 6. Invalid payload -> 400
    let (status, body) = send_request(&app, Method::POST, uri, Some(json!({"value": null}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid body");
}

// ---------------------------------------------------------------------------
// 7. /openai-compatibility (Vec<String>)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_openai_compatibility_lifecycle() {
    let ctx = setup_test_context("openai-compatibility");
    let state = Arc::new(AppState::new(&ctx.config).expect("app state"));
    let app = create_app(state);
    let uri = "/v0/management/openai-compatibility";

    // 1. Initial GET -> []
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["openai-compatibility"], json!([]));

    // 2. POST bare array -> 200
    let (status, body) = send_request(
        &app,
        Method::POST,
        uri,
        Some(json!([
            "http://localhost:8000/v1",
            "http://localhost:8001/v1"
        ])),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    // 3. GET verify -> updated
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["openai-compatibility"],
        json!(["http://localhost:8000/v1", "http://localhost:8001/v1"])
    );

    // 4. PUT wrapped in {"items": [...]} -> 200
    let (status, body) = send_request(
        &app,
        Method::PUT,
        uri,
        Some(json!({"items": ["http://localhost:9000/v1"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    // 5. GET verify -> ["http://localhost:9000/v1"]
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["openai-compatibility"],
        json!(["http://localhost:9000/v1"])
    );

    // 6. DELETE by value
    let (status, body) = send_request(
        &app,
        Method::DELETE,
        &format!("{uri}?value=http://localhost:9000/v1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    // 7. GET verify -> []
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["openai-compatibility"], json!([]));

    // 8. Invalid payload -> 400
    let (status, body) = send_request(&app, Method::POST, uri, Some(json!("not-an-array"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid body");
}

// ---------------------------------------------------------------------------
// 8. /oauth-model-alias (Value)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_oauth_model_alias_lifecycle() {
    let ctx = setup_test_context("oauth-model-alias");
    let state = Arc::new(AppState::new(&ctx.config).expect("app state"));
    let app = create_app(state);
    let uri = "/v0/management/oauth-model-alias";

    // 1. Initial GET -> null
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["oauth-model-alias"], Value::Null);

    // 2. POST alias map -> 200
    let alias_map = json!({
        "openai": {
            "gpt-4": "gpt-4o",
            "gpt-3.5-turbo": "gpt-4o-mini"
        }
    });
    let (status, body) = send_request(&app, Method::POST, uri, Some(alias_map.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    // 3. GET verify -> alias_map
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["oauth-model-alias"], alias_map);

    // 4. PUT wrapped in {"items": ...} -> 200
    let updated_map = json!({
        "anthropic": {
            "claude-v1": "claude-3-5-sonnet"
        }
    });
    let (status, body) =
        send_request(&app, Method::PUT, uri, Some(json!({"items": updated_map}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    // 5. GET verify -> updated_map
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["oauth-model-alias"], updated_map);

    // 6. DELETE without channel -> 400 missing channel
    let (status, body) = send_request(&app, Method::DELETE, uri, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "missing channel");

    // 7. DELETE with channel -> 400 channel not found (channel-keyed clear semantics)
    let (status, body) =
        send_request(&app, Method::DELETE, &format!("{uri}?channel=foo"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "channel not found");
}

// ---------------------------------------------------------------------------
// 9. /oauth-excluded-models (BTreeMap<String, Vec<String>>)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_oauth_excluded_models_lifecycle() {
    let ctx = setup_test_context("oauth-excluded-models");
    let state = Arc::new(AppState::new(&ctx.config).expect("app state"));
    let app = create_app(state);
    let uri = "/v0/management/oauth-excluded-models";

    // 1. Initial GET -> {}
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["oauth-excluded-models"], json!({}));

    // 2. POST excluded map -> 200
    let excluded = json!({
        "openai": ["gpt-4-32k", "gpt-4-vision-preview"],
        "anthropic": ["claude-2.0"]
    });
    let (status, body) = send_request(&app, Method::POST, uri, Some(excluded.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    // 3. GET verify -> excluded
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["oauth-excluded-models"], excluded);

    // 4. DELETE channel-keyed refusal without channel
    let (status, body) = send_request(&app, Method::DELETE, uri, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "missing channel");
}

// ---------------------------------------------------------------------------
// 10. /oauth-request-scoped-errors (Value)
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_oauth_request_scoped_errors_lifecycle() {
    let ctx = setup_test_context("oauth-request-scoped-errors");
    let state = Arc::new(AppState::new(&ctx.config).expect("app state"));
    let app = create_app(state);
    let uri = "/v0/management/oauth-request-scoped-errors";

    // 1. Initial GET -> null
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["oauth-request-scoped-errors"], Value::Null);

    // 2. POST scoped errors config -> 200
    let errors_cfg = json!({
        "openai": ["rate_limit_exceeded", "insufficient_quota"]
    });
    let (status, body) = send_request(&app, Method::POST, uri, Some(errors_cfg.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    // 3. GET verify -> errors_cfg
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["oauth-request-scoped-errors"], errors_cfg);

    // 4. PUT update -> 200
    let updated_cfg = json!({
        "anthropic": ["overloaded_error"]
    });
    let (status, body) = send_request(&app, Method::PUT, uri, Some(updated_cfg.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    // 5. GET verify -> updated_cfg
    let (status, body) = send_request(&app, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["oauth-request-scoped-errors"], updated_cfg);
}

// ---------------------------------------------------------------------------
// 11. Cross-cutting: Restart persistence for all 10 endpoints
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_all_10_config_endpoints_survive_restart_persistence() {
    let ctx = setup_test_context("restart-persistence");

    // Step 1: Initial state & server
    {
        let state = Arc::new(AppState::new(&ctx.config).expect("app state 1"));
        let app = create_app(state);

        // Update all 10 endpoints
        let updates: Vec<(&str, Value)> = vec![
            ("/force-model-prefix", json!({"value": true})),
            ("/max-retry-credentials", json!({"value": 8})),
            ("/max-retry-interval", json!({"value": 12})),
            ("/logging-to-file", json!({"value": false})),
            ("/logs-max-total-size-mb", json!({"value": 450})),
            ("/error-logs-max-files", json!({"value": 15})),
            ("/openai-compatibility", json!(["http://localhost:5000/v1"])),
            (
                "/oauth-model-alias",
                json!({"google": {"gemini-pro": "gemini-1.5-pro"}}),
            ),
            ("/oauth-excluded-models", json!({"openai": ["gpt-4-old"]})),
            (
                "/oauth-request-scoped-errors",
                json!({"anthropic": ["rate_limit_exceeded"]}),
            ),
        ];

        for (endpoint, body) in updates {
            let uri = format!("/v0/management{endpoint}");
            let (status, resp_body) = send_request(&app, Method::POST, &uri, Some(body)).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "POST failed for {endpoint}: {resp_body}"
            );
            assert_eq!(resp_body["status"], "ok");
        }
    }

    // Verify config.yaml exists on disk
    let config_yaml_path = ctx.auth_dir.join("config.yaml");
    assert!(config_yaml_path.exists(), "config.yaml must exist on disk");
    let raw_yaml = std::fs::read_to_string(&config_yaml_path).expect("read config.yaml");
    assert!(raw_yaml.contains("force-model-prefix: true"));
    assert!(raw_yaml.contains("max-retry-credentials: 8"));
    assert!(raw_yaml.contains("max-retry-interval: 12"));
    assert!(raw_yaml.contains("logging-to-file: false"));
    assert!(raw_yaml.contains("logs-max-total-size-mb: 450"));
    assert!(raw_yaml.contains("error-logs-max-files: 15"));

    // Step 2: Restart simulator: re-initialize AppState from the same config / disk
    {
        let restarted_state = Arc::new(AppState::new(&ctx.config).expect("restarted app state"));
        let restarted_app = create_app(restarted_state);

        // GET all 10 endpoints and verify persisted values
        let expectations: Vec<(&str, &str, Value)> = vec![
            ("/force-model-prefix", "force-model-prefix", json!(true)),
            ("/max-retry-credentials", "max-retry-credentials", json!(8)),
            ("/max-retry-interval", "max-retry-interval", json!(12)),
            ("/logging-to-file", "logging-to-file", json!(false)),
            (
                "/logs-max-total-size-mb",
                "logs-max-total-size-mb",
                json!(450),
            ),
            ("/error-logs-max-files", "error-logs-max-files", json!(15)),
            (
                "/openai-compatibility",
                "openai-compatibility",
                json!(["http://localhost:5000/v1"]),
            ),
            (
                "/oauth-model-alias",
                "oauth-model-alias",
                json!({"google": {"gemini-pro": "gemini-1.5-pro"}}),
            ),
            (
                "/oauth-excluded-models",
                "oauth-excluded-models",
                json!({"openai": ["gpt-4-old"]}),
            ),
            (
                "/oauth-request-scoped-errors",
                "oauth-request-scoped-errors",
                json!({"anthropic": ["rate_limit_exceeded"]}),
            ),
        ];

        for (endpoint, key, expected_val) in expectations {
            let uri = format!("/v0/management{endpoint}");
            let (status, body) = send_request(&restarted_app, Method::GET, &uri, None).await;
            assert_eq!(status, StatusCode::OK, "GET failed for {endpoint}");
            assert_eq!(
                body[key], expected_val,
                "Value for {endpoint} ({key}) did not survive restart"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 12. Security: Unauthenticated access to config endpoints is rejected
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_unauthenticated_requests_are_rejected() {
    let ctx = setup_test_context("unauthed");
    let state = Arc::new(AppState::new(&ctx.config).expect("app state"));
    let app = create_app(state);

    let endpoints = vec![
        "/v0/management/force-model-prefix",
        "/v0/management/max-retry-credentials",
        "/v0/management/max-retry-interval",
        "/v0/management/logging-to-file",
        "/v0/management/logs-max-total-size-mb",
        "/v0/management/error-logs-max-files",
        "/v0/management/openai-compatibility",
        "/v0/management/oauth-model-alias",
        "/v0/management/oauth-excluded-models",
        "/v0/management/oauth-request-scoped-errors",
    ];

    for endpoint in endpoints {
        let status = send_request_unauthed(&app, Method::GET, endpoint).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "Endpoint {endpoint} must reject unauthenticated requests"
        );
    }
}
