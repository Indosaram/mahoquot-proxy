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
use mahoquot_types::{Health, PoolMember, Strategy};
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn spent_streaming_accounts_are_skipped_until_a_serving_account_is_found() {
    // Given: four accounts returning HTTP 200 + SSE rate-limit errors before
    // one serving account, with a failover budget smaller than the pool.
    let mut listener = None;
    for port in 18840..=18899 {
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(bound) => {
                listener = Some(bound);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => panic!("fixture bind failed: {error}"),
        }
    }
    let listener = listener.expect("available fixture port");
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let fixture = Router::new().route(
        "/v1/messages",
        post(move || {
            let observed = Arc::clone(&observed);
            async move {
                let attempt = observed.fetch_add(1, Ordering::SeqCst);
                let start = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"fixture\",\"role\":\"assistant\",\"content\":[]}}\n\n";
                let result = if attempt < 4 {
                    "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"account spent\"}}\n\n"
                } else {
                    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"served-by-peer\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
                };
                ([("content-type", "text/event-stream")], format!("{start}{result}"))
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });
    let dir = common::unique_temp_dir("anthropic-stream-failover");
    for index in 0..5 {
        std::fs::write(
            dir.join(format!("claude-{index}.json")),
            json!({
                "type": "claude", "identity_slug": format!("account-{index}"),
                "email": format!("account-{index}@example.test"),
                "access_token": "fixture", "upstream_override": upstream
            })
            .to_string(),
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
            max_failover: 2,
            ..GatewayConfig::default()
        })
        .unwrap(),
    );
    let app = create_app(Arc::clone(&state));

    let mut finalizers: Vec<_> = state
        .pool
        .load()
        .members
        .iter()
        .map(|member| (member.id.clone(), state.subscribe_finalizer(member.id())))
        .collect();

    // When: the client makes one streaming request through the real route.
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        app.oneshot(
            Request::post("/v1/chat/completions")
                .header("authorization", "Bearer fixture")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":"claude-opus-5-5", "stream":true,
                "messages":[{"role":"user", "content":"hello"}]})
                    .to_string(),
                ))
                .unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        response.into_body().collect(),
    )
    .await
    .unwrap()
    .unwrap()
    .to_bytes();
    let text = String::from_utf8(body.to_vec()).unwrap();

    // Then: only the serving account's content reaches the client; spent
    // accounts are temporarily cooled down, never permanently disabled.
    assert!(text.contains("served-by-peer"), "{text}");
    assert!(!text.contains("account spent"), "{text}");
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    assert_eq!(
        state
            .pool
            .load()
            .members
            .iter()
            .filter(|m| matches!(m.health(), Health::Cooldown { .. }))
            .count(),
        4
    );
    server.abort();
    server.await.unwrap_err();
    let serving = state
        .pool
        .load()
        .members
        .iter()
        .find(|member| !matches!(member.health(), Health::Cooldown { .. }))
        .unwrap()
        .id
        .clone();
    let (_, finalizer) = finalizers
        .iter_mut()
        .find(|(id, _)| id == &serving)
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), finalizer.recv())
        .await
        .unwrap()
        .expect("request finalized before removing auth directory");
    drop(state);
    std::fs::remove_dir_all(dir).unwrap();
}
