use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::inbound::ApiKeys;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use serde_json::json;
use tower::ServiceExt;

const TEST_API_KEY: &str = "test-large-body-key";

struct TestDir(PathBuf);

impl Drop for TestDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn test_app() -> (axum::Router, TestDir) {
    let auth_dir = std::env::temp_dir().join(format!(
        "mahoquot-large-body-{}-{}",
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
    let state = Arc::new(AppState::new(&config).expect("app state"));
    (create_app(state), TestDir(auth_dir))
}

async fn post_chat_completion(body_bytes: Vec<u8>) -> StatusCode {
    let (app, _dir) = test_app();
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_API_KEY}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body_bytes))
        .expect("request build");
    app.oneshot(request)
        .await
        .expect("service oneshot")
        .status()
}

fn chat_body(filler_bytes: usize) -> Vec<u8> {
    let body = json!({
        "model": "gemini-3.8-flash-high",
        "messages": [{ "role": "user", "content": "x".repeat(filler_bytes) }]
    });
    serde_json::to_vec(&body).expect("serialize body")
}

#[tokio::test]
async fn a_multi_megabyte_conversation_is_not_rejected_by_the_body_limit() {
    let status = post_chat_completion(chat_body(32 * 1024 * 1024)).await;
    assert_ne!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn a_small_conversation_reaches_the_same_handler() {
    let small = post_chat_completion(chat_body(16)).await;
    let large = post_chat_completion(chat_body(32 * 1024 * 1024)).await;
    assert_eq!(large, small);
}
