//! A Codex upstream can answer HTTP 200 with a well-formed stream that carries a
//! role prelude, reasoning, and a terminal event but no text and no tool call.
//! That turn is empty by construction: relaying it verbatim makes the client
//! consume a dead response and retry, and the retry usually lands on the same
//! account. The relay must spend a failover attempt instead.

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

/// The observed upstream shape: creation, reasoning, completion, no content.
const EMPTY_CODEX_TURN: &str = concat!(
    "event: response.created\n",
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_empty\",\"status\":\"in_progress\"}}\n\n",
    "event: response.reasoning_summary_text.delta\n",
    "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"considering the question\"}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_empty\",\"status\":\"completed\",\"output\":[]}}\n\n",
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
async fn an_empty_codex_turn_fails_over_instead_of_reaching_the_client() {
    // Given: account A answers 200 with an empty turn, account B serves text.
    let listener_a = bind_fixture_listener().await;
    let upstream_a = format!("http://{}", listener_a.local_addr().unwrap());
    let calls_a = Arc::new(AtomicUsize::new(0));
    let observed_a = Arc::clone(&calls_a);
    let fixture_a = Router::new().route(
        common::CODEX_PATH,
        post(move || {
            let observed = Arc::clone(&observed_a);
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                ([("content-type", "text/event-stream")], EMPTY_CODEX_TURN)
            }
        }),
    );
    let server_a = tokio::spawn(async move { axum::serve(listener_a, fixture_a).await.unwrap() });

    let listener_b = bind_fixture_listener().await;
    let upstream_b = format!("http://{}", listener_b.local_addr().unwrap());
    let calls_b = Arc::new(AtomicUsize::new(0));
    let observed_b = Arc::clone(&calls_b);
    let fixture_b = Router::new().route(
        common::CODEX_PATH,
        post(move || {
            let observed = Arc::clone(&observed_b);
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                (
                    [("content-type", "text/event-stream")],
                    common::codex_sse("served-by-peer"),
                )
            }
        }),
    );
    let server_b = tokio::spawn(async move { axum::serve(listener_b, fixture_b).await.unwrap() });

    let dir = common::unique_temp_dir("codex-empty-stream-failover");
    for (name, upstream) in [("a", &upstream_a), ("b", &upstream_b)] {
        let mut credential: serde_json::Value = serde_json::from_str(
            &common::create_auth_file_json(name, &format!("acc_{name}"), "fixture", Some(upstream)),
        )
        .unwrap();
        credential["upstream_override"] = json!(upstream);
        std::fs::write(
            dir.join(format!("codex-{name}.json")),
            credential.to_string(),
        )
        .unwrap();
    }

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

    // When: the client streams one chat completion.
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        app.oneshot(
            Request::post("/v1/chat/completions")
                .header("authorization", "Bearer fixture")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "codex",
                        "stream": true,
                        "messages": [{"role": "user", "content": "hello"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        ),
    )
    .await
    .expect("the gateway must not hold the empty turn open")
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

    // Then: the empty turn never reaches the client and the next account serves.
    assert!(text.contains("served-by-peer"), "client saw: {text}");
    assert_eq!(calls_a.load(Ordering::SeqCst), 1, "empty account tried once");
    assert_eq!(calls_b.load(Ordering::SeqCst), 1, "peer account served");
    assert!(
        state.get_stats().failed_over >= 1,
        "an empty turn must be recorded as a failover"
    );

    server_a.abort();
    server_b.abort();
    drop(state);
    std::fs::remove_dir_all(dir).unwrap();
}
