mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use mahoquot_types::Strategy;
use serde_json::Value;

const QA_PORT: u16 = 18872;

#[derive(Clone)]
struct CaptureState {
    hits: Arc<AtomicUsize>,
    paths: Arc<std::sync::Mutex<Vec<String>>>,
    hosts: Arc<std::sync::Mutex<Vec<String>>>,
}

async fn capture_fixture(
    State(state): State<CaptureState>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    _body: Bytes,
) -> impl IntoResponse {
    state.hits.fetch_add(1, Ordering::SeqCst);
    state.paths.lock().unwrap().push(uri.path().to_string());
    if let Some(host) = headers.get("host").and_then(|h| h.to_str().ok()) {
        state.hosts.lock().unwrap().push(host.to_string());
    }
    if uri.path().ends_with("/v1/messages") {
        return (
            StatusCode::OK,
            [("content-type", "application/json")],
            r#"{"id":"msg_fixture","type":"message","role":"assistant","content":[{"type":"text","text":"claude fixture"}],"model":"claude-sonnet-4-6","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#,
        );
    }
    (
        StatusCode::OK,
        [("content-type", "text/event-stream")],
        "data: {\"response\":{\"responseId\":\"ag_fixture\",\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"antigravity fixture\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":1,\"candidatesTokenCount\":1,\"totalTokenCount\":2}}}\n\n",
    )
}

async fn spawn_capture_fixture() -> (String, CaptureState, tokio::task::JoinHandle<()>) {
    let state = CaptureState {
        hits: Arc::new(AtomicUsize::new(0)),
        paths: Arc::new(std::sync::Mutex::new(Vec::new())),
        hosts: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .fallback(post(capture_fixture))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), state, task)
}

fn credential_with(kind: &str, identity_slug: &str, upstream: Option<&str>) -> String {
    let extra = match kind {
        "codex" => {
            r#""account_id":"acc-1","id_token":"idt","last_refresh":"2026-01-01T00:00:00Z","#
        }
        "antigravity" => r#""project_id":"project-fixture","#,
        "kiro" => r#""region":"us-east-1","#,
        _ => "",
    };
    let mut value: Value = serde_json::from_str(&format!(
        r#"{{{extra}"identity_slug":"{identity_slug}","access_token":"tok-{kind}",
            "refresh_token":"ref-{kind}","email":"user@{kind}.test",
            "expired":"2030-01-01T00:00:00Z","type":"{kind}"}}"#
    ))
    .expect("valid credential fixture");
    if let Some(upstream) = upstream {
        value["upstream_override"] = Value::String(upstream.to_string());
    }
    value.to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn task_11_http_qa_scenario() {
    let (upstream_url, capture_state, upstream_task) = spawn_capture_fixture().await;

    let auth_dir = common::unique_temp_dir("mahoquot-task-11-qa");

    // Filename order deliberately puts Claude first ("a-claude.json").
    // The registry gives the Antigravity binding priority 100 and native Claude priority 90.
    std::fs::write(
        auth_dir.join("a-claude.json"),
        credential_with("claude", "claude-lower", Some(&upstream_url)),
    )
    .unwrap();
    std::fs::write(
        auth_dir.join("z-antigravity.json"),
        credential_with("antigravity", "antigravity-priority", Some(&upstream_url)),
    )
    .unwrap();

    let gateway_config = GatewayConfig {
        port: QA_PORT,
        auth_dir: auth_dir.clone(),
        config_path: auth_dir.join("config.yaml"),
        strategy: Strategy::FillFirst,
        max_failover: 6,
        auth_refresh_enabled: false,
        ..GatewayConfig::default()
    };

    let state = Arc::new(AppState::new(&gateway_config).expect("gateway state"));
    let app = create_app(Arc::clone(&state));

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{QA_PORT}"))
        .await
        .expect("bound to QA_PORT");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let completions_url = format!("http://127.0.0.1:{QA_PORT}/v1/chat/completions");

    let mut http_evidence = String::new();

    // 1. Scenario 1: Model claude-sonnet-4-6 with priority-based routing
    // Higher priority Antigravity binding (100) must win over Claude (90)
    let payload1 = serde_json::json!({
        "model": "claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "fixture"}]
    });
    let payload1_str = serde_json::to_string(&payload1).unwrap();

    http_evidence.push_str("POST /v1/chat/completions HTTP/1.1\n");
    http_evidence.push_str(&format!("Host: 127.0.0.1:{QA_PORT}\n"));
    http_evidence.push_str("Content-Type: application/json\n\n");
    http_evidence.push_str(&serde_json::to_string_pretty(&payload1).unwrap());
    http_evidence.push_str("\n\n");

    // Execute via curl to directly satisfy requirement 5 invocation
    let curl1_output = tokio::process::Command::new("curl")
        .args([
            "-i",
            &completions_url,
            "-H",
            "Content-Type: application/json",
            "--data",
            &payload1_str,
        ])
        .output()
        .await
        .expect("executed curl for scenario 1");

    let curl1_response = String::from_utf8_lossy(&curl1_output.stdout).to_string();
    assert!(
        curl1_output.status.success(),
        "curl 1 failed: {}",
        String::from_utf8_lossy(&curl1_output.stderr)
    );
    assert!(curl1_response.contains("200 OK"));
    http_evidence.push_str(&curl1_response);
    http_evidence.push_str("\n\n---\n\n");

    // Check upstream capture assertions
    assert_eq!(capture_state.hits.load(Ordering::SeqCst), 1);
    let paths = capture_state.paths.lock().unwrap().clone();
    assert_eq!(paths.len(), 1);
    assert!(
        paths[0].contains("v1internal"),
        "higher-priority Antigravity binding must win; captured: {:?}",
        paths
    );
    let hosts = capture_state.hosts.lock().unwrap().clone();
    for host in &hosts {
        assert!(
            host.starts_with("127.0.0.1:") || host.starts_with("localhost:"),
            "request reached real host instead of loopback fixture: {host}"
        );
    }

    // 2. Scenario 2: Unknown model with no loaded Codex
    // MUST return local 400 Bad Request with code: model_not_found and ZERO fixture requests
    let payload2 = serde_json::json!({
        "model": "unknown-fixture-model",
        "messages": [{"role": "user", "content": "fixture"}]
    });
    let payload2_str = serde_json::to_string(&payload2).unwrap();

    http_evidence.push_str("POST /v1/chat/completions HTTP/1.1\n");
    http_evidence.push_str(&format!("Host: 127.0.0.1:{QA_PORT}\n"));
    http_evidence.push_str("Content-Type: application/json\n\n");
    http_evidence.push_str(&serde_json::to_string_pretty(&payload2).unwrap());
    http_evidence.push_str("\n\n");

    let curl2_output = tokio::process::Command::new("curl")
        .args([
            "-i",
            &completions_url,
            "-H",
            "Content-Type: application/json",
            "--data",
            &payload2_str,
        ])
        .output()
        .await
        .expect("executed curl for scenario 2");

    let curl2_response = String::from_utf8_lossy(&curl2_output.stdout).to_string();
    assert!(
        curl2_output.status.success(),
        "curl 2 failed: {}",
        String::from_utf8_lossy(&curl2_output.stderr)
    );
    assert!(curl2_response.contains("400 Bad Request"));
    assert!(curl2_response.contains("model_not_found"));
    http_evidence.push_str(&curl2_response);
    http_evidence.push('\n');

    // Fixture upstream hit count must remain 1 (0 new hits)
    assert_eq!(
        capture_state.hits.load(Ordering::SeqCst),
        1,
        "zero fixture requests must reach upstream for unknown model"
    );

    // Shutdown and clean up
    let _ = shutdown_tx.send(());
    server_handle.await.expect("server gracefully stopped");
    upstream_task.abort();
    std::fs::remove_dir_all(auth_dir).ok();

    // Write full evidence files to both repositories
    let proxy_evidence_dir =
        PathBuf::from("/Users/indo/code/project/mahoquot-proxy/.omo/evidence/model-registry");
    let quotio_evidence_dir =
        PathBuf::from("/Users/indo/code/project/quotio-rs/.omo/evidence/model-registry");
    std::fs::create_dir_all(&proxy_evidence_dir).expect("proxy evidence dir created");
    std::fs::create_dir_all(&quotio_evidence_dir).expect("quotio evidence dir created");

    std::fs::write(
        proxy_evidence_dir.join("task-11-routing.http"),
        &http_evidence,
    )
    .expect("wrote proxy task-11-routing.http");

    std::fs::write(
        quotio_evidence_dir.join("task-11-routing.http"),
        &http_evidence,
    )
    .expect("wrote quotio task-11-routing.http");
}
