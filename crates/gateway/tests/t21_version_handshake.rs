use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use mahoquot_gateway::{config::GatewayConfig, routes::create_app, state::AppState};
use mahoquot_types::Strategy;
use tower::ServiceExt;

mod common;

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
    }
}

// Plan D8 (SEAM-5): the app detects a mismatched gateway pair via these
// fields; dropping either one silently breaks the handshake.
#[tokio::test]
async fn healthz_reports_build_version_and_api_schema() {
    let auth_dir = common::unique_temp_dir("mahoquot-version-handshake");
    let app = create_app(Arc::new(
        AppState::new(&gateway_config(&auth_dir)).expect("state"),
    ));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let payload: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");

    assert_eq!(payload["status"], "ok");
    assert_eq!(
        payload["version"],
        env!("CARGO_PKG_VERSION"),
        "build version must be exposed for the app's handshake check"
    );
    assert_eq!(
        payload["api_schema"], 1,
        "management api schema version must be exposed for the app's handshake check"
    );
}
