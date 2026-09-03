use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use mahoquot_gateway::account::{load_account_members, ProviderKind};
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use mahoquot_types::Strategy;

/// Tests in this file run concurrently in one process, so the directory name
/// needs a per-instance counter: pid alone collides between them and the loader
/// would then see a sibling test's credentials.
static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

struct TempAuthDir(PathBuf);

impl TempAuthDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "qg-t11-{tag}-{}-{}",
            std::process::id(),
            DIR_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("create temp auth dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, name: &str, contents: &str) {
        std::fs::write(self.0.join(name), contents).expect("write credential");
    }
}

impl Drop for TempAuthDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn credential(kind: &str) -> String {
    credential_with(kind, &format!("{kind}-user"), None)
}

fn credential_with(kind: &str, identity_slug: &str, upstream: Option<&str>) -> String {
    let extra = match kind {
        "codex" => {
            r#""account_id":"acc-1","id_token":"idt","last_refresh":"2026-01-01T00:00:00Z","#
        }
        "antigravity" => r#""project_id":"proj-1","#,
        "kiro" => r#""region":"us-east-1","#,
        _ => "",
    };
    let mut value: serde_json::Value = serde_json::from_str(&format!(
        r#"{{{extra}"identity_slug":"{identity_slug}","access_token":"tok-{kind}",
            "refresh_token":"ref-{kind}","email":"user@{kind}.test",
            "expired":"2030-01-01T00:00:00Z","type":"{kind}"}}"#
    ))
    .expect("valid credential fixture");
    if let Some(upstream) = upstream {
        value["upstream_override"] = serde_json::Value::String(upstream.to_string());
    }
    value.to_string()
}

#[derive(Clone)]
struct CaptureState {
    hits: Arc<AtomicUsize>,
    paths: Arc<std::sync::Mutex<Vec<String>>>,
}

async fn capture_fixture(
    State(state): State<CaptureState>,
    uri: axum::http::Uri,
    _body: Bytes,
) -> impl IntoResponse {
    state.hits.fetch_add(1, Ordering::SeqCst);
    state.paths.lock().unwrap().push(uri.path().to_string());
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

async fn spawn_capture_fixture() -> (String, CaptureState) {
    let state = CaptureState {
        hits: Arc::new(AtomicUsize::new(0)),
        paths: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .fallback(post(capture_fixture))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), state)
}

async fn spawn_gateway(auth_dir: &Path) -> String {
    let config = GatewayConfig {
        auth_dir: auth_dir.to_path_buf(),
        config_path: auth_dir.join("config.yaml"),
        strategy: Strategy::FillFirst,
        max_failover: 6,
        auth_refresh_enabled: false,
        ..GatewayConfig::default()
    };
    let state = Arc::new(AppState::new(&config).expect("gateway state"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, create_app(state)).await.unwrap();
    });
    format!("http://{address}")
}

#[test]
fn pool_loads_every_supported_provider_from_one_auth_dir() {
    let dir = TempAuthDir::new("six");
    let kinds = [
        ("codex", ProviderKind::Codex),
        ("antigravity", ProviderKind::Antigravity),
        ("claude", ProviderKind::Claude),
        ("cursor", ProviderKind::Cursor),
        ("kiro", ProviderKind::Kiro),
        ("zcode", ProviderKind::Zcode),
    ];
    for (name, _) in kinds {
        dir.write(&format!("{name}-user.json"), &credential(name));
    }

    let members = load_account_members(dir.path()).expect("loader must succeed");
    assert_eq!(members.len(), 6, "expected one member per provider");

    let mut loaded: Vec<&str> = members.iter().map(|m| m.kind().as_str()).collect();
    loaded.sort_unstable();
    assert_eq!(
        loaded,
        vec!["antigravity", "claude", "codex", "cursor", "kiro", "zcode"]
    );
}

#[test]
fn malformed_and_unknown_credentials_are_skipped_without_dropping_valid_members() {
    let dir = TempAuthDir::new("bad");
    dir.write("codex-good.json", &credential("codex"));
    dir.write("claude-truncated.json", r#"{"access_token":"tok","#);
    dir.write(
        "vendor-unknown.json",
        r#"{"access_token":"t","refresh_token":"r","email":"u@x.test",
            "expired":"2030-01-01T00:00:00Z","type":"totally-unknown"}"#,
    );

    let members = load_account_members(dir.path())
        .expect("a malformed sibling must not turn the load into an error");

    assert_eq!(members.len(), 1, "only the valid credential should load");
    assert_eq!(members[0].kind(), ProviderKind::Codex);
}

#[test]
fn an_auth_dir_with_only_bad_credentials_yields_an_empty_pool_rather_than_an_error() {
    let dir = TempAuthDir::new("allbad");
    dir.write("claude-truncated.json", "{ not json");
    dir.write("vendor-unknown.json", r#"{"type":"nope"}"#);

    let members = load_account_members(dir.path()).expect("must not error");
    assert!(members.is_empty());
}

#[test]
fn each_provider_only_claims_models_it_can_actually_serve() {
    let dir = TempAuthDir::new("models");
    for name in ["codex", "claude", "cursor", "kiro", "zcode"] {
        dir.write(&format!("{name}-user.json"), &credential(name));
    }
    let members = load_account_members(dir.path()).expect("load");
    let by_kind = |k: ProviderKind| {
        members
            .iter()
            .find(|m| m.kind() == k)
            .expect("member present")
            .clone()
    };

    // A Kiro account must not be picked for an OpenAI model: it cannot serve it,
    // and routing there would send the request to the wrong upstream entirely.
    assert!(!by_kind(ProviderKind::Kiro).supports_model("gpt-5.6-sol"));
    assert!(!by_kind(ProviderKind::Zcode).supports_model("gpt-5.6-sol"));
    assert!(!by_kind(ProviderKind::Claude).supports_model("gpt-5.6-sol"));

    // ...but each must claim its own catalogue.
    assert!(by_kind(ProviderKind::Kiro).supports_model("kiro/claude-haiku-4-5-20251001"));
    assert!(by_kind(ProviderKind::Zcode).supports_model("glm-5.2"));
    assert!(by_kind(ProviderKind::Claude).supports_model("claude-sonnet-4-5-20250929"));
}

#[tokio::test]
async fn overlapping_closed_bindings_follow_registry_priority_not_pool_order() {
    let dir = TempAuthDir::new("binding-priority");
    let (upstream, capture) = spawn_capture_fixture().await;
    // Filename order deliberately puts Claude first. The registry gives the
    // Antigravity binding priority 100 and native Claude priority 90.
    dir.write(
        "a-claude.json",
        &credential_with("claude", "claude-lower", Some(&upstream)),
    );
    dir.write(
        "z-antigravity.json",
        &credential_with("antigravity", "antigravity-priority", Some(&upstream)),
    );
    let gateway = spawn_gateway(dir.path()).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "claude-sonnet-4-6",
            "messages": [{"role": "user", "content": "fixture"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(capture.hits.load(Ordering::SeqCst), 1);
    let paths = capture.paths.lock().unwrap().clone();
    assert_eq!(paths.len(), 1);
    assert!(
        paths[0].contains("v1internal"),
        "higher-priority Antigravity binding must win, captured {paths:?}"
    );
}

#[tokio::test]
async fn unknown_model_without_loaded_open_binding_is_local_model_not_found() {
    let dir = TempAuthDir::new("unknown-no-codex");
    let (upstream, capture) = spawn_capture_fixture().await;
    dir.write(
        "claude-only.json",
        &credential_with("claude", "claude-only", Some(&upstream)),
    );
    let gateway = spawn_gateway(dir.path()).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "unknown-fixture-model",
            "messages": [{"role": "user", "content": "fixture"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let payload: serde_json::Value = response.json().await.unwrap();
    assert_eq!(payload["error"]["code"], "model_not_found");
    assert_eq!(capture.hits.load(Ordering::SeqCst), 0);
}
