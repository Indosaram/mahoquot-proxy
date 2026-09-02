mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use common::unique_temp_dir;
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use mahoquot_types::Strategy;
use tower::ServiceExt;

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    use http_body_util::BodyExt;
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("json body")
}

fn gateway_config(auth_dir: &std::path::Path) -> GatewayConfig {
    GatewayConfig {
        port: 0,
        auth_dir: auth_dir.to_path_buf(),
        strategy: Strategy::StrictRoundRobin,
        max_failover: 3,
        log_level: "info".to_string(),
        api_keys: mahoquot_gateway::inbound::ApiKeys::default(),
        models_env: None,
        refresh_url: mahoquot_providers::refresh::REFRESH_TOKEN_URL.to_string(),
        auth_refresh_enabled: false,
        usage_poll_secs: 120,
        config_path: auth_dir.join("config.yaml"),
        history_queue_capacity: 1024,
        history_batch_size: 64,
    }
}

async fn spawn_mock_upstream() -> (SocketAddr, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let mock_app = Router::new()
        .route("/models", get(models_handler))
        .with_state(hits.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, mock_app).await.unwrap() });
    (addr, hits)
}

async fn models_handler(State(hits): State<Arc<AtomicUsize>>, headers: HeaderMap) -> Response {
    hits.fetch_add(1, Ordering::SeqCst);
    let authorized = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == "Bearer good-key");
    if authorized {
        Json(serde_json::json!({"data": [{"id": "deepseek/deepseek-v4-flash"}]})).into_response()
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

async fn import_command_code(app: &Router, api_key: &str) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v0/management/command-code/import")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"api_key": api_key}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (status, body_json(response).await)
}

fn credential_file_count(auth_dir: &std::path::Path) -> usize {
    std::fs::read_dir(auth_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains("command-code"))
        .count()
}

// Plan D9 (AUTH-7): a garbage key must never become a stored account; the
// import verifies the credential against the upstream once and refuses to
// persist on failure.
#[tokio::test]
async fn import_verifies_credentials_before_persisting() {
    let auth_dir = unique_temp_dir("mahoquot-command-code-validated");
    let (addr, hits) = spawn_mock_upstream().await;
    std::env::set_var("MAHOQUOT_COMMAND_CODE_BASE_URL", format!("http://{addr}"));
    let app = create_app(Arc::new(
        AppState::new(&gateway_config(&auth_dir)).expect("state"),
    ));

    let (status, body) = import_command_code(&app, "garbage-x").await;
    assert!(
        status.is_client_error(),
        "garbage key must be rejected, got {status}: {body}"
    );
    assert_eq!(
        credential_file_count(&auth_dir),
        0,
        "a refused credential must not be persisted"
    );

    let (status, body) = import_command_code(&app, "good-key").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(credential_file_count(&auth_dir), 1);
    assert!(hits.load(Ordering::SeqCst) >= 2);
    std::env::remove_var("MAHOQUOT_COMMAND_CODE_BASE_URL");
}
