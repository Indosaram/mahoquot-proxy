mod common;

use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use common::unique_temp_dir;
use http_body_util::BodyExt;
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use mahoquot_types::Strategy;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;

const MODEL: &str = "claude-sonnet-4-5-20250929";
const OK_MESSAGE: &str = r#"{"id":"msg_ok","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;

static INIT_CRYPTO: Once = Once::new();

type Scripted = (u16, &'static str, String);

#[derive(Clone, Default)]
struct Upstream {
    hits: Arc<AtomicUsize>,
    script: Arc<Mutex<VecDeque<Scripted>>>,
    seen: Arc<Mutex<Vec<(HeaderMap, serde_json::Value)>>>,
}

impl Upstream {
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn then(&self, status: u16, content_type: &'static str, body: impl Into<String>) {
        self.script
            .lock()
            .unwrap()
            .push_back((status, content_type, body.into()));
    }

    fn last(&self) -> (HeaderMap, serde_json::Value) {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("at least one upstream call recorded")
    }

}

struct TlsListener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, addr) = match self.tcp.accept().await {
                Ok(pair) => pair,
                Err(_) => continue,
            };
            match self.acceptor.accept(stream).await {
                Ok(tls_stream) => return (tls_stream, addr),
                Err(_) => continue,
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

fn make_tls_acceptor() -> TlsAcceptor {
    INIT_CRYPTO.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    let cert_der = CertificateDer::from_slice(include_bytes!("fixtures/claude_direct_cert.der"));
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(&include_bytes!(
        "fixtures/claude_direct_key.der"
    )[..]));
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .expect("valid test server certificate and key");
    TlsAcceptor::from(Arc::new(config))
}

fn make_direct_client(local_addr: SocketAddr) -> reqwest::Client {
    let ca_pem = include_bytes!("fixtures/claude_direct_ca.pem");
    let ca_cert = reqwest::Certificate::from_pem(ca_pem).expect("valid ca cert pem");
    reqwest::Client::builder()
        .no_proxy()
        .resolve("api.anthropic.com", local_addr)
        .add_root_certificate(ca_cert)
        .gzip(true)
        .deflate(true)
        .brotli(true)
        .zstd(true)
        .build()
        .expect("build direct reqwest client")
}

struct Fixture {
    dir: std::path::PathBuf,
    servers: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for server in &self.servers {
            server.abort();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Fixture {
    fn new(prefix: &str) -> Self {
        Self {
            dir: unique_temp_dir(prefix),
            servers: Vec::new(),
        }
    }

    async fn tls_upstream(&mut self) -> (SocketAddr, Upstream) {
        let upstream = Upstream::default();
        let handle = upstream.clone();
        let app = Router::new().route(
            "/v1/messages",
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let upstream = handle.clone();
                async move {
                    upstream.hits.fetch_add(1, Ordering::SeqCst);
                    let value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                    upstream.seen.lock().unwrap().push((headers, value));
                    let (status, content_type, body) = upstream
                        .script
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or((200, "application/json", OK_MESSAGE.to_string()));
                    (
                        StatusCode::from_u16(status).unwrap(),
                        [("content-type", content_type)],
                        body,
                    )
                        .into_response()
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tls_listener = TlsListener {
            tcp: listener,
            acceptor: make_tls_acceptor(),
        };
        self.servers.push(tokio::spawn(async move {
            let _ = axum::serve(tls_listener, app).await;
        }));
        (addr, upstream)
    }

    fn oauth_direct(&self, id: &str) {
        write_claude(&self.dir, id, None, None);
    }

    fn api_key(&self, id: &str) {
        write_claude(&self.dir, id, None, Some("sk-ant-api-test"));
    }

    fn state(&self, upstream_addr: SocketAddr) -> Arc<AppState> {
        self.state_with_config(upstream_addr, None)
    }

    fn state_with_config(&self, upstream_addr: SocketAddr, yaml: Option<&str>) -> Arc<AppState> {
        for entry in std::fs::read_dir(&self.dir).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("claude-") && n.ends_with(".json")) {
                let mut credential: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                credential["upstream_override"] = serde_json::json!(format!("https://api.anthropic.com:{}", upstream_addr.port()));
                std::fs::write(path, credential.to_string()).unwrap();
            }
        }
        let config_path = self.dir.join("config.yaml");
        if let Some(yaml) = yaml {
            std::fs::write(&config_path, yaml).unwrap();
        } else if !config_path.exists() {
            std::fs::write(&config_path, "logging-to-file: false\n").unwrap();
        }
        let mut state = AppState::new(&GatewayConfig {
            auth_dir: self.dir.clone(),
            config_path,
            auth_refresh_enabled: false,
            max_failover: 3,
            strategy: Strategy::StrictRoundRobin,
            usage_poll_secs: 120,
            port: 0,
            ..GatewayConfig::default()
        })
        .expect("construct AppState");

        state.claude_direct_client = make_direct_client(upstream_addr);
        state.http_client = make_direct_client(upstream_addr);
        Arc::new(state)
    }
}

fn write_claude(dir: &Path, id: &str, upstream: Option<&str>, api_key: Option<&str>) {
    let mut credential = serde_json::json!({
        "type": "claude",
        "identity_slug": id,
        "email": format!("{id}@example.test"),
        "access_token": format!("token-{id}"),
        "refresh_token": format!("refresh-{id}"),
        "expired": "2099-01-01T00:00:00Z",
    });
    if let Some(u) = upstream {
        credential["upstream_override"] = serde_json::Value::String(u.to_string());
    }
    if let Some(k) = api_key {
        credential["api_key"] = serde_json::Value::String(k.to_string());
    }
    std::fs::write(dir.join(format!("claude-{id}.json")), credential.to_string()).unwrap();
}

async fn send_request(
    state: &Arc<AppState>,
    headers: &[(&str, &str)],
    body: serde_json::Value,
) -> (StatusCode, HeaderMap, axum::body::Bytes) {
    let mut request = Request::post("/v1/messages").header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        create_app(Arc::clone(state)).oneshot(request.body(Body::from(body.to_string())).unwrap()),
    )
    .await
    .expect("gateway response within 5s")
    .expect("oneshot response");
    let status = response.status();
    let resp_headers = response.headers().clone();
    let bytes = tokio::time::timeout(Duration::from_secs(5), response.into_body().collect())
        .await
        .expect("collect within 5s")
        .expect("body bytes")
        .to_bytes();
    (status, resp_headers, bytes)
}

#[tokio::test]
async fn test_direct_oauth_classification_and_2_1_316_profile() {
    let mut fx = Fixture::new("claude-direct-profile");
    let (addr, upstream) = fx.tls_upstream().await;
    fx.oauth_direct("alpha");
    let state = fx.state(addr);

    let req_body = serde_json::json!({
        "model": MODEL,
        "max_tokens": 32,
        "messages": [{"role": "user", "content": "ping"}],
    });

    let (status, _, body) = send_request(&state, &[], req_body).await;
    assert_eq!(status, StatusCode::OK, "response body: {:?}", String::from_utf8_lossy(&body));
    assert_eq!(upstream.hits(), 1);

    let (headers, body_json) = upstream.last();

    let ua = headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or_default();
    assert!(
        ua.contains("2.1.316") && ua.contains("sdk-cli"),
        "expected 2.1.316 sdk-cli UA, got: {ua}"
    );

    let system = body_json.get("system").and_then(|s| s.as_array()).expect("system array");
    let billing_block = system.iter().find(|b| {
        b.get("text")
            .and_then(|t| t.as_str())
            .map_or(false, |t| t.starts_with("x-anthropic-billing-header:"))
    });
    assert!(billing_block.is_some(), "missing x-anthropic-billing-header in system: {system:?}");
    let billing_text = billing_block.unwrap()["text"].as_str().unwrap();
    assert!(billing_text.contains("cc_entrypoint=sdk-cli;"), "entrypoint in: {billing_text}");
    assert!(billing_text.contains("cc_version=2.1.316"), "version in: {billing_text}");
    assert!(billing_text.contains("cch="), "cch in: {billing_text}");

    let user_id_raw = body_json
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|u| u.as_str())
        .expect("metadata.user_id string");
    let user_id_obj: serde_json::Value = serde_json::from_str(user_id_raw).expect("valid user_id json");
    let device_id = user_id_obj["device_id"].as_str().expect("device_id string");
    assert!(!device_id.is_empty(), "device_id must not be empty");
    let session_id = user_id_obj["session_id"].as_str().expect("session_id string");
    assert!(!session_id.is_empty(), "session_id must not be empty");

    let req_body_2 = serde_json::json!({
        "model": MODEL,
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "ping 2"}],
    });
    let (status2, _, _) = send_request(&state, &[], req_body_2).await;
    assert_eq!(status2, StatusCode::OK);
    assert_eq!(upstream.hits(), 2);
    let (_, body_json_2) = upstream.last();
    let user_id_raw_2 = body_json_2["metadata"]["user_id"].as_str().unwrap();
    let user_id_obj_2: serde_json::Value = serde_json::from_str(user_id_raw_2).unwrap();
    assert_eq!(user_id_obj_2["device_id"].as_str().unwrap(), device_id, "device_id must be persistent");
}

#[tokio::test]
async fn test_negative_controls_no_direct_transformation() {
    let mut fx = Fixture::new("claude-negative-controls");
    let (addr, upstream) = fx.tls_upstream().await;

    fx.api_key("keyed");
    let state = fx.state(addr);
    let (status, _, _) = send_request(
        &state,
        &[],
        serde_json::json!({
            "model": MODEL,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "ping"}],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (headers, body_json) = upstream.last();
    assert!(headers.contains_key("x-api-key"), "expected x-api-key header");
    let body_str = body_json.to_string();
    assert!(
        !body_str.contains("x-anthropic-billing-header"),
        "keyed account must not have direct billing header"
    );
    assert!(
        body_json.get("metadata").and_then(|m| m.get("user_id")).is_none(),
        "keyed account must not inject direct metadata user_id"
    );

    std::fs::remove_file(fx.dir.join("claude-keyed.json")).unwrap();
    fx.oauth_direct("uncloaked");
    let cloak_off_state = fx.state_with_config(
        addr,
        Some("logging-to-file: false\ndisable-claude-cloak-mode: true\n"),
    );
    let (status_cloak, _, _) = send_request(
        &cloak_off_state,
        &[("user-agent", "custom-client/1.0")],
        serde_json::json!({
            "model": MODEL,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "ping"}],
        }),
    )
    .await;
    assert_eq!(status_cloak, StatusCode::OK);
    let (headers_cloak, body_cloak) = upstream.last();
    assert_eq!(
        headers_cloak.get("user-agent").and_then(|v| v.to_str().ok()),
        Some("custom-client/1.0")
    );
    assert!(!body_cloak.to_string().contains("x-anthropic-billing-header"));
}

#[tokio::test]
async fn test_system_message_and_per_turn_preservation() {
    let mut fx = Fixture::new("claude-direct-system");
    let (addr, upstream) = fx.tls_upstream().await;
    fx.oauth_direct("alpha");
    let state = fx.state(addr);

    let req_body = serde_json::json!({
        "model": MODEL,
        "max_tokens": 32,
        "system": [{"type":"text","text":"CALLER_SYSTEM_SENTINEL","cache_control":{"type":"ephemeral"}}],
        "messages": [
            {"role": "user", "content": "first turn"},
            {"role": "system", "content": [{"type":"text","text":"TURN_SENTINEL"}], "output_config":{"effort":"low"}}
        ],
    });

    let (status, _, _) = send_request(&state, &[], req_body.clone()).await;
    assert_eq!(status, StatusCode::OK);

    let (_, body_json) = upstream.last();
    let blocks = body_json["system"].as_array().unwrap();
    assert!(blocks.iter().any(|block| block["text"] == req_body["system"][0]["text"]));
    assert_eq!(body_json["messages"], req_body["messages"]);
}

#[tokio::test]
async fn test_native_tool_names_preservation_json() {
    let mut fx = Fixture::new("claude-direct-tools-json");
    let (addr, upstream) = fx.tls_upstream().await;

    let tool_call_reply = serde_json::json!({
        "id": "msg_tool_call",
        "type": "message",
        "role": "assistant",
        "model": MODEL,
        "content": [
            {
                "type": "tool_use",
                "id": "tu_call_1",
                "name": "mcp__workspace__lookup",
                "input": {"query": "file.rs"}
            },
            {
                "type": "tool_use",
                "id": "tu_call_2",
                "name": "custom_linter",
                "input": {"path": "src/"}
            }
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 15, "output_tokens": 25}
    });
    upstream.then(200, "application/json", tool_call_reply.to_string());

    fx.oauth_direct("alpha");
    let state = fx.state(addr);

    let req_body = serde_json::json!({
        "model": MODEL,
        "max_tokens": 64,
        "tools": [
            {
                "name": "mcp__workspace__lookup",
                "description": "Look up symbol",
                "input_schema": {"type": "object", "properties": {"query": {"type": "string"}}}
            },
            {
                "name": "custom_linter",
                "description": "Run custom linter",
                "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}
            },
            {
                "name": "Read",
                "description": "Read file",
                "input_schema": {"type": "object", "properties": {"file": {"type": "string"}}}
            }
        ],
        "messages": [{"role": "user", "content": "examine code"}],
    });

    let (status, _, resp_bytes) = send_request(&state, &[], req_body).await;
    assert_eq!(status, StatusCode::OK);

    let (_, wire_body) = upstream.last();
    let wire_tools = wire_body["tools"].as_array().expect("tools array on wire");
    let tool_names: Vec<&str> = wire_tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();
    assert!(tool_names.contains(&"mcp__workspace__lookup"), "missing mcp tool on wire: {tool_names:?}");
    assert!(tool_names.contains(&"custom_linter"), "missing custom tool on wire: {tool_names:?}");
    assert!(tool_names.contains(&"Read"), "missing Read tool on wire: {tool_names:?}");

    let resp_json: serde_json::Value = serde_json::from_slice(&resp_bytes).expect("valid response json");
    let content = resp_json["content"].as_array().expect("content blocks");
    let resp_tool_names: Vec<&str> = content
        .iter()
        .filter_map(|c| c.get("name").and_then(|n| n.as_str()))
        .collect();
    assert_eq!(resp_tool_names, vec!["mcp__workspace__lookup", "custom_linter"]);
}

#[tokio::test]
async fn test_native_tool_result_continuation() {
    let mut fx = Fixture::new("claude-direct-tool-result");
    let (addr, upstream) = fx.tls_upstream().await;
    fx.oauth_direct("alpha");
    let state = fx.state(addr);

    let req_body = serde_json::json!({
        "model": MODEL,
        "max_tokens": 64,
        "messages": [
            {"role": "user", "content": "read file"},
            {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "reading"},
                    {
                        "type": "tool_use",
                        "id": "tu_read_99",
                        "name": "mcp__workspace__lookup",
                        "input": {"query": "main.rs"}
                    }
                ]
            },
            {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "tu_read_99",
                        "content": "fn main() {}"
                    }
                ]
            }
        ],
    });

    let (status, _, _) = send_request(&state, &[], req_body).await;
    assert_eq!(status, StatusCode::OK);

    let (_, wire_body) = upstream.last();
    let wire_messages = wire_body["messages"].as_array().expect("messages on wire");
    assert_eq!(wire_messages.len(), 3, "must have all 3 turns preserved");
    let user_turn_3 = &wire_messages[2];
    let content_turn_3 = user_turn_3["content"].as_array().expect("array content in turn 3");
    let tool_res = content_turn_3.iter().find(|c| c["type"] == "tool_result").expect("tool_result block");
    assert_eq!(tool_res["tool_use_id"], "tu_read_99");
    assert_eq!(tool_res["content"], "fn main() {}");
}

#[tokio::test]
async fn test_native_sse_passthrough_unknown_events_and_fields() {
    let mut fx = Fixture::new("claude-direct-sse-passthrough");
    let (addr, upstream) = fx.tls_upstream().await;

    let stream_wire = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_sse_1\",\"model\":\"claude-sonnet-4-5-20250929\",\"usage\":{\"input_tokens\":12}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"future_field\":true,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_sse_1\",\"name\":\"mcp__workspace__lookup\",\"input\":{}}}\n\n",
        "event: future_opaque_event\n",
        "data: {\"type\":\"future_opaque_event\",\"future_val\":999}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":8}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    upstream.then(200, "text/event-stream", stream_wire);

    fx.oauth_direct("alpha");
    let state = fx.state(addr);

    let req_body = serde_json::json!({
        "model": MODEL,
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role": "user", "content": "stream ping"}],
    });

    let (status, resp_headers, resp_bytes) = send_request(&state, &[], req_body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        resp_headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );

    let (upstream_headers, _) = upstream.last();
    assert_eq!(
        upstream_headers.get("accept").and_then(|v| v.to_str().ok()),
        Some("application/json"),
        "direct Anthropic request must send accept: application/json upstream"
    );

    assert_eq!(
        resp_bytes.as_ref(),
        stream_wire.as_bytes(),
        "native Anthropic SSE stream must pass through byte-for-byte"
    );
}

#[tokio::test]
async fn test_session_affinity_and_no_affinity_isolation() {
    let mut fx = Fixture::new("claude-direct-sessions");
    let (addr, upstream) = fx.tls_upstream().await;
    fx.oauth_direct("alpha");
    let state = fx.state(addr);

    let (status1, _, _) = send_request(
        &state,
        &[("x-claude-code-session-id", "sess-A")],
        serde_json::json!({
            "model": MODEL,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "turn 1"}],
        }),
    )
    .await;
    assert_eq!(status1, StatusCode::OK);
    let (h1, b1) = upstream.last();
    let wire_sess_1 = h1.get("x-claude-code-session-id").and_then(|v| v.to_str().ok()).unwrap();
    let meta_user_1: serde_json::Value = serde_json::from_str(b1["metadata"]["user_id"].as_str().unwrap()).unwrap();
    assert_eq!(meta_user_1["session_id"].as_str().unwrap(), wire_sess_1);

    let (status2, _, _) = send_request(
        &state,
        &[("x-claude-code-session-id", "sess-A")],
        serde_json::json!({
            "model": MODEL,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "turn 2"}],
        }),
    )
    .await;
    assert_eq!(status2, StatusCode::OK);
    let (h2, _) = upstream.last();
    let wire_sess_2 = h2.get("x-claude-code-session-id").and_then(|v| v.to_str().ok()).unwrap();
    assert_eq!(wire_sess_1, wire_sess_2, "same affinity must produce stable session ID");

    let (status3, _, _) = send_request(
        &state,
        &[("x-claude-code-session-id", "sess-B")],
        serde_json::json!({
            "model": MODEL,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "turn 3"}],
        }),
    )
    .await;
    assert_eq!(status3, StatusCode::OK);
    let (h3, _) = upstream.last();
    let wire_sess_3 = h3.get("x-claude-code-session-id").and_then(|v| v.to_str().ok()).unwrap();
    assert_ne!(wire_sess_1, wire_sess_3, "distinct affinity sessions must be isolated");

    let (status4, _, _) = send_request(
        &state,
        &[],
        serde_json::json!({
            "model": MODEL,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "no affinity"}],
        }),
    )
    .await;
    assert_eq!(status4, StatusCode::OK);
    let (h4, b4) = upstream.last();
    let wire_sess_4 = h4.get("x-claude-code-session-id").and_then(|v| v.to_str().ok()).unwrap();
    assert_ne!(wire_sess_4, wire_sess_1);
    assert_ne!(wire_sess_4, wire_sess_3);
    let meta_user_4: serde_json::Value = serde_json::from_str(b4["metadata"]["user_id"].as_str().unwrap()).unwrap();
    assert_eq!(meta_user_4["session_id"].as_str().unwrap(), wire_sess_4);
    let (status5, _, _) = send_request(&state, &[], serde_json::json!({
        "model": MODEL, "max_tokens":16, "messages":[{"role":"user","content":"no affinity"}]
    })).await;
    assert_eq!(status5, StatusCode::OK);
    let (h5, _) = upstream.last();
    assert_ne!(h5["x-claude-code-session-id"], h4["x-claude-code-session-id"]);
}

#[tokio::test]
async fn test_cch_field_is_present_on_each_signed_request() {
    let mut fx = Fixture::new("claude-direct-cch");
    let (addr, upstream) = fx.tls_upstream().await;
    fx.oauth_direct("alpha");
    let state = fx.state(addr);

    let req_body = serde_json::json!({
        "model": MODEL,
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "reproducible prompt"}],
    });

    let (s1, _, _) = send_request(&state, &[("x-claude-code-session-id", "fixed-sess")], req_body.clone()).await;
    assert_eq!(s1, StatusCode::OK);
    let (_, b1) = upstream.last();

    let (s2, _, _) = send_request(&state, &[("x-claude-code-session-id", "fixed-sess")], req_body).await;
    assert_eq!(s2, StatusCode::OK);
    let (_, b2) = upstream.last();

    let extract_cch = |body: &serde_json::Value| -> String {
        let system = body["system"].as_array().expect("system array");
        let billing = system.iter().find(|b| {
            b.get("text")
                .and_then(|t| t.as_str())
                .map_or(false, |t| t.starts_with("x-anthropic-billing-header:"))
        }).expect("billing block")["text"].as_str().unwrap();
        let cch_part = billing.split(';').find(|part| part.trim().starts_with("cch=")).expect("cch field");
        cch_part.trim().to_string()
    };

    let cch1 = extract_cch(&b1);
    let cch2 = extract_cch(&b2);
    for cch in [cch1, cch2] {
        let value = cch.strip_prefix("cch=").unwrap();
        assert_eq!(value.len(), 5);
        assert!(value.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
