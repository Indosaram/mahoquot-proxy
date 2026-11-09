//! A Responses client against a generic chat-completions upstream.
//!
//! The request is translated to `messages` before it leaves the gateway, so
//! the reply is chat-shaped. Without a chat decoder the Responses renderer sees
//! no recognisable events and emits an empty answer.

mod common;

use std::sync::{Arc, Mutex};

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, Method, Request, Uri},
    routing::post,
    Router,
};
use mahoquot_gateway::{config::GatewayConfig, routes::create_app, state::AppState};
use serde_json::{json, Value};
use tower::ServiceExt;

#[derive(Default)]
struct Captured {
    requests: Vec<(Method, Uri, HeaderMap, Value)>,
}

async fn chat_reply(
    State(seen): State<Arc<Mutex<Captured>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> ([(&'static str, &'static str); 1], String) {
    let request: Value = serde_json::from_slice(&body).unwrap();
    let stream = request["stream"].as_bool().unwrap_or(false);
    seen.lock().unwrap().requests.push((method, uri, headers, request));
    if stream {
        let frames = [
            json!({"id":"chatcmpl-fixture","choices":[{"delta":{"role":"assistant"}}]}),
            json!({"id":"chatcmpl-fixture","choices":[{"delta":{"content":"Hello "}}]}),
            json!({"id":"chatcmpl-fixture","choices":[{"delta":{"content":"from chat"}}]}),
            json!({"id":"chatcmpl-fixture","choices":[{"delta":{},"finish_reason":"stop"}],
                   "usage":{"prompt_tokens":9,"completion_tokens":3,"total_tokens":12}}),
        ];
        let mut text = String::new();
        for frame in frames {
            text.push_str(&format!("data: {frame}\n\n"));
        }
        text.push_str("data: [DONE]\n\n");
        ([("content-type", "text/event-stream")], text)
    } else {
        (
            [("content-type", "application/json")],
            json!({
                "id": "chatcmpl-fixture",
                "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "Hello from chat"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 9, "completion_tokens": 3, "total_tokens": 12}
            })
            .to_string(),
        )
    }
}

async fn exercise(stream: bool) -> (Value, Value, String) {
    let seen = Arc::new(Mutex::new(Captured::default()));
    let mut listener = None;
    for port in 18840..=18899 {
        if let Ok(bound) = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            listener = Some(bound);
            break;
        }
    }
    let listener = listener.expect("isolated generic chat mock port");
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn({
        let seen = seen.clone();
        async move {
            axum::serve(
                listener,
                Router::new().fallback(post(chat_reply)).with_state(seen),
            )
            .await
            .unwrap();
        }
    });
    let auth = common::unique_temp_dir("generic-chat-responses");
    std::fs::write(
        auth.join("generic-chat-fixture.json"),
        json!({
            "type": "generic", "provider": "cline", "adapter": "openai-chat",
            "base_url": upstream, "api_key": "fixture-chat",
            "models": ["z-ai/glm-5.3-flash"], "disabled": false
        })
        .to_string(),
    )
    .unwrap();
    let config = GatewayConfig {
        auth_dir: auth.clone(),
        config_path: auth.join("config.yaml"),
        auth_refresh_enabled: false,
        api_keys: mahoquot_gateway::inbound::ApiKeys::from_env_value("fixture-key"),
        ..GatewayConfig::default()
    };
    let app = create_app(Arc::new(AppState::new(&config).unwrap()));
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        app.oneshot(
            Request::post("/v1/responses")
                .header("authorization", "Bearer fixture-key")
                .header("content-type", "application/json")
                .header("x-session-id", "generic-chat-session")
                .body(Body::from(
                    json!({
                        "model": "z-ai/glm-5.3-flash",
                        "stream": stream,
                        "input": "Say hello"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
    })
    .await
    .expect("generic chat relay completes");
    assert_eq!(response.status(), 200);
    let raw = axum::body::to_bytes(response.into_body(), 2_000_000).await.unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    server.abort();
    let _ = server.await;
    let request = seen.lock().unwrap().requests.clone();
    assert_eq!(request.len(), 1, "exactly one upstream call: {text}");
    std::fs::remove_dir_all(auth).unwrap();
    (request[0].3.clone(), Value::Null, text)
}

#[tokio::test]
async fn responses_client_receives_chat_stream_as_responses_events() {
    let (upstream_request, _, text) = exercise(true).await;
    // The translated body must stay a stream: a buffered chat reply cannot be
    // replayed as Responses events.
    assert_eq!(upstream_request["stream"], true, "{upstream_request}");
    assert!(upstream_request["messages"].is_array(), "{upstream_request}");
    assert!(text.contains("response.output_text.delta"), "{text}");
    assert!(text.contains("Hello "), "{text}");
    assert!(text.contains("from chat"), "{text}");
    // Responses SSE has no [DONE] sentinel; the terminal event is the
    // completed response, which the renderer must emit exactly once.
    assert_eq!(
        text.matches("event: response.completed").count(),
        1,
        "the stream terminates once: {text}"
    );
}

#[tokio::test]
async fn responses_client_receives_a_buffered_chat_completion_as_a_response_object() {
    let (upstream_request, _, text) = exercise(false).await;
    assert_eq!(upstream_request["stream"], false, "{upstream_request}");
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["object"], "response");
    assert_eq!(value["status"], "completed");
    assert_eq!(
        value["output"][0]["content"][0]["text"], "Hello from chat",
        "{value}"
    );
    assert_eq!(value["usage"]["input_tokens"], 9);
    assert_eq!(value["usage"]["output_tokens"], 3);
}
