mod common;

use std::future::IntoFuture;
use std::sync::{Arc, Mutex};

use axum::{body::Bytes, extract::State, routing::post, Router};
use mahoquot_gateway::{config::GatewayConfig, routes::create_app, state::AppState};
use serde_json::{json, Value};
use tower::ServiceExt;

#[tokio::test]
async fn gemini_session_survives_history_changes_without_forwarding_ingress_headers() {
    // Given: an isolated provider endpoint and a real gateway route.
    let seen = Arc::new(Mutex::new(Vec::<(axum::http::HeaderMap, Value)>::new()));
    async fn capture(
        State(seen): State<Arc<Mutex<Vec<(axum::http::HeaderMap, Value)>>>>,
        headers: axum::http::HeaderMap,
        body: Bytes,
    ) -> impl axum::response::IntoResponse {
        seen.lock().unwrap().push((headers, serde_json::from_slice(&body).unwrap()));
        let response = json!({"response": {
            "candidates": [{"content": {"role": "model", "parts": [{"text": "ok"}]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 12, "candidatesTokenCount": 1, "cachedContentTokenCount": 0}
        }});
        ([("content-type", "text/event-stream")], format!("data: {response}\n\n"))
    }
    let mut listener = None;
    for port in 18880..=18899 {
        if let Ok(bound) = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            listener = Some(bound);
            break;
        }
    }
    let listener = listener.expect("an isolated test port in 18880-18899");
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(axum::serve(listener, Router::new().fallback(post(capture)).with_state(seen.clone())).into_future());
    let auth = common::unique_temp_dir("cache-session-wire");
    std::fs::write(auth.join("antigravity-fixture.json"), json!({
        "type": "antigravity", "identity_slug": "cache-fixture", "email": "cache@example.test",
        "access_token": "fixture", "refresh_token": "fixture", "project_id": "fixture-project",
        "expired": "2099-01-01T00:00:00Z", "upstream_override": upstream
    }).to_string()).unwrap();
    let config = GatewayConfig {
        auth_dir: auth.clone(), config_path: auth.join("config.yaml"), auth_refresh_enabled: false,
        api_keys: mahoquot_gateway::inbound::ApiKeys::from_env_value("fixture-key"),
        ..GatewayConfig::default()
    };
    let app = create_app(Arc::new(AppState::new(&config).unwrap()));

    // When: history changes in one session, and another session uses the same history.
    for (session, content) in [("session-a", "first"), ("session-a", "compacted"), ("session-b", "compacted")] {
        let response = tokio::time::timeout(std::time::Duration::from_secs(5), app.clone().oneshot(
            axum::http::Request::post("/v1/chat/completions")
                .header("authorization", "Bearer fixture-key")
                .header("content-type", "application/json")
                .header("x-session-id", session)
                .body(axum::body::Body::from(json!({
                    "model": "gemini-3.8-flash-high", "stream": false,
                    "messages": [{"role": "user", "content": content}]
                }).to_string())).unwrap()
        )).await.unwrap().unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1_000_000).await.unwrap();
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    }
    server.abort();
    let _ = server.await;
    std::fs::remove_dir_all(auth).unwrap();

    // Then: stable upstream identity is explicit; client routing headers stay local.
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    let ids: Vec<_> = seen.iter().map(|(_, body)| body["request"]["sessionId"].as_str().expect("upstream session id")).collect();
    assert_eq!(ids[0], ids[1]);
    assert_ne!(ids[1], ids[2]);
    assert!(ids.iter().all(|id| id.starts_with('-') && id[1..].parse::<u64>().is_ok()));
    assert!(seen.iter().all(|(headers, _)| !headers.contains_key("x-session-id")));
}

/// The signature an Antigravity upstream attaches to a tool call is the only
/// copy the gateway ever sees; the client replays the call by id, so a decoder
/// built without the session's replay scope silently turned every follow-up
/// turn into a signature-less request.
#[tokio::test]
async fn antigravity_replays_the_thought_signature_its_own_upstream_emitted() {
    const SIGNATURE: &str = "SIG-WIRE-1";

    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    async fn capture(
        State(seen): State<Arc<Mutex<Vec<Value>>>>,
        body: Bytes,
    ) -> impl axum::response::IntoResponse {
        seen.lock().unwrap().push(serde_json::from_slice(&body).unwrap());
        let response = json!({"response": {
            "candidates": [{"content": {"role": "model", "parts": [{
                "functionCall": {"id": "call_todo_1", "name": "todo", "args": {"x": 1}},
                "thoughtSignature": SIGNATURE
            }]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 1,
                              "cachedContentTokenCount": 0}
        }});
        ([("content-type", "text/event-stream")], format!("data: {response}\n\n"))
    }

    let mut listener = None;
    for port in 18880..=18899 {
        if let Ok(bound) = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            listener = Some(bound);
            break;
        }
    }
    let listener = listener.expect("an isolated test port in 18880-18899");
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(
        axum::serve(listener, Router::new().fallback(post(capture)).with_state(seen.clone()))
            .into_future(),
    );
    let auth = common::unique_temp_dir("cache-signature-wire");
    std::fs::write(auth.join("antigravity-fixture.json"), json!({
        "type": "antigravity", "identity_slug": "signature-fixture", "email": "sig@example.test",
        "access_token": "fixture", "refresh_token": "fixture", "project_id": "fixture-project",
        "expired": "2099-01-01T00:00:00Z", "upstream_override": upstream
    }).to_string()).unwrap();
    let config = GatewayConfig {
        auth_dir: auth.clone(), config_path: auth.join("config.yaml"), auth_refresh_enabled: false,
        api_keys: mahoquot_gateway::inbound::ApiKeys::from_env_value("fixture-key"),
        ..GatewayConfig::default()
    };
    let app = create_app(Arc::new(AppState::new(&config).unwrap()));

    let send = |messages: Value| {
        let app = app.clone();
        async move {
            let response = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                app.oneshot(
                    axum::http::Request::post("/v1/chat/completions")
                        .header("authorization", "Bearer fixture-key")
                        .header("content-type", "application/json")
                        .header("x-session-id", "session-a")
                        .body(axum::body::Body::from(json!({
                            "model": "gemini-3.8-flash-high",
                            "stream": true,
                            "messages": messages
                        }).to_string())).unwrap(),
                ),
            )
            .await
            .unwrap()
            .unwrap();
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), 1_000_000).await.unwrap();
            (status, String::from_utf8_lossy(&body).into_owned())
        }
    };

    // When: the first turn asks and the upstream answers with a signed call,
    // then the client replays that call under the id it was handed.
    let (status, first) = send(json!([{"role": "user", "content": "todo please"}])).await;
    assert_eq!(status, 200, "{first}");
    assert!(!first.contains(SIGNATURE), "the signature must not leak to the client: {first}");
    let (status, second) = send(json!([
        {"role": "user", "content": "todo please"},
        {"role": "assistant", "content": null, "tool_calls": [{
            "id": "call_todo_1", "type": "function",
            "function": {"name": "todo", "arguments": "{\"x\":1}"}
        }]},
        {"role": "tool", "tool_call_id": "call_todo_1", "content": "{\"ok\":true}"}
    ])).await;
    assert_eq!(status, 200, "{second}");
    server.abort();
    let _ = server.await;
    std::fs::remove_dir_all(auth).unwrap();

    // Then: the replayed call carries the signature the gateway captured.
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let parts = seen[1]["request"]["contents"][1]["parts"]
        .as_array()
        .expect("replayed assistant turn")
        .clone();
    let replayed = parts
        .iter()
        .find(|part| part.get("functionCall").is_some())
        .expect("replayed functionCall part");
    assert_eq!(replayed["functionCall"]["id"], "call_todo_1");
    assert_eq!(replayed["thoughtSignature"], SIGNATURE, "parts: {parts:?}");
}

/// A caller that sends no session header must still land on one stable replay
/// scope: the anchor falls back to the first user text (hashed, never sent),
/// which is what CLIProxyAPI and OpenCodex do.
#[tokio::test]
async fn a_caller_without_a_session_header_keeps_one_stable_replay_scope() {
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    async fn capture(
        State(seen): State<Arc<Mutex<Vec<Value>>>>,
        body: Bytes,
    ) -> impl axum::response::IntoResponse {
        seen.lock().unwrap().push(serde_json::from_slice(&body).unwrap());
        let response = json!({"response": {
            "candidates": [{"content": {"role": "model", "parts": [{"text": "ok"}]},
                            "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 1}
        }});
        ([( "content-type", "text/event-stream")], format!("data: {response}\n\n"))
    }

    let mut listener = None;
    for port in 18880..=18899 {
        if let Ok(bound) = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            listener = Some(bound);
            break;
        }
    }
    let listener = listener.expect("an isolated test port in 18880-18899");
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(
        axum::serve(listener, Router::new().fallback(post(capture)).with_state(seen.clone()))
            .into_future(),
    );
    let auth = common::unique_temp_dir("cache-anchor-wire");
    std::fs::write(auth.join("antigravity-fixture.json"), json!({
        "type": "antigravity", "identity_slug": "anchor-fixture", "email": "anchor@example.test",
        "access_token": "fixture", "refresh_token": "fixture", "project_id": "fixture-project",
        "expired": "2099-01-01T00:00:00Z", "upstream_override": upstream
    }).to_string()).unwrap();
    let config = GatewayConfig {
        auth_dir: auth.clone(), config_path: auth.join("config.yaml"), auth_refresh_enabled: false,
        api_keys: mahoquot_gateway::inbound::ApiKeys::from_env_value("fixture-key"),
        ..GatewayConfig::default()
    };
    let app = create_app(Arc::new(AppState::new(&config).unwrap()));

    for follow_up in ["first", "second"] {
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            app.clone().oneshot(
                axum::http::Request::post("/v1/chat/completions")
                    .header("authorization", "Bearer fixture-key")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(json!({
                        "model": "gemini-3.8-flash-high",
                        "stream": false,
                        "messages": [
                            {"role": "user", "content": "anchor this conversation"},
                            {"role": "assistant", "content": follow_up}
                        ]
                    }).to_string())).unwrap(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), 200);
        axum::body::to_bytes(response.into_body(), 1_000_000).await.unwrap();
    }
    server.abort();
    let _ = server.await;
    std::fs::remove_dir_all(auth).unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let ids: Vec<_> = seen
        .iter()
        .map(|body| body["request"]["sessionId"].as_str().expect("anchored session id"))
        .collect();
    assert_eq!(ids[0], ids[1], "the anchor must not move between turns");
    // The anchor text itself is never sent: the id is `-` + a numeric digest.
    assert!(
        ids.iter().all(|id| id.starts_with('-') && id[1..].parse::<u64>().is_ok()),
        "session ids must be digests: {ids:?}"
    );
}
