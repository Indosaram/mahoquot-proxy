mod common;

use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State as AxumState;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
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
    spawn_gateway_with_captcha(auth_dir, None, None).await
}

async fn spawn_gateway_with_captcha(
    auth_dir: &std::path::Path,
    captcha_config_url: Option<String>,
    captcha_solver_bin: Option<std::path::PathBuf>,
) -> String {
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
        captcha_config_url,
        captcha_solver_bin,
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
    assert!(header("x-client-language").is_some_and(|v| !v.is_empty()));
    assert!(header("x-client-timezone").is_some_and(|v| !v.is_empty()));
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
    assert!(header("x-request-id").as_deref().unwrap().len() >= 32);

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
    // The in-200 biz 1005 maps to a 429 and benches the account; with the
    // pool exhausted the client sees the deterministic 503 instead.
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    let payload: Value = response.json().await.unwrap();
    assert_eq!(payload["error"]["type"], "quota_exhausted");
    assert_eq!(payload["error"]["code"], "MODEL_QUOTA_EXHAUSTED");

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
    // No solver override and a dead config endpoint: the solve path fails fast,
    // offline — the challenge still surfaces as the bounded 502 mapping.
    let gateway =
        spawn_gateway_with_captcha(&temp_dir, Some("http://127.0.0.1:9".to_string()), None).await;

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

/// Config endpoint mock serving the captcha scene the relay must read.
async fn spawn_mock_captcha_config() -> String {
    let app = Router::new().route(
        "/configs",
        get(|| async {
            Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/json")
                .body(Body::from(
                    json!({ "data": { "configs": { "captcha": {
                        "enabled": true, "sceneId": "scene-t31", "prefix": "pret31", "region": "sgp"
                    } } } })
                    .to_string(),
                ))
                .unwrap()
                .into_response()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://127.0.0.1:{port}")
}

/// Fake solver sidecar: takes the solve request via argv (production contract)
/// and prints a fixed result.
fn write_solver_script(dir: &std::path::Path, stdout_line: &str) -> std::path::PathBuf {
    let path = dir.join("fake-solver.sh");
    std::fs::write(
        &path,
        format!("#!/bin/sh\nprintf '%s\\n' '{stdout_line}'\n"),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

#[tokio::test]
async fn zcode_plan_challenge_solves_and_replays_once() {
    let temp_dir = unique_temp_dir("qgw-test-t31-zcode-solve");
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_respond = hits.clone();
    let (upstream, captured) = spawn_mock_plan_gateway(move || {
        let hit = hits_for_respond.fetch_add(1, AtomicOrdering::SeqCst);
        if hit == 0 {
            Response::builder()
                .status(StatusCode::FORBIDDEN)
                .header("Content-Type", "application/json")
                .body(Body::from(
                    json!({ "code": 3007, "msg": "waf" }).to_string(),
                ))
                .unwrap()
                .into_response()
        } else {
            Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "text/event-stream")
                .body(Body::from(ANTHROPIC_SSE))
                .unwrap()
                .into_response()
        }
    })
    .await;
    let config_base = spawn_mock_captcha_config().await;
    let solver = write_solver_script(&temp_dir, r#"{"ok":true,"param":"test-param-t31"}"#);
    std::fs::write(temp_dir.join("zcode-z.json"), zcode_credential(&upstream)).unwrap();
    let gateway = spawn_gateway_with_captcha(
        &temp_dir,
        Some(format!("{config_base}/configs")),
        Some(solver),
    )
    .await;

    let response = send_messages(&gateway, "glm-5.3-flash").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("message_start"), "replayed SSE lost: {body}");

    let captures = captured.lock().unwrap();
    assert_eq!(captures.len(), 2, "expected exactly one challenge replay");
    let header = |cap: &CapturedRequest, name: &str| {
        cap.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    assert_eq!(
        header(&captures[1], "x-aliyun-captcha-verify-param").as_deref(),
        Some("test-param-t31")
    );
    assert_eq!(
        header(&captures[1], "x-aliyun-captcha-verify-region").as_deref(),
        Some("sgp")
    );
    assert_eq!(
        header(&captures[0], "x-aliyun-captcha-verify-param"),
        None,
        "the original challenge request must carry no verify header"
    );
    // The replay reuses the original plan body byte-for-byte.
    assert_eq!(captures[0].body, captures[1].body);
    assert_eq!(captures[1].body["model"], "GLM-5.3-Flash");

    std::fs::remove_dir_all(temp_dir).ok();
}

#[tokio::test]
async fn zcode_plan_solve_failure_surfaces_upstream_error_without_replay() {
    let temp_dir = unique_temp_dir("qgw-test-t31-zcode-solvefail");
    let (upstream, captured) = spawn_mock_plan_gateway(|| {
        Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header("Content-Type", "application/json")
            .body(Body::from(
                json!({ "code": 3007, "msg": "waf" }).to_string(),
            ))
            .unwrap()
            .into_response()
    })
    .await;
    let config_base = spawn_mock_captcha_config().await;
    let solver = write_solver_script(&temp_dir, r#"{"ok":false,"error":"solver exploded"}"#);
    std::fs::write(temp_dir.join("zcode-z.json"), zcode_credential(&upstream)).unwrap();
    let gateway = spawn_gateway_with_captcha(
        &temp_dir,
        Some(format!("{config_base}/configs")),
        Some(solver),
    )
    .await;

    let response = send_messages(&gateway, "glm-5.3-flash").await;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_GATEWAY);
    let payload: Value = response.json().await.unwrap();
    assert_eq!(payload["error"]["type"], "upstream_error");
    let message = payload["error"]["message"].as_str().unwrap();
    assert!(message.contains("automatic solve failed"), "{message}");
    assert!(message.contains("solver exploded"), "{message}");
    assert_eq!(
        captured.lock().unwrap().len(),
        1,
        "a failed solve must not replay into the challenge"
    );

    std::fs::remove_dir_all(temp_dir).ok();
}
