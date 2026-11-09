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

async fn command_code_reply(
    State(seen): State<Arc<Mutex<Captured>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> ([(&'static str, &'static str); 1], String) {
    seen.lock().unwrap().requests.push((
        method,
        uri,
        headers,
        serde_json::from_slice(&body).unwrap(),
    ));
    let records = [
        json!({"type":"text-delta","text":"Hello from Command Code"}),
        json!({"type":"tool-call","toolCallId":"call-fixture","toolName":"read_file","input":{"path":"README.md"}}),
        json!({"type":"finish","finishReason":"tool-calls","totalUsage":{"inputTokens":7,"outputTokens":4,"totalTokens":11}}),
    ];
    (
        [("content-type", "application/x-ndjson")],
        records
            .into_iter()
            .map(|record| format!("{record}\n"))
            .collect(),
    )
}

async fn exercise_command_code_wire(stream: bool, use_override: bool) {
    let seen = Arc::new(Mutex::new(Captured::default()));
    let mut listener = None;
    for port in 18840..=18899 {
        if let Ok(bound) =
            tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await
        {
            listener = Some(bound);
            break;
        }
    }
    let listener = listener.expect("isolated Command Code mock port");
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn({
        let seen = seen.clone();
        async move {
            axum::serve(
                listener,
                Router::new()
                    .fallback(post(command_code_reply))
                    .with_state(seen),
            )
            .await
            .unwrap();
        }
    });
    let auth = common::unique_temp_dir("command-code-wire");
    let mut credential = json!({
        "type":"generic", "provider":"command-code", "adapter":"command-code",
        "base_url":upstream, "api_key":"command-code-oauth-token", "auth_mode":"oauth",
        "models":["command-code/fixture"], "disabled":false
    });
    if use_override {
        credential["upstream_override"] = credential["base_url"].clone();
        credential["base_url"] = json!("http://127.0.0.1:9");
    }
    std::fs::write(
        auth.join("generic-command-code-fixture.json"),
        credential.to_string(),
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
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let response = app
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header("authorization", "Bearer fixture-key")
                    .header("content-type", "application/json")
                    .header("x-session-id", "command-code-wire-session")
                    .body(Body::from(json!({
                        "model":"command-code/fixture", "stream":stream,
                        "messages":[{"role":"user","content":"Inspect the README"}],
                        "tools":[{"type":"function","function":{"name":"read_file","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}}]
                    }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let raw = axum::body::to_bytes(response.into_body(), 1_000_000).await.unwrap();
        if stream {
            let text = std::str::from_utf8(&raw).unwrap();
            let chunks: Vec<Value> = text.lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter(|data| *data != "[DONE]")
                .map(|data| serde_json::from_str(data).unwrap())
                .collect();
            assert!(chunks.iter().any(|chunk| chunk["choices"][0]["delta"]["content"] == "Hello from Command Code"));
            assert!(chunks.iter().any(|chunk| chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["name"] == "read_file"));
            let arguments: String = chunks.iter().filter_map(|chunk| chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str()).collect();
            assert_eq!(serde_json::from_str::<Value>(&arguments).unwrap(), json!({"path":"README.md"}));
            assert!(text.ends_with("data: [DONE]\n\n"));
        } else {
            let output: Value = serde_json::from_slice(&raw).unwrap();
            assert_eq!(output["object"], "chat.completion");
            let choice = &output["choices"][0];
            assert_eq!(choice["message"]["content"], "Hello from Command Code");
            assert_eq!(choice["message"]["tool_calls"][0]["id"], "call-fixture");
            assert_eq!(choice["message"]["tool_calls"][0]["function"]["name"], "read_file");
            let arguments = choice["message"]["tool_calls"][0]["function"]["arguments"].as_str().unwrap();
            assert_eq!(serde_json::from_str::<Value>(arguments).unwrap(), json!({"path":"README.md"}));
            assert_eq!(choice["finish_reason"], "tool_calls");
        }
    }).await.expect("Command Code relay completes");
    server.abort();
    let _ = server.await;
    std::fs::remove_dir_all(auth).unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.requests.len(), 1);
    let (method, uri, headers, body) = &seen.requests[0];
    assert_eq!(method, Method::POST);
    assert_eq!(uri.path(), "/alpha/generate");
    assert_eq!(
        body["params"]["messages"][0]["content"][0]["text"],
        "Inspect the README"
    );
    assert_eq!(body["params"]["stream"], true);
    assert_eq!(body["params"]["model"], "fixture");
    assert_eq!(body["mode"], "agent");
    assert_eq!(body["permissionMode"], "standard");
    for (name, value) in [
        ("authorization", "Bearer command-code-oauth-token"),
        ("content-type", "application/json"),
        ("user-agent", "cli"),
        ("x-command-code-version", "0.52.1"),
        ("x-cli-environment", "production"),
        ("x-taste-learning", "false"),
        ("x-co-flag", "false"),
    ] {
        assert_eq!(headers.get(name).unwrap(), value);
    }
    assert!(!headers.get("x-session-id").unwrap().is_empty());
}

#[tokio::test]
async fn command_code_oauth_chat_uses_cli_wire_and_decodes_ndjson() {
    exercise_command_code_wire(false, false).await;
}

#[tokio::test]
async fn command_code_oauth_stream_uses_upstream_override_and_decodes_ndjson() {
    exercise_command_code_wire(true, true).await;
}
