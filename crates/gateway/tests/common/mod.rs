#![allow(dead_code)]

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn unique_temp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("{prefix}-{pid}-{nanos}-{seq}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Supply the transport peer metadata normally installed by `axum::serve` to
/// oneshot requests that exercise the dedicated management gate.
pub fn with_loopback_peer<B>(mut request: axum::http::Request<B>) -> axum::http::Request<B> {
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            0,
        ))));
    request
}

/// Configure the dedicated management secret (Task 26) in the gateway config
/// file at `config_path` before the gateway state is built, preserving any
/// YAML the test already wrote there. Management and /admin routes answer 404
/// until a secret is configured.
pub fn configure_management_secret(config_path: &std::path::Path, secret: &str) {
    let raw = std::fs::read_to_string(config_path).unwrap_or_default();
    let mut doc: serde_yaml::Value = if raw.trim().is_empty() {
        serde_yaml::Value::Mapping(Default::default())
    } else {
        serde_yaml::from_str(&raw).expect("test config yaml")
    };
    let root = doc.as_mapping_mut().expect("config root mapping");
    let key = serde_yaml::Value::from("remote-management");
    if !root.contains_key(&key) {
        root.insert(key.clone(), serde_yaml::Value::Mapping(Default::default()));
    }
    root.get_mut(&key)
        .and_then(serde_yaml::Value::as_mapping_mut)
        .expect("remote-management mapping")
        .insert("secret-key".into(), secret.into());
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).expect("config dir");
    }
    std::fs::write(config_path, serde_yaml::to_string(&doc).unwrap()).expect("write config");
}

/// Task 26 fixture: the operator's dedicated management secret is the first
/// inbound key the test already presents, written into the gateway config
/// file before the state is built (management/admin answer 404 until a
/// secret is configured). Tests that assert the master/management split
/// itself (t6, t26, t27) configure distinct keys explicitly instead.
pub fn managed(
    mut config: mahoquot_gateway::config::GatewayConfig,
) -> mahoquot_gateway::config::GatewayConfig {
    if config.config_path.as_os_str().is_empty() {
        config.config_path = config.auth_dir.join("config.yaml");
    }
    if let Some(secret) = config.api_keys.values().first().cloned() {
        configure_management_secret(&config.config_path, &secret);
    }
    config
}

/// Borrowing form of [`managed`] for `AppState::new(&...)` call sites.
pub fn managed_ref(
    config: &mahoquot_gateway::config::GatewayConfig,
) -> mahoquot_gateway::config::GatewayConfig {
    managed(config.clone())
}

pub fn create_auth_file_json(
    id: &str,
    account_id: &str,
    access_token: &str,
    upstream_override: Option<&str>,
) -> String {
    let mut obj = serde_json::json!({
        "identity_slug": id,
        "access_token": access_token,
        "account_id": account_id,
        "email": format!("{id}@example.com"),
        "expired": "2099-01-01T00:00:00Z",
        "id_token": "fake_idt",
        "last_refresh": "2026-08-27T00:00:00Z",
        "refresh_token": "fake_rt",
        "type": "plus"
    });
    if let Some(url) = upstream_override {
        obj["upstream_override"] = serde_json::Value::String(url.to_string());
    }
    serde_json::to_string(&obj).unwrap()
}

pub const CODEX_PATH: &str = "/backend-api/codex/responses";

pub const OPENAI_REQUEST: &str =
    r#"{"model":"codex","stream":true,"messages":[{"role":"user","content":"hi"}]}"#;

/// A request body carrying an explicit session identity.
///
/// The relay pins a session to one account for prompt-cache affinity, and with
/// no session identity it derives one from the request body. Reusing one fixed
/// fixture therefore pins every request to the same account, which is correct
/// production behaviour but hides the distribution these routing tests measure.
/// Give each request its own session so the pool spread is what is asserted.
pub fn session_request(session: &str) -> String {
    format!(
        r#"{{"model":"codex","stream":true,"messages":[{{"role":"user","content":"hi"}}],"session_id":"{session}"}}"#
    )
}

pub fn codex_sse(text: &str) -> String {
    format!(
        concat!(
            "event: response.created\n",
            "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp_fixture\"}}}}\n\n",
            "event: response.output_item.added\n",
            "data: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"id\":\"msg_fixture\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}}}\n\n",
            "event: response.output_text.delta\n",
            "data: {{\"type\":\"response.output_text.delta\",\"content_index\":0,\"delta\":\"{text}\",\"item_id\":\"msg_fixture\",\"output_index\":0}}\n\n",
            "event: response.completed\n",
            "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_fixture\",\"status\":\"completed\"}}}}\n\n",
        ),
        text = text
    )
}
