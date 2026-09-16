//! Relay-level regression for the 2026-09-16 production failure
//! (`tools.0.custom.input_schema: ... must match draft 2020-12`): the strict
//! Vertex-hosted Anthropic validator rejects anyOf/oneOf, so the dispatched
//! OmO-shaped toolset must arrive union-free. Upstream is a local mock only.

mod common;

use std::{path::PathBuf, sync::Arc, time::Duration};

use axum::{body::Body, http::StatusCode, response::Response, routing::post, Json, Router};
use mahoquot_gateway::{config::GatewayConfig, routes::create_app, state::AppState};
use serde_json::{json, Value};

const MODEL: &str = "claude-opus-4-6";

fn omo_request() -> Value {
    json!({
        "model": MODEL,
        "max_tokens": 1,
        "stream": false,
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "read",
                    "description": "Read the contents of a file.",
                    "parameters": {
                        "additionalProperties": false,
                        "properties": {
                            "limit": {
                                "anyOf": [{"type": "number"}, {"type": "null"}],
                                "default": null,
                                "description": "Maximum number of lines to read"
                            },
                            "offset": {
                                "anyOf": [{"type": "number"}, {"type": "null"}],
                                "default": 1,
                                "description": "Line number to start reading from (1-indexed)"
                            },
                            "path": {
                                "description": "Path to the file to read (relative or absolute)",
                                "type": "string"
                            }
                        },
                        "required": ["path", "offset", "limit"],
                        "type": "object"
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "task",
                    "description": "Spawn one child task or fan out a batch.",
                    "parameters": {
                        "$schema": "http://json-schema.org/draft-07/schema#",
                        "additionalProperties": false,
                        "properties": {
                            "category": {"description": "Category name", "type": "string"},
                            "prompt": {"description": "The instruction", "type": "string"},
                            "run_in_background": {
                                "default": false,
                                "description": "true returns the task id at once",
                                "type": "boolean"
                            },
                            "task_summary": {"maxLength": 80, "type": "string"}
                        },
                        "required": [],
                        "type": "object"
                    }
                }
            }
        ]
    })
}

fn contains_union(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.contains_key("anyOf")
                || map.contains_key("oneOf")
                || map.values().any(contains_union)
        }
        Value::Array(items) => items.iter().any(contains_union),
        _ => false,
    }
}

#[tokio::test]
async fn omo_toolset_dispatches_union_free_schemas_to_claude_upstream() {
    struct Fixture {
        dir: PathBuf,
        servers: Vec<tokio::task::JoinHandle<()>>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for server in &self.servers {
                server.abort();
            }
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    let mut fixture = Fixture {
        dir: common::unique_temp_dir("claude-schema"),
        servers: Vec::new(),
    };
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let upstream = Router::new().route(
        "/v1/messages",
        post(move |Json(body): Json<Value>| {
            let sender = sender.clone();
            async move {
                sender.send(body).unwrap();
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "id": "msg_mock",
                            "type": "message",
                            "role": "assistant",
                            "model": MODEL,
                            "content": [{"type": "text", "text": "schema accepted"}],
                            "stop_reason": "end_turn",
                            "stop_sequence": null,
                            "usage": {"input_tokens": 1, "output_tokens": 1}
                        })
                        .to_string(),
                    ))
                    .unwrap()
            }
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", upstream_listener.local_addr().unwrap());
    fixture.servers.push(tokio::spawn(async move {
        axum::serve(upstream_listener, upstream).await.unwrap();
    }));

    std::fs::write(
        fixture.dir.join("claude-mock.json"),
        json!({
            "identity_slug": "claude-mock",
            "access_token": "mock_token",
            "api_key": "mock-key",
            "upstream_override": upstream_url,
            "email": "claude-mock@example.com",
            "expired": "2099-01-01T00:00:00Z",
            "account_id": "mock_account",
            "disabled": false,
            "type": "claude"
        })
        .to_string(),
    )
    .unwrap();

    let state = Arc::new(
        AppState::new(&GatewayConfig {
            auth_dir: fixture.dir.clone(),
            auth_refresh_enabled: false,
            ..Default::default()
        })
        .unwrap(),
    );
    let app = create_app(state);
    let gateway_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_url = format!("http://{}", gateway_listener.local_addr().unwrap());
    fixture.servers.push(tokio::spawn(async move {
        axum::serve(gateway_listener, app).await.unwrap();
    }));

    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
        .post(format!("{gateway_url}/v1/chat/completions"))
        .json(&omo_request())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "gateway rejected the OmO toolset: {body}"
    );
    assert_eq!(body["choices"][0]["message"]["role"], "assistant");

    let forwarded = receiver.recv().await.expect("upstream saw the request");
    assert_eq!(forwarded["model"], MODEL);

    let tools = forwarded["tools"].as_array().expect("anthropic tools");
    // The claude wire prefixes OpenAI function names.
    assert_eq!(tools[0]["name"], "custom_read");
    let read_schema = &tools[0]["input_schema"];
    assert!(
        !contains_union(read_schema),
        "union survived the dispatch: {read_schema}"
    );
    // Pruned from required because the union encoded optionality.
    assert_eq!(read_schema["required"], json!(["path"]));
    assert_eq!(read_schema["properties"]["offset"]["type"], "number");
    assert_eq!(read_schema["properties"]["offset"]["default"], json!(1));
    assert_eq!(read_schema["properties"]["limit"]["type"], "number");
    assert_eq!(read_schema["properties"]["limit"]["default"], json!(null));
    for tool in tools {
        let schema = &tool["input_schema"];
        assert!(
            !contains_union(schema),
            "union survived for {}: {schema}",
            tool["name"]
        );
    }
}
