mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use common::unique_temp_dir;
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::inbound::ApiKeys;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use serde_json::{json, Value};
use tower::ServiceExt;

const TEST_KEY: &str = "test-parity-p1-key";

fn setup_app(auth_dir: &std::path::Path) -> axum::Router {
    let config = GatewayConfig {
        auth_dir: auth_dir.to_path_buf(),
        api_keys: ApiKeys::new(vec![TEST_KEY.to_string()]),
        config_path: auth_dir.join("config.yaml"),
        ..GatewayConfig::default()
    };
    create_app(Arc::new(AppState::new(&config).expect("state")))
}

#[tokio::test]
async fn test_keep_alive_public_endpoint() {
    let temp_dir = unique_temp_dir("p1-test-keepalive");
    std::fs::create_dir_all(&temp_dir).unwrap();
    let app = setup_app(&temp_dir);

    let req = Request::builder()
        .method("GET")
        .uri("/keep-alive")
        .body(Body::empty())
        .expect("request");
    let resp = app.oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body, json!({ "status": "ok" }));
    std::fs::remove_dir_all(temp_dir).ok();
}

#[tokio::test]
async fn test_realtime_sip_and_live_subpaths() {
    let temp_dir = unique_temp_dir("p1-test-realtime");
    std::fs::create_dir_all(&temp_dir).unwrap();
    let app = setup_app(&temp_dir);

    // 1. Live sideband GET: /v1/live/:call_id and /live/:call_id
    for uri in &["/v1/live/call_abc123", "/live/call_abc123"] {
        let req = Request::builder()
            .method("GET")
            .uri(*uri)
            .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
            .body(Body::empty())
            .expect("request");
        let resp = app.clone().oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::UPGRADE_REQUIRED);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let body: Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(body["error"], "WebSocket upgrade required");
    }

    // 2. Realtime SIP actions: accept, reject, refer
    for action in &["accept", "reject", "refer"] {
        let req = Request::builder()
            .method("POST")
            .uri(format!("/v1/realtime/calls/call_123/{action}"))
            .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .expect("request");
        let resp = app.clone().oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let body: Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(body["error"]["code"], "realtime_capability_not_supported");
        let expected_msg =
            format!("Realtime SIP {action} are not supported by the ChatGPT/Codex OAuth upstream");
        assert_eq!(body["error"]["message"], expected_msg);
    }

    // 3. Realtime hangup
    let req = Request::builder()
        .method("POST")
        .uri("/v1/realtime/calls/call_123/hangup")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
        .body(Body::empty())
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["error"]["code"], "realtime_call_not_found");

    // 4. Transcription & translations
    let req = Request::builder()
        .method("POST")
        .uri("/v1/realtime/transcription_sessions")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
        .body(Body::empty())
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);

    for method in &["GET", "POST"] {
        let req = Request::builder()
            .method(*method)
            .uri("/v1/realtime/translations")
            .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
            .body(Body::empty())
            .expect("request");
        let resp = app.clone().oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    let req = Request::builder()
        .method("POST")
        .uri("/v1/realtime/translations/client_secrets")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
        .body(Body::empty())
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);

    std::fs::remove_dir_all(temp_dir).ok();
}

#[tokio::test]
async fn test_auth_files_fields_selective_patch() {
    let temp_dir = unique_temp_dir("p1-test-authfields");
    std::fs::create_dir_all(&temp_dir).unwrap();
    let auth_file_path = temp_dir.join("claude-test.json");
    let initial_content = json!({
        "type": "claude",
        "email": "test@example.com",
        "disabled": false,
        "proxy_url": "http://old-proxy.local",
        "notes": "initial note"
    });
    std::fs::write(
        &auth_file_path,
        serde_json::to_string_pretty(&initial_content).unwrap(),
    )
    .unwrap();

    let app = setup_app(&temp_dir);

    // Patch selective fields: proxy_url, notes, prefix, headers.X-Custom
    let patch_payload = json!({
        "name": "claude-test.json",
        "proxy_url": "http://new-proxy.local",
        "prefix": "custom-prefix",
        "notes": Value::Null,
        "headers.X-Custom": "custom-header-val"
    });

    let req = Request::builder()
        .method("PATCH")
        .uri("/v0/management/auth-files/fields")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&patch_payload).unwrap()))
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::OK);

    // Read file from disk and assert changes
    let content: Value =
        serde_json::from_str(&std::fs::read_to_string(&auth_file_path).unwrap()).unwrap();
    assert_eq!(content["proxy_url"], "http://new-proxy.local");
    assert_eq!(content["prefix"], "custom-prefix");
    assert!(content.get("notes").is_none());
    assert_eq!(content["headers"]["X-Custom"], "custom-header-val");
    assert_eq!(content["email"], "test@example.com");
    assert_eq!(content["disabled"], false);

    std::fs::remove_dir_all(temp_dir).ok();
}

#[tokio::test]
async fn test_mount_difference_aliases() {
    let temp_dir = unique_temp_dir("p1-test-aliases");
    std::fs::create_dir_all(&temp_dir).unwrap();
    let app = setup_app(&temp_dir);

    // 1. /interactions and /v1beta/interactions
    for uri in &["/interactions", "/v1beta/interactions"] {
        let req = Request::builder()
            .method("POST")
            .uri(*uri)
            .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"model": "non-existent-model", "input": "hello"}"#,
            ))
            .expect("request");
        let resp = app.clone().oneshot(req).await.expect("response");
        assert!(
            resp.status() == StatusCode::SERVICE_UNAVAILABLE
                || resp.status() == StatusCode::BAD_REQUEST
        );
    }

    // 2. /alpha/search and /v1/alpha/search
    for uri in &["/alpha/search", "/v1/alpha/search"] {
        let req = Request::builder()
            .method("POST")
            .uri(*uri)
            .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .expect("request");
        let resp = app.clone().oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // 3. /models and /v1/models
    for uri in &["/models", "/v1/models"] {
        let req = Request::builder()
            .method("GET")
            .uri(*uri)
            .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
            .body(Body::empty())
            .expect("request");
        let resp = app.clone().oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // 4. /videos, /v1/videos
    for uri in &[
        "/videos",
        "/v1/videos",
        "/videos/generations",
        "/v1/videos/generations",
    ] {
        let req = Request::builder()
            .method("POST")
            .uri(*uri)
            .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"model": "unknown-video"}"#))
            .expect("request");
        let resp = app.clone().oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // 5. /videos/:id and /openai/v1/videos/:video_id/content
    for uri in &[
        "/videos/v_123",
        "/v1/videos/v_123",
        "/videos/v_123/content",
        "/openai/v1/videos/v_123/content",
    ] {
        let req = Request::builder()
            .method("GET")
            .uri(*uri)
            .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
            .body(Body::empty())
            .expect("request");
        let resp = app.clone().oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // 6. /oauth-callback and /v0/management/oauth-callback
    for uri in &["/oauth-callback", "/v0/management/oauth-callback"] {
        let req = Request::builder()
            .method("GET")
            .uri(*uri)
            .body(Body::empty())
            .expect("request");
        let resp = app.clone().oneshot(req).await.expect("response");
        assert!(resp.status() == StatusCode::BAD_REQUEST || resp.status() == StatusCode::OK);
    }

    // 7. Top-level aliases for core inference routes: /chat/completions, /completions, /messages, /messages/count_tokens
    for uri in &[
        "/chat/completions",
        "/v1/chat/completions",
        "/completions",
        "/v1/completions",
        "/messages",
        "/v1/messages",
    ] {
        let req = Request::builder()
            .method("POST")
            .uri(*uri)
            .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}]}"#,
            ))
            .expect("request");
        let resp = app.clone().oneshot(req).await.expect("response");
        // With empty pool, all return 503 auth_not_found / no healthy account
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // 8. Responses aliases: /responses, /v1/responses, /responses/compact, /v1/responses/compact
    for uri in &["/responses/compact", "/v1/responses/compact"] {
        let req = Request::builder()
            .method("POST")
            .uri(*uri)
            .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"model": "gpt-4o"}"#))
            .expect("request");
        let resp = app.clone().oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    std::fs::remove_dir_all(temp_dir).ok();
}

#[tokio::test]
async fn test_auth_urls_endpoint_family() {
    let temp_dir = unique_temp_dir("p1-test-authurls");
    std::fs::create_dir_all(&temp_dir).unwrap();
    let app = setup_app(&temp_dir);

    for provider_endpoint in &[
        "/v0/management/anthropic-auth-url",
        "/v0/management/kimi-auth-url",
        "/v0/management/xai-auth-url",
        "/v0/management/codex-auth-url",
        "/v0/management/antigravity-auth-url",
    ] {
        let req = Request::builder()
            .method("GET")
            .uri(*provider_endpoint)
            .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
            .body(Body::empty())
            .expect("request");
        let resp = app.clone().oneshot(req).await.expect("response");
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "Failed for endpoint {provider_endpoint}"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let body: Value = serde_json::from_slice(&bytes).expect("json");
        assert!(
            body.get("url").is_some()
                || body.get("verification_uri").is_some()
                || body.get("login_url").is_some()
                || body.get("auth_url").is_some(),
            "Expected auth url field in response: {body:?}"
        );
    }

    std::fs::remove_dir_all(temp_dir).ok();
}

#[tokio::test]
async fn test_auth_files_crud_and_status() {
    let temp_dir = unique_temp_dir("p1-test-authstatus");
    std::fs::create_dir_all(&temp_dir).unwrap();
    let auth_file_path = temp_dir.join("test-acc.json");
    let initial_content = json!({
        "type": "claude",
        "email": "user@example.com",
        "disabled": false
    });
    std::fs::write(
        &auth_file_path,
        serde_json::to_string_pretty(&initial_content).unwrap(),
    )
    .unwrap();

    let app = setup_app(&temp_dir);

    // 1. GET /v0/management/auth-files
    let req = Request::builder()
        .method("GET")
        .uri("/v0/management/auth-files")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
        .body(Body::empty())
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    let list_body: Value = serde_json::from_slice(&bytes).expect("json");
    assert!(list_body["files"]
        .as_array()
        .expect("array")
        .iter()
        .any(|item| item["name"] == "test-acc.json"));

    // 2. PATCH /v0/management/auth-files/status (disable)
    let patch_payload = json!({
        "name": "test-acc.json",
        "disabled": true
    });
    let req = Request::builder()
        .method("PATCH")
        .uri("/v0/management/auth-files/status")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&patch_payload).unwrap()))
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::OK);

    let updated_content: Value =
        serde_json::from_str(&std::fs::read_to_string(&auth_file_path).unwrap()).unwrap();
    assert_eq!(updated_content["disabled"], true);

    std::fs::remove_dir_all(temp_dir).ok();
}

#[tokio::test]
async fn test_existing_live_and_realtime_calls_regression() {
    let temp_dir = unique_temp_dir("p1-test-regression");
    std::fs::create_dir_all(&temp_dir).unwrap();
    let app = setup_app(&temp_dir);

    // 1. /v1/realtime/client_secrets
    let req = Request::builder()
        .method("POST")
        .uri("/v1/realtime/client_secrets")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"session": {"model": "gpt-realtime"}}"#))
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    let secret_body: Value = serde_json::from_slice(&bytes).expect("json");
    assert!(
        secret_body.get("value").is_some()
            || secret_body.get("client_secret").is_some()
            || secret_body.get("key").is_some()
            || secret_body.get("secret").is_some()
    );

    // 2. /v1/realtime/sessions
    let req = Request::builder()
        .method("POST")
        .uri("/v1/realtime/sessions")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"model": "gpt-realtime"}"#))
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::OK);

    // 3. /v1/realtime/calls POST offer
    let req = Request::builder()
        .method("POST")
        .uri("/v1/realtime/calls")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            r#"{"sdp": "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n", "session": {}}"#,
        ))
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);

    // 4. /v1/live POST offer
    let req = Request::builder()
        .method("POST")
        .uri("/v1/live")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            r#"{"sdp": "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n", "session": {}}"#,
        ))
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);

    std::fs::remove_dir_all(temp_dir).ok();
}
