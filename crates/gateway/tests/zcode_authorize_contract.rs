mod common;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use mahoquot_gateway::{config::GatewayConfig, management::oauth::oauth_routes, state::AppState};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

#[tokio::test]
async fn authorize_page_paste_keeps_the_login_pending() {
    let auth_dir = common::unique_temp_dir("zcode-authorize-contract");
    let state = Arc::new(
        AppState::new(&GatewayConfig {
            auth_dir: auth_dir.clone(),
            config_path: auth_dir.join("config.yaml"),
            auth_refresh_enabled: false,
            ..GatewayConfig::default()
        })
        .unwrap(),
    );
    let app = oauth_routes().with_state(state);
    let started = app.clone().oneshot(Request::get("/zcode-auth-url?broker_url=http://127.0.0.1:18899/unexpected&api_base=http://127.0.0.1:18899").body(Body::empty()).unwrap()).await.unwrap();
    let started: Value =
        serde_json::from_slice(&to_bytes(started.into_body(), usize::MAX).await.unwrap()).unwrap();
    let input = started["url"]
        .as_str()
        .unwrap()
        .replace("/api/oauth/authorize", "/auth/oauth/authorize");
    let response = app
        .clone()
        .oneshot(
            Request::post("/zcode-callback")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"state":started["state"],"callback_url":input}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(response["status"], "pending");
    assert_eq!(response["url"], started["url"]);
    let status = app
        .oneshot(
            Request::get(format!(
                "/get-auth-status?state={}",
                started["state"].as_str().unwrap()
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    let status: Value =
        serde_json::from_slice(&to_bytes(status.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(status["status"], "pending");
    std::fs::remove_dir_all(auth_dir).unwrap();
}
