//! Regression: an upstream served gpt-5.6-luna for a gpt-6-astra request and the
//! relay still labelled every frame as astra, hiding the substitution.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::Router;
use http_body_util::BodyExt;
use mahoquot_gateway::{
    config::GatewayConfig, inbound::ApiKeys, routes::create_app, state::AppState,
};
use mahoquot_types::Strategy;
use serde_json::json;
use tower::ServiceExt;

const SUBSTITUTED_CODEX_TURN: &str = concat!(
    "event: response.created\n",
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_sub\",\"status\":\"in_progress\",\"model\":\"gpt-5.6-luna\"}}\n\n",
    "event: response.output_text.delta\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\",\"output_index\":0,\"content_index\":0}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_sub\",\"status\":\"completed\",\"model\":\"gpt-5.6-luna\",\"output\":[]}}\n\n",
);

async fn bind_fixture_listener() -> tokio::net::TcpListener {
    for port in 18840..=18899 {
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(bound) => return bound,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => panic!("fixture bind failed: {error}"),
        }
    }
    panic!("no free fixture port in 18840-18899");
}

#[tokio::test]
async fn a_substituted_upstream_model_is_visible_to_the_client() {
    let listener = bind_fixture_listener().await;
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let fixture = Router::new().route(
        common::CODEX_PATH,
        post(move || {
            let observed = Arc::clone(&observed);
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                (
                    [("content-type", "text/event-stream")],
                    SUBSTITUTED_CODEX_TURN,
                )
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });

    let dir = common::unique_temp_dir("codex-model-substitution");
    let mut credential: serde_json::Value = serde_json::from_str(&common::create_auth_file_json(
        "acct",
        "acc_acct",
        "fixture",
        Some(&upstream),
    ))
    .unwrap();
    credential["upstream_override"] = json!(upstream);
    std::fs::write(dir.join("codex-acct.json"), credential.to_string()).unwrap();

    let state = Arc::new(
        AppState::new(&GatewayConfig {
            auth_dir: dir.clone(),
            config_path: dir.join("config.yaml"),
            api_keys: ApiKeys::new(vec!["fixture".to_string()]),
            auth_refresh_enabled: false,
            strategy: Strategy::FillFirst,
            max_failover: 3,
            ..GatewayConfig::default()
        })
        .unwrap(),
    );
    let app = create_app(Arc::clone(&state));

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        app.oneshot(
            Request::post("/v1/chat/completions")
                .header("authorization", "Bearer fixture")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-6-astra",
                        "stream": true,
                        "messages": [{"role": "user", "content": "hi"}],
                        "session_id": "substitution-session",
                    })
                    .to_string(),
                ))
                .unwrap(),
        ),
    )
    .await
    .expect("the relay must answer")
    .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        response.into_body().collect(),
    )
    .await
    .expect("the served stream must terminate")
    .unwrap()
    .to_bytes();
    let text = String::from_utf8(body.to_vec()).unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 1, "one upstream call: {text}");
    assert!(
        text.contains("gpt-5.6-luna"),
        "upstream served gpt-5.6-luna but the client never sees it: {text}"
    );

    server.abort();
    drop(state);
    std::fs::remove_dir_all(dir).unwrap();
}
