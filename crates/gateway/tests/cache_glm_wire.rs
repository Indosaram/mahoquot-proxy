mod common;

use std::sync::{Arc, Mutex};

use axum::{body::Bytes, extract::State, routing::post, Router};
use mahoquot_gateway::{config::GatewayConfig, routes::create_app, state::AppState};
use serde_json::{json, Value};
use tower::ServiceExt;

#[tokio::test]
async fn glm_preserves_usage_presence_and_does_not_inject_cache_routing_fields() {
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    async fn capture(State(seen): State<Arc<Mutex<Vec<Value>>>>, body: Bytes) -> axum::Json<Value> {
        let mut requests = seen.lock().unwrap();
        requests.push(serde_json::from_slice(&body).unwrap());
        let mut usage = json!({"prompt_tokens": 100, "completion_tokens": 1});
        match requests.len() {
            2 => usage["prompt_tokens_details"] = json!({"cached_tokens": 0}),
            3 => usage["prompt_tokens_details"] = json!({"cached_tokens": 80}),
            _ => {}
        }
        axum::Json(json!({"id": "fixture", "object": "chat.completion", "choices": [], "usage": usage}))
    }
    let mut listener = None;
    for port in 18860..=18879 {
        if let Ok(bound) = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            listener = Some(bound);
            break;
        }
    }
    let listener = listener.expect("isolated mock port");
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn({
        let seen = seen.clone();
        async move { axum::serve(listener, Router::new().fallback(post(capture)).with_state(seen)).await.unwrap() }
    });
    let auth = common::unique_temp_dir("cache-glm-wire");
    std::fs::write(auth.join("generic-cline-fixture.json"), json!({
        "type": "generic", "provider": "cline", "adapter": "openai-chat",
        "base_url": upstream, "api_key": "fixture", "models": ["z-ai/glm-5.3-flash"]
    }).to_string()).unwrap();
    let config = GatewayConfig {
        auth_dir: auth.clone(), config_path: auth.join("config.yaml"), auth_refresh_enabled: false,
        api_keys: mahoquot_gateway::inbound::ApiKeys::from_env_value("fixture-key"),
        ..GatewayConfig::default()
    };
    let app = create_app(Arc::new(AppState::new(&config).unwrap()));
    let request_body = json!({"model": "z-ai/glm-5.3-flash", "stream": false,
        "messages": [{"role": "system", "content": "fixed"}, {"role": "user", "content": "fixed"}],
        "tools": [{"type": "function", "function": {"name": "fixture", "parameters": {"type": "object"}}}]
    });
    for expected in [None, Some(0), Some(80)] {
        let response = tokio::time::timeout(std::time::Duration::from_secs(5), app.clone().oneshot(
            axum::http::Request::post("/v1/chat/completions")
                .header("authorization", "Bearer fixture-key").header("content-type", "application/json")
                .header("x-session-id", "stable-fixture")
                .body(axum::body::Body::from(request_body.to_string())).unwrap()
        )).await.unwrap().unwrap();
        assert_eq!(response.status(), 200);
        let raw = axum::body::to_bytes(response.into_body(), 1_000_000).await.unwrap();
        let output: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(output.pointer("/usage/prompt_tokens_details/cached_tokens").and_then(Value::as_u64), expected);
        let usage = mahoquot_gateway::usage::extract_response_token_usage(&raw, &raw).unwrap();
        assert_eq!(usage.cached_input_tokens_known, expected.is_some());
        assert_eq!(usage.cached_input_tokens, expected.unwrap_or(0));
    }
    server.abort();
    let _ = server.await;
    std::fs::remove_dir_all(auth).unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert!(seen.iter().all(|body| body == &request_body));
}
