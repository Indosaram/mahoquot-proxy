mod common;

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State as AxumState;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use base64::Engine as _;
use common::unique_temp_dir;
use mahoquot_gateway::{config::GatewayConfig, routes::create_app, state::AppState};
use serde_json::{json, Value};

const PLAN_JWT_PAYLOAD: &[u8] = br#"{"user_id":"usr-42"}"#;

fn plan_jwt() -> String {
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(PLAN_JWT_PAYLOAD);
    format!("eyJhbGciOiJub25lIn0.{payload}.sig")
}

fn zcode_credential(upstream_override: &str) -> String {
    json!({
        "identity_slug": "zc",
        "access_token": plan_jwt(),
        "refresh_token": "",
        "email": "z@example.com",
        "expired": "2099-01-01T00:00:00Z",
        "type": "zcode",
        "upstream_override": upstream_override,
    })
    .to_string()
}

#[derive(Clone)]
struct CapturedRequest {
    headers: Vec<(String, String)>,
    body: Value,
}

type Capture = Arc<Mutex<Vec<CapturedRequest>>>;

fn capture() -> Capture {
    Arc::new(Mutex::new(Vec::new()))
}

async fn spawn_mock_plan_gateway(
    respond: impl Fn() -> Response + Send + Sync + Clone + 'static,
) -> (String, Capture) {
    let captured: Capture = capture();
    let state_for_route = captured.clone();
    let app = Router::new()
        .route(
            "/v1/messages",
            post(
                move |AxumState(state): AxumState<Capture>,
                      headers: axum::http::HeaderMap,
                      body: axum::body::Bytes| {
                    let respond = respond.clone();
                    async move {
                        let parsed = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
                        let headers = headers
                            .iter()
                            .map(|(name, value)| {
                                (
                                    name.as_str().to_string(),
                                    value.to_str().unwrap_or_default().to_string(),
                                )
                            })
                            .collect();
                        state.lock().unwrap().push(CapturedRequest {
                            headers,
                            body: parsed,
                        });
                        respond()
                    }
                },
            ),
        )
        .with_state(state_for_route);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://127.0.0.1:{port}"), captured)
}

async fn spawn_gateway(auth_dir: &std::path::Path) -> String {
    let config = GatewayConfig {
        usage_poll_secs: 0,
        port: 0,
        auth_dir: auth_dir.to_path_buf(),
        strategy: mahoquot_types::Strategy::StrictRoundRobin,
        max_failover: 1,
        log_level: "warn".to_string(),
        api_keys: mahoquot_gateway::inbound::ApiKeys::default(),
        models_env: None,
        refresh_url: mahoquot_providers::refresh::REFRESH_TOKEN_URL.to_string(),
        auth_refresh_enabled: false,
        ..Default::default()
    };
    let state = Arc::new(AppState::new(&config).unwrap());
    let app = create_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://127.0.0.1:{port}")
}

async fn send_messages(gateway: &str, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gateway}/v1/messages"))
        .json(&json!({
            "model": model,
            "max_tokens": 16,
            "stream": true,
            "messages": [
                { "role": "user", "content": [
                    { "type": "text", "text": "hi", "cache_control": { "type": "ephemeral" } }
                ]}
            ]
        }))
        .send()
        .await
        .unwrap()
}

const ANTHROPIC_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_z\",\"role\":\"assistant\",\"content\":[]}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
);

#[tokio::test]
async fn zcode_plan_request_carries_client_identity_and_body() {
    let temp_dir = unique_temp_dir("qgw-test-t31-zcode-identity");
    let (upstream, captured) = spawn_mock_plan_gateway(|| {
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .body(Body::from(ANTHROPIC_SSE))
            .unwrap()
            .into_response()
    })
    .await;
    std::fs::write(temp_dir.join("zcode-z.json"), zcode_credential(&upstream)).unwrap();
    let gateway = spawn_gateway(&temp_dir).await;

    let response = send_messages(&gateway, "glm-5.3-flash").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(
        body.contains("message_start"),
        "SSE passthrough broken: {body}"
    );

    let captures = captured.lock().unwrap();
    assert_eq!(captures.len(), 1);
    let sent = &captures[0];
    let header = |name: &str| {
        sent.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let strip_bearer_prefix = |value: Option<String>| {
        value.map(|v| v.strip_prefix("Bearer ").map(str::to_string).unwrap_or(v))
    };
    assert_eq!(
        strip_bearer_prefix(header("authorization")).as_deref(),
        Some(plan_jwt().as_str())
    );
    assert_eq!(header("anthropic-version").as_deref(), Some("2023-06-01"));
    assert_eq!(
        header("user-agent").as_deref(),
        Some("ZCode/3.11.2 ai-sdk/anthropic/3.0.81")
    );
    assert_eq!(header("x-title").as_deref(), Some("Z Code@cli"));
    assert_eq!(header("x-zcode-agent").as_deref(), Some("glm"));
    assert_eq!(header("x-zcode-session-type").as_deref(), Some("main"));
    assert_eq!(
        header("x-client-language").is_some_and(|v| !v.is_empty()),
        true
    );
    assert_eq!(
        header("x-client-timezone").is_some_and(|v| !v.is_empty()),
        true
    );
    assert!(header("x-platform").unwrap_or_default().contains('-'));
    assert!(header("x-request-id").unwrap_or_default().len() >= 32);
    assert!(header("x-zcode-trace-id").unwrap_or_default().len() >= 32);
    assert_eq!(header("x-api-key"), None);
    assert_eq!(header("x-query-id"), None);
    assert_eq!(header("x-session-id"), None);
    assert_eq!(header("x-device-mid"), None);

    assert_eq!(sent.body["model"], "GLM-5.3-Flash");
    let system = sent.body["system"].as_array().unwrap();
    assert!(system[0]["text"]
        .as_str()
        .unwrap()
        .starts_with("You are ZCode"));
    assert!(system[2]["text"]
        .as_str()
        .unwrap()
        .contains("You are powered by the model named GLM-5.3-Flash."));
    assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(sent.body["metadata"]["user_id"], "usr-42");
    assert_eq!(header("x-request-id").as_deref().unwrap().len() >= 32, true);

    let last_message = &sent.body["messages"].as_array().unwrap()[0];
    let block = &last_message["content"].as_array().unwrap()[0];
    assert_eq!(block["cache_control"]["type"], "ephemeral");

    std::fs::remove_dir_all(temp_dir).ok();
}

#[tokio::test]
async fn zcode_plan_biz_error_inside_200_maps_to_429() {
    let temp_dir = unique_temp_dir("qgw-test-t31-zcode-biz");
    let (upstream, _captured) = spawn_mock_plan_gateway(|| {
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(Body::from(
                json!({ "code": 1005, "msg": "exceed quota limit" }).to_string(),
            ))
            .unwrap()
            .into_response()
    })
    .await;
    std::fs::write(temp_dir.join("zcode-z.json"), zcode_credential(&upstream)).unwrap();
    let gateway = spawn_gateway(&temp_dir).await;

    let response = send_messages(&gateway, "glm-5.3-flash").await;
    assert_eq!(response.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    let payload: Value = response.json().await.unwrap();
    assert_eq!(payload["error"]["type"], "rate_limit_error");
    assert_eq!(payload["error"]["code"], 1005);
    assert!(payload["error"]["message"]
        .as_str()
        .unwrap()
        .contains("1005"));

    std::fs::remove_dir_all(temp_dir).ok();
}

#[tokio::test]
async fn zcode_plan_waf_challenge_maps_to_upstream_error() {
    let temp_dir = unique_temp_dir("qgw-test-t31-zcode-waf");
    let (upstream, _captured) = spawn_mock_plan_gateway(|| {
        Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header("Content-Type", "application/json")
            .header("x-aliyun-captcha-verify-param", "challenge-param")
            .body(Body::from(json!({ "code": 3007 }).to_string()))
            .unwrap()
            .into_response()
    })
    .await;
    std::fs::write(temp_dir.join("zcode-z.json"), zcode_credential(&upstream)).unwrap();
    let gateway = spawn_gateway(&temp_dir).await;

    let response = send_messages(&gateway, "glm-5.3-flash").await;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_GATEWAY);
    let payload: Value = response.json().await.unwrap();
    assert_eq!(payload["error"]["type"], "upstream_error");
    assert!(payload["error"]["message"]
        .as_str()
        .unwrap()
        .contains("captcha challenge"));

    std::fs::remove_dir_all(temp_dir).ok();
}
