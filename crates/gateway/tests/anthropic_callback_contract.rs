mod common;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::management::oauth::oauth_routes;
use mahoquot_gateway::state::AppState;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

#[tokio::test]
async fn invalid_callback_stays_open_until_a_successful_exchange() {
    let mut listener = None;
    for port in 18840..=18899 {
        if let Ok(bound) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            listener = Some(bound);
            break;
        }
    }
    let listener = listener.expect("available local test port");
    let token_url = format!("http://{}/token", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let mock = Router::new().route("/token", post(move |Json(body): Json<Value>| {
        seen.fetch_add(1, Ordering::SeqCst);
        async move {
            if body["code"] == "valid-code" {
                (StatusCode::OK, Json(json!({"access_token":"local-access","refresh_token":"local-refresh","expires_in":3600})))
            } else {
                (StatusCode::BAD_REQUEST, Json(json!({"error":"invalid_grant"})))
            }
        }
    }));
    let mock_task = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let auth_dir = common::unique_temp_dir("anthropic-callback-contract");
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
    let start = app
        .clone()
        .oneshot(
            Request::get(format!("/anthropic-auth-url?token_url={token_url}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(start.status(), StatusCode::OK);
    let start: Value =
        serde_json::from_slice(&to_bytes(start.into_body(), usize::MAX).await.unwrap()).unwrap();
    let session = start["state"].as_str().unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let invalid_state = client
        .get("http://127.0.0.1:54545/callback")
        .query(&[("state", "wrong-state"), ("code", "invalid-code")])
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_state.status(), StatusCode::BAD_REQUEST);
    invalid_state.bytes().await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    let invalid_code = client
        .get("http://127.0.0.1:54545/callback")
        .query(&[("state", session), ("code", "invalid-code")])
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_code.status(), StatusCode::BAD_REQUEST);
    invalid_code.bytes().await.unwrap();
    let success = client
        .get("http://127.0.0.1:54545/callback")
        .query(&[("state", session), ("code", "valid-code")])
        .send()
        .await
        .unwrap();
    assert_eq!(success.status(), StatusCode::OK);
    success.bytes().await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    let status = app
        .oneshot(
            Request::get(format!("/get-auth-status?state={session}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status: Value =
        serde_json::from_slice(&to_bytes(status.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(status["status"], "ok");
    mock_task.abort();
    let _ = mock_task.await;
    std::fs::remove_dir_all(auth_dir).unwrap();
}
