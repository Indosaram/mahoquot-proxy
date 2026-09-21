//! Codex vision regressions. The upstream here is a strict local mock, not OpenAI.
mod common;

use std::{path::PathBuf, sync::Arc, time::Duration};

use axum::{body::Body, http::StatusCode, response::Response, routing::post, Json, Router};
use mahoquot_gateway::{
    compat::openai_to_codex, config::GatewayConfig, routes::create_app, state::AppState,
};
use serde_json::{json, Value};

const MODEL: &str = "gpt-6-astra";
// Synthetic 32x32 RGB PNG with valid IHDR, IDAT and IEND checksums.
const PNG: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAIAAAD8GO2jAAAAKElEQVR4nO3NsQ0AAAzCMP5/un0CNkuZ41wybXsHAAAAAAAAAAAAxR4yw/wuPL6QkAAAAABJRU5ErkJggg==";
const REMOTE: &str = "https://example.com/screenshot.png";

fn translate(messages: Value) -> Value {
    let request = json!({"model": MODEL, "messages": messages});
    let translated = openai_to_codex(&serde_json::to_vec(&request).unwrap()).unwrap();
    assert_eq!(translated.model, MODEL);
    serde_json::from_slice(&translated.body).unwrap()
}

#[test]
fn astra_user_images_preserve_urls_and_optional_detail() {
    for url in [PNG, REMOTE] {
        for detail in [None, Some("low"), Some("high"), Some("auto"), Some("original")] {
            let mut image = json!({"type": "image_url", "image_url": {"url": url}});
            let mut expected = json!({"type": "input_image", "image_url": url});
            if let Some(detail) = detail {
                image["image_url"]["detail"] = json!(detail);
                expected["detail"] = json!(detail);
            }
            let body = translate(json!([{"role": "user", "content": [
                {"type": "text", "text": "Read the button names"}, image
            ]}]));
            assert_eq!(
                body["input"][0]["content"],
                json!([
                    {"type": "input_text", "text": "Read the button names"}, expected
                ]),
                "url={url}, detail={detail:?}"
            );
        }
    }
}

fn tool_messages(role: &str, content: Value) -> Value {
    json!([
        {"role": "user", "content": "Read the screenshot"},
        {"role": "assistant", "content": null, "tool_calls": [{
            "id": "call_image", "type": "function",
            "function": {"name": "read_image", "arguments": "{}"}
        }]},
        {"role": role, "tool_call_id": "call_image", "content": content}
    ])
}

#[test]
fn astra_image_tool_output_keeps_images_text_order_and_call_id() {
    for role in ["tool", "function"] {
        let body = translate(tool_messages(
            role,
            json!([
                {"type": "text", "text": "Screenshot follows"},
                {"type": "image_url", "image_url": {"url": PNG, "detail": "high"}},
                {"type": "text", "text": "Read the button labels"},
                {"type": "image_url", "image_url": {"url": REMOTE}}
            ]),
        ));
        assert_eq!(
            body["input"][2],
            json!({
                "type": "function_call_output", "call_id": "call_image", "output": [
                    {"type": "input_text", "text": "Screenshot follows"},
                    {"type": "input_image", "image_url": PNG, "detail": "high"},
                    {"type": "input_text", "text": "Read the button labels"},
                    {"type": "input_image", "image_url": REMOTE}
                ]
            })
        );
    }
}

#[test]
fn astra_image_only_tool_result_is_not_an_empty_string() {
    let body = translate(tool_messages(
        "tool",
        json!([
            {"type": "image_url", "image_url": {"url": PNG}}
        ]),
    ));
    assert_eq!(
        body["input"][2]["output"],
        json!([
            {"type": "input_image", "image_url": PNG}
        ])
    );
}

#[test]
fn codex_text_only_tool_outputs_keep_legacy_string_shape() {
    for (content, expected) in [
        (json!("plain output"), "plain output"),
        (
            json!([{"type": "text", "text": "one"}, {"type": "text", "text": "two"}]),
            "onetwo",
        ),
        (Value::Null, ""),
        (json!([]), ""),
    ] {
        let body = translate(tool_messages("tool", content));
        assert_eq!(body["input"][2]["output"], expected);
        assert_eq!(body["input"][2]["call_id"], "call_image");
    }
}

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

async fn assert_chat_relay(messages: Value, expected_input: Value, stream: bool) {
    let mut fixture = Fixture {
        dir: common::unique_temp_dir("astra-vision"),
        servers: Vec::new(),
    };
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let upstream = Router::new().route(
        common::CODEX_PATH,
        post(move |Json(body): Json<Value>| {
            let expected_input = expected_input.clone();
            let sender = sender.clone();
            async move {
                sender.send(body.clone()).unwrap();
                // This deliberately validates the translated contract. A failure is
                // a mock diagnostic, not a claimed production OpenAI error message.
                if body["model"] != MODEL || body["input"] != expected_input {
                    return Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .header("content-type", "application/json")
                        .body(Body::from(
                            json!({"error": {
                                "type": "invalid_request_error",
                                "code": "mock_vision_payload_mismatch",
                                "message": "Mock Codex contract: image content or detail was lost in translation"
                            }})
                            .to_string(),
                        ))
                        .unwrap();
                }
                assert_eq!(body["store"], false);
                assert_eq!(body["stream"], true);
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from(common::codex_sse("mock vision accepted")))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let upstream_url = format!("http://{}", listener.local_addr().unwrap());
    fixture.servers.push(tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap()
    }));
    std::fs::write(
        fixture.dir.join("codex-vision-plus.json"),
        common::create_auth_file_json(
            "vision",
            "acc_vision",
            "synthetic_token",
            Some(&upstream_url),
        ),
    )
    .unwrap();
    let state = Arc::new(
        AppState::new(&GatewayConfig {
            auth_dir: fixture.dir.clone(),
            auth_refresh_enabled: false,
            max_failover: 1,
            ..Default::default()
        })
        .unwrap(),
    );
    let app = create_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let gateway_url = format!("http://{}", listener.local_addr().unwrap());
    fixture.servers.push(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap()
    }));

    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
        .post(format!("{gateway_url}/v1/chat/completions"))
        .json(&json!({"model": MODEL, "stream": stream, "messages": messages}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    println!("Astra mock relay stream={stream}: HTTP {status}; {body}");
    assert_eq!(status, StatusCode::OK, "{body}");
    let forwarded = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(forwarded["model"], MODEL);
    if stream {
        assert!(body.contains("mock vision accepted"));
        assert!(body.trim_end().ends_with("data: [DONE]"));
    } else {
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["model"], MODEL);
        assert_eq!(
            body["choices"][0]["message"]["content"],
            "mock vision accepted"
        );
    }
}

#[tokio::test]
async fn astra_chat_user_image_reaches_codex_without_image_generation_capability() {
    let registry = mahoquot_registry::embedded_registry_snapshot().unwrap();
    let resolved = registry.resolve(MODEL).unwrap();
    assert!(resolved
        .effective_capabilities
        .contains(&mahoquot_registry::ModelCapability::Chat));
    assert!(!resolved
        .effective_capabilities
        .contains(&mahoquot_registry::ModelCapability::Image));
    for stream in [false, true] {
        assert_chat_relay(
            json!([{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": PNG}}
            ]}]),
            json!([{"role": "user", "content": [
                {"type": "input_image", "image_url": PNG}
            ]}]),
            stream,
        )
        .await;
    }
}

#[tokio::test]
async fn astra_chat_user_image_detail_reaches_codex() {
    for stream in [false, true] {
        assert_chat_relay(
            json!([{"role": "user", "content": [
                {"type": "text", "text": "Read the screenshot"},
                {"type": "image_url", "image_url": {"url": PNG, "detail": "high"}}
            ]}]),
            json!([{"role": "user", "content": [
                {"type": "input_text", "text": "Read the screenshot"},
                {"type": "input_image", "image_url": PNG, "detail": "high"}
            ]}]),
            stream,
        )
        .await;
    }
}

#[tokio::test]
async fn astra_chat_image_tool_result_reaches_codex() {
    for stream in [false, true] {
        assert_chat_relay(
            tool_messages(
                "tool",
                json!([
                    {"type": "image_url", "image_url": {"url": PNG, "detail": "original"}}
                ]),
            ),
            json!([
                {"role": "user", "content": [{"type": "input_text", "text": "Read the screenshot"}]},
                {"type": "function_call", "call_id": "call_image", "name": "read_image", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_image", "output": [
                    {"type": "input_image", "image_url": PNG, "detail": "original"}
                ]}
            ]),
            stream,
        )
        .await;
    }
}
