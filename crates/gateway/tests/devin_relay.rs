mod common;

#[path = "devin_regressions/relay_target.rs"]
mod relay_target;

use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{header, HeaderMap, StatusCode, Version};
use axum::routing::post;
use axum::Router;
use prost::Message;
use serde_json::json;

use mahoquot_gateway::compat::devin::{
    authorization_header, frame_data, frame_end_stream, STREAM_CONTENT_TYPE,
};
use mahoquot_gateway::compat::devin_proto::{ChatMessageResponse, GetChatMessageRequest};
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::runtime_state::PoolSnapshot;
use mahoquot_gateway::state::AppState;
use mahoquot_types::{Health, PoolMember};
use mahoquot_registry::{
    AuthorityMask, CatalogSource, CatalogVersion, DiscoveredModel, ModelCapability, ModelId,
    ProviderContribution, ProviderId, ProviderPolicy, RegistryBuilder,
};

static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

// ─── Process-based Mock Harness with Bounded Readiness & RAII ────────────────

struct ChildGuard {
    child: Option<Child>,
    stderr_thread: Option<std::thread::JoinHandle<String>>,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(th) = self.stderr_thread.take() {
            let _ = th.join();
        }
    }
}

struct DevinMockProcess {
    _guard: ChildGuard,
    pub url: String,
}

impl DevinMockProcess {
    fn start(scenario: &str) -> Self {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let project_root = std::path::Path::new(manifest_dir)
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let script_path = project_root.join("scripts/devin-mock.mjs");
        let mut child = Command::new("bun")
            .arg(&script_path)
            .arg("--port")
            .arg("0")
            .arg("--scenario")
            .arg(scenario)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn bun scripts/devin-mock.mjs");

        let stdout = child.stdout.take().expect("failed to get stdout");
        let mut stderr = child.stderr.take().expect("failed to get stderr");
        let stderr_thread = std::thread::spawn(move || {
            use std::io::Read;
            let mut err_msg = String::new();
            let _ = stderr.read_to_string(&mut err_msg);
            err_msg
        });

        let guard = ChildGuard {
            child: Some(child),
            stderr_thread: Some(stderr_thread),
        };

        // Bounded readiness reading with 5-second timeout to avoid pipe deadlock
        let (tx, rx) = std::sync::mpsc::channel();
        let read_handle = std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            let mut line = String::new();
            let res = reader.read_line(&mut line).map(|_| line);
            let _ = tx.send(res);
        });

        let line_res = match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(res) => {
                let _ = read_handle.join();
                res.expect("must read line from mock stdout")
            }
            Err(_) => {
                drop(guard);
                let _ = read_handle.join();
                panic!("timed out waiting for devin-mock readiness");
            }
        };

        let parsed: serde_json::Value = serde_json::from_str(&line_res)
            .unwrap_or_else(|e| panic!("failed to parse mock readiness JSON '{line_res}': {e}"));
        let url = parsed["url"].as_str().expect("url must be string").to_string();

        Self {
            _guard: guard,
            url,
        }
    }

    async fn get_state(&self) -> serde_json::Value {
        let client = reqwest::Client::new();
        let res = client
            .get(format!("{}/__control/state", self.url))
            .send()
            .await
            .expect("failed to get mock control state");
        res.json().await.expect("state must be JSON")
    }
}

// ─── Gateway Server Setup Helper ─────────────────────────────────────────────

fn register_devin_models(state: &AppState, models: &[&str]) {
    let mut builder = RegistryBuilder::new(CatalogVersion::new(1), CatalogSource::Discovered);
    builder.register_provider(ProviderId::devin(), ProviderPolicy::Discovered);

    let discovered_registry: Vec<DiscoveredModel> = models
        .iter()
        .map(|model_name| {
            let full_id = if model_name.starts_with("devin/") {
                model_name.to_string()
            } else {
                format!("devin/{model_name}")
            };
            DiscoveredModel::new(ModelId::new(&full_id).unwrap())
                .with_capabilities([ModelCapability::Chat, ModelCapability::Tools])
                .with_context_limit(200_000)
                .with_authority(AuthorityMask::MODELS_ONLY)
        })
        .collect();

    builder
        .apply_contribution(
            ProviderContribution::from_discovered_models(ProviderId::devin(), discovered_registry)
                .with_policy(ProviderPolicy::Discovered),
        )
        .unwrap();

    let snapshot = builder.build().unwrap();
    snapshot.validate().unwrap();

    let current = state.pool.load();
    let discovered_account: Vec<mahoquot_gateway::devin_catalog::DiscoveredDevinModel> = models
        .iter()
        .map(|model_name| {
            let uid = model_name
                .strip_prefix("devin/")
                .unwrap_or(model_name)
                .to_string();
            let full_id = format!("devin/{uid}");
            let supports_images = uid.starts_with("glm-") || uid.starts_with("swe-");
            mahoquot_gateway::devin_catalog::DiscoveredDevinModel {
                model_uid: uid.clone(),
                public_id: full_id,
                label: uid,
                supports_images,
                is_premium: false,
                is_beta: false,
                is_recommended: false,
                is_new: false,
                is_capacity_limited: false,
                promo_active: false,
                max_tokens: Some(200_000),
                credit_multiplier: Some(1.0),
                description: None,
            }
        })
        .collect();

    for member in &current.members {
        if member.kind() == mahoquot_gateway::account::ProviderKind::Devin {
            let key = mahoquot_gateway::devin_catalog::DevinCacheKey::new(
                &member.id,
                &member.access_token(),
                member.effective_base_url(),
            );
            let cat_state = mahoquot_gateway::devin_catalog::DevinAccountCatalogState::new_success(
                key,
                discovered_account.clone(),
                100,
            );
            member.set_devin_catalog_state(Arc::new(cat_state));
        }
    }

    let mut new_models = current.models.clone();
    for m in models {
        let full_id = if m.starts_with("devin/") {
            m.to_string()
        } else {
            format!("devin/{m}")
        };
        if !new_models.iter().any(|e| e.id == full_id) {
            new_models.push(mahoquot_gateway::models_route::ModelEntry {
                id: full_id,
                owned_by: "devin".to_string(),
            });
        }
    }

    let new_pool = PoolSnapshot::new(
        current.generation + 1,
        current.members.clone(),
        new_models,
        Arc::new(snapshot),
    );
    state.pool.store(Arc::new(new_pool));
}

fn devin_credential(identity: &str, token: &str, upstream_url: &str) -> String {
    serde_json::to_string(&json!({
        "type": "devin",
        "identity_slug": identity,
        "label": format!("Devin {}", identity),
        "access_token": token,
        "api_server_url": upstream_url,
        "upstream_override": upstream_url,
        "disabled": false
    }))
    .unwrap()
}

struct TestGateway {
    pub base_url: String,
    pub auth_dir: PathBuf,
    pub config_path: PathBuf,
    pub state: Arc<AppState>,
    server_task: tokio::task::JoinHandle<()>,
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.server_task.abort();
        let _ = std::fs::remove_dir_all(&self.auth_dir);
    }
}

async fn start_devin_gateway(identity: &str, token: &str, upstream_url: &str) -> TestGateway {
    start_devin_gateway_with_accounts(&[(identity, token, upstream_url)]).await
}

async fn start_devin_gateway_with_accounts(accounts: &[(&str, &str, &str)]) -> TestGateway {
    let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let auth_dir = common::unique_temp_dir(&format!("devin-relay-test-{seq}"));
    let config_path = auth_dir.join("config.yaml");

    for &(identity, token, upstream_url) in accounts {
        let cred_file = auth_dir.join(format!("devin-{identity}.json"));
        std::fs::write(&cred_file, devin_credential(identity, token, upstream_url)).unwrap();
    }

    let config = GatewayConfig {
        auth_dir: auth_dir.clone(),
        api_keys: mahoquot_gateway::inbound::ApiKeys::from_env_value("test-inbound-key"),
        auth_refresh_enabled: false,
        max_failover: 3,
        config_path: config_path.clone(),
        ..GatewayConfig::default()
    };

    let state = Arc::new(AppState::new(&config).unwrap());
    register_devin_models(&state, &["glm-5-2", "swe-1-7"]);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = create_app(Arc::clone(&state));
    let server_task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    TestGateway {
        base_url: format!("http://{addr}"),
        auth_dir,
        config_path,
        state,
        server_task,
    }
}

// ─── Test 1: Wire Contract Equality (Headers, Protocol, Protobuf, Version) ───

#[derive(Clone, Debug)]
struct CapturedWire {
    version: Version,
    headers: HeaderMap,
    body: Vec<u8>,
}

#[tokio::test]
async fn test_devin_relay_wire_contract_assertions() {
    let captured = Arc::new(Mutex::new(None));
    let captured_clone = Arc::clone(&captured);

    let mock_app = Router::new().fallback(post(
        move |req: axum::extract::Request| {
            let captured = Arc::clone(&captured_clone);
            async move {
                let version = req.version();
                let headers = req.headers().clone();
                let bytes = axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024)
                    .await
                    .unwrap()
                    .to_vec();

                *captured.lock().unwrap() = Some(CapturedWire {
                    version,
                    headers,
                    body: bytes,
                });

                let f1 = frame_data(b"");
                let end = frame_end_stream(r#"{"error":null,"hasSyncPoints":false}"#);
                let mut wire = f1;
                wire.extend_from_slice(&end);

                (
                    StatusCode::OK,
                    [
                        (header::CONTENT_TYPE, STREAM_CONTENT_TYPE),
                        (header::HeaderName::from_static("connect-protocol-version"), "1"),
                    ],
                    wire,
                )
            }
        },
    ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = listener.local_addr().unwrap();
    let mock_task = tokio::spawn(async move {
        axum::serve(listener, mock_app).await.unwrap();
    });

    let mock_url = format!("http://{mock_addr}");
    let test_token = "devin-session-token-secret123";
    let gw = start_devin_gateway("work", test_token, &mock_url).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/chat/completions", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .header(header::CONTENT_TYPE, "application/json")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "Hello Devin"}],
            "stream": true
        }))
        .send()
        .await
        .expect("gateway request should succeed");

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.unwrap();
    assert!(body.contains("[DONE]"), "stream must close with [DONE]");

    let wire = captured.lock().unwrap().take().expect("mock must receive request");
    mock_task.abort();

    // 1. Assert HTTP/1.1
    assert_eq!(wire.version, Version::HTTP_11, "upstream request must use HTTP/1.1");

    // 2. Assert literal Authorization header: Basic <token>-<token>
    let auth = wire
        .headers
        .get(header::AUTHORIZATION)
        .expect("must have authorization header")
        .to_str()
        .unwrap();
    assert_eq!(
        auth,
        format!("Basic {test_token}-{test_token}"),
        "must match literal Basic <token>-<token>"
    );
    assert_eq!(auth, authorization_header(test_token));

    // 3. Assert exactly one Content-Type header matching application/connect+proto
    let content_types: Vec<_> = wire.headers.get_all(header::CONTENT_TYPE).iter().collect();
    assert_eq!(
        content_types.len(),
        1,
        "must have exactly one Content-Type header, got: {:?}",
        content_types
    );
    assert_eq!(
        content_types[0], STREAM_CONTENT_TYPE,
        "content-type must be application/connect+proto"
    );

    // 4. Assert Accept header
    let accept = wire.headers.get(header::ACCEPT).and_then(|v| v.to_str().ok());
    assert_eq!(
        accept,
        Some(STREAM_CONTENT_TYPE),
        "accept header must be application/connect+proto"
    );

    // 5. Assert Connect-Protocol-Version: 1
    assert_eq!(
        wire.headers.get("connect-protocol-version").and_then(|v| v.to_str().ok()),
        Some("1")
    );

    // 6. Assert Connect envelope framing: flag 0x00 and 4-byte BE length
    assert!(wire.body.len() >= 5, "body must contain 5-byte Connect header");
    assert_eq!(wire.body[0], 0x00, "flag must be 0x00 (data frame)");
    let payload_len = u32::from_be_bytes(wire.body[1..5].try_into().unwrap()) as usize;
    assert_eq!(wire.body.len(), 5 + payload_len, "exact declared payload length");

    // 7. Parse protobuf and assert metadata.api_key equals token
    let proto_bytes = &wire.body[5..5 + payload_len];
    let req = GetChatMessageRequest::decode(proto_bytes).expect("must decode GetChatMessageRequest");
    let meta = req.metadata.expect("metadata must be present");
    assert_eq!(
        meta.api_key.as_deref(),
        Some(test_token),
        "protobuf metadata.api_key must match token exactly"
    );

    // 8. Assert chat_model_uid is stripped of "devin/" prefix
    assert_eq!(
        req.chat_model_uid.as_deref(),
        Some("glm-5-2"),
        "chat_model_uid must be 'glm-5-2' without 'devin/' prefix"
    );
}

// ─── Test 2: No Auth Redirect to Untrusted Origins ───────────────────────────

#[tokio::test]
async fn test_devin_relay_no_auth_redirect() {
    let second_captured_auth = Arc::new(Mutex::new(None));
    let second_captured_clone = Arc::clone(&second_captured_auth);

    // Secondary server representing redirect target
    let second_app = Router::new().fallback(post(move |req: axum::extract::Request| {
        let second_captured = Arc::clone(&second_captured_clone);
        async move {
            let auth = req
                .headers()
                .get(header::AUTHORIZATION)
                .map(|h| h.to_str().unwrap().to_string());
            *second_captured.lock().unwrap() = auth;
            (StatusCode::OK, "redirect landing")
        }
    }));
    let second_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let second_addr = second_listener.local_addr().unwrap();
    let second_url = format!("http://{second_addr}/redirected");
    let second_task = tokio::spawn(async move {
        axum::serve(second_listener, second_app).await.unwrap();
    });

    // Primary mock returning 307 redirect
    let redirect_target = second_url.clone();
    let primary_app = Router::new().fallback(post(move || {
        let target = redirect_target.clone();
        async move {
            (
                StatusCode::TEMPORARY_REDIRECT,
                [(header::LOCATION, target)],
                "redirecting",
            )
        }
    }));
    let primary_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let primary_addr = primary_listener.local_addr().unwrap();
    let primary_task = tokio::spawn(async move {
        axum::serve(primary_listener, primary_app).await.unwrap();
    });

    let primary_url = format!("http://{primary_addr}");
    let test_token = "devin-secret-token-no-leak";
    let gw = start_devin_gateway("work", test_token, &primary_url).await;

    let client = reqwest::Client::new();
    let _resp = client
        .post(format!("{}/v1/chat/completions", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false
        }))
        .send()
        .await
        .expect("request should complete");

    let leaked_auth = second_captured_auth.lock().unwrap().take();
    assert!(
        leaked_auth.is_none(),
        "Authorization header must NEVER be leaked to a redirected origin, got: {:?}",
        leaked_auth
    );

    primary_task.abort();
    second_task.abort();
}

// ─── Test 3: Streaming Chat Completion Success with Mock Harness ─────────────

#[tokio::test]
async fn test_devin_relay_streaming_success() {
    let mock = DevinMockProcess::start("default");
    let test_token = "devin-session-token-stream-success";
    let gw = start_devin_gateway("stream-acc", test_token, &mock.url).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/chat/completions", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "Write a greeting"}],
            "stream": true
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );

    let text = resp.text().await.unwrap();
    assert!(text.contains("data: "), "must contain SSE data frames");
    assert!(text.contains("Hello, I am Devin."), "must contain text delta from mock");
    assert!(text.contains("[DONE]"), "must terminate with [DONE]");

    let state = mock.get_state().await;
    assert_eq!(state["request_count"].as_u64(), Some(1));
}

// ─── Test 4: Non-Streaming Chat Completion Success ───────────────────────────

#[tokio::test]
async fn test_devin_relay_non_streaming_success() {
    let mock = DevinMockProcess::start("default");
    let test_token = "devin-session-token-non-stream";
    let gw = start_devin_gateway("non-stream-acc", test_token, &mock.url).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/chat/completions", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "Hello Devin"}],
            "stream": false
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()),
        Some("application/json")
    );

    let val: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(val["object"], "chat.completion");
    let content = val["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(content.contains("Hello, I am Devin."));
    assert_eq!(val["choices"][0]["finish_reason"], "stop");
}

// ─── Test 5: HTTP 200 Connect Error Code Mapping ─────────────────────────────

#[tokio::test]
async fn test_devin_relay_http_200_connect_error_code_mapping() {
    let mock = DevinMockProcess::start("code-only-error");
    let test_token = "devin-token-code-err";
    let gw = start_devin_gateway("err-acc", test_token, &mock.url).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/chat/completions", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "Trigger quota"}],
            "stream": false
        }))
        .send()
        .await
        .expect("request should finish");

    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "HTTP 200 resource_exhausted must map to HTTP 429"
    );

    let err_body: serde_json::Value = resp.json().await.unwrap();
    let err_msg = err_body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        err_msg.contains("quota or rate limit exhausted"),
        "error message must be safe fixed description, got: '{err_msg}'"
    );
}

// ─── Test 6: Fragmented Code-Only Error Yields Same HTTP 429 as Unsplit ──────

#[tokio::test]
async fn test_devin_relay_fragmented_code_only_error_matches_unsplit() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let mock_app = Router::new().fallback(post(|| async {
        let frame = frame_end_stream(r#"{"error":{"code":"resource_exhausted"}}"#);
        let chunks: Vec<bytes::Bytes> = frame
            .chunks(2)
            .map(bytes::Bytes::copy_from_slice)
            .collect();
        let stream = futures::stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>));

        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, STREAM_CONTENT_TYPE),
                (header::HeaderName::from_static("connect-protocol-version"), "1"),
            ],
            axum::body::Body::from_stream(stream),
        )
    }));

    let mock_task = tokio::spawn(async move {
        axum::serve(listener, mock_app).await.unwrap();
    });

    let mock_url = format!("http://{addr}");
    let gw = start_devin_gateway("frag-acc", "tok-frag", &mock_url).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/chat/completions", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "Trigger fragmented error"}],
            "stream": false
        }))
        .send()
        .await
        .expect("request should finish");

    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "fragmented HTTP 200 resource_exhausted must map to HTTP 429"
    );

    let err_body: serde_json::Value = resp.json().await.unwrap();
    let err_msg = err_body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        err_msg.contains("quota or rate limit exhausted"),
        "error message must match safe fixed description, got: '{err_msg}'"
    );

    mock_task.abort();
}

// ─── Test 7: Late Error Under Coalesced vs Split Transport ───────────────────

#[tokio::test]
async fn test_devin_relay_late_error_no_success_terminal() {
    // We test BOTH transport chunkings (Split and Coalesced) to assert identical semantics!

    // 1. Split Transport: Frame 1 is yielded, downstream receives text delta, then Frame 2 is yielded
    let (chunk2_tx, chunk2_rx) = tokio::sync::oneshot::channel::<()>();
    let chunk2_rx = Arc::new(tokio::sync::Mutex::new(Some(chunk2_rx)));
    let chunk2_rx_clone = Arc::clone(&chunk2_rx);

    let listener_split = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr_split = listener_split.local_addr().unwrap();

    let mock_app_split = Router::new().fallback(post(move || {
        let rx_holder = Arc::clone(&chunk2_rx_clone);
        async move {
            let msg = ChatMessageResponse {
                delta_text: Some("Starting output before late error...".to_string()),
                ..Default::default()
            };
            let mut f1_payload = Vec::new();
            msg.encode(&mut f1_payload).unwrap();
            let f1 = frame_data(&f1_payload);
            let f2 = frame_end_stream(r#"{"error":{"code":"internal","message":"stream crashed"}}"#);

            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            // Emit Frame 1 first
            let _ = tx.send(Ok::<_, std::io::Error>(bytes::Bytes::from(f1)));

            tokio::spawn(async move {
                // Wait until downstream actually receives Frame 1
                if let Some(rx) = rx_holder.lock().await.take() {
                    let _ = rx.await;
                }
                // Emit Frame 2 (terminal error)
                let _ = tx.send(Ok(bytes::Bytes::from(f2)));
            });

            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, STREAM_CONTENT_TYPE),
                    (header::HeaderName::from_static("connect-protocol-version"), "1"),
                ],
                axum::body::Body::from_stream(futures::stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|chunk| (chunk, rx))
                })),
            )
        }
    }));

    let task_split = tokio::spawn(async move { axum::serve(listener_split, mock_app_split).await.unwrap() });
    let gw_split = start_devin_gateway("late-split", "tok-late-1", &format!("http://{addr_split}")).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/chat/completions", gw_split.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "test late split"}],
            "stream": true
        }))
        .send()
        .await
        .expect("request should initiate");

    assert_eq!(resp.status(), StatusCode::OK);
    let mut stream = resp.bytes_stream();

    use futures::StreamExt;
    let mut initial_text = String::new();
    while let Some(chunk) = stream.next().await {
        let text = String::from_utf8_lossy(&chunk.unwrap()).to_string();
        initial_text.push_str(&text);
        if initial_text.contains("Starting output") {
            break;
        }
    }
    assert!(
        initial_text.contains("Starting output"),
        "downstream must receive first text delta, got: {initial_text}"
    );

    // Release upstream to emit terminal error Chunk 2
    let _ = chunk2_tx.send(());

    // Read remaining stream
    let mut remaining = String::new();
    while let Some(chunk) = stream.next().await {
        remaining.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
    }

    assert!(
        remaining.contains(r#""type":"upstream_error""#),
        "stream must emit error frame: {remaining}"
    );
    assert!(
        !remaining.contains(r#""finish_reason":"stop""#),
        "late-error must NOT emit success finish_reason 'stop'"
    );
    assert!(remaining.ends_with("data: [DONE]\n\n"), "must end with [DONE]");

    task_split.abort();

    // 2. Coalesced Transport: Both Frame 1 and Frame 2 arrive in the exact same single transport chunk
    let listener_coal = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr_coal = listener_coal.local_addr().unwrap();

    let mock_app_coal = Router::new().fallback(post(|| async {
        let msg = ChatMessageResponse {
            delta_text: Some("Coalesced output...".to_string()),
            ..Default::default()
        };
        let mut f1_payload = Vec::new();
        msg.encode(&mut f1_payload).unwrap();
        let f1 = frame_data(&f1_payload);
        let f2 = frame_end_stream(r#"{"error":{"code":"internal","message":"stream crashed"}}"#);

        let mut combined = f1;
        combined.extend_from_slice(&f2);

        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, STREAM_CONTENT_TYPE),
                (header::HeaderName::from_static("connect-protocol-version"), "1"),
            ],
            combined,
        )
    }));

    let task_coal = tokio::spawn(async move { axum::serve(listener_coal, mock_app_coal).await.unwrap() });
    let gw_coal = start_devin_gateway("late-coal", "tok-late-2", &format!("http://{addr_coal}")).await;

    let resp_coal = client
        .post(format!("{}/v1/chat/completions", gw_coal.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "test late coal"}],
            "stream": true
        }))
        .send()
        .await
        .expect("request should initiate");

    // Chunk-invariant preflight extracts ONLY Frame 1, preserving Frame 2 for stream processing:
    assert_eq!(resp_coal.status(), StatusCode::OK);
    let coal_text = resp_coal.text().await.unwrap();

    assert!(coal_text.contains("Coalesced output"), "must deliver text delta");
    assert!(coal_text.contains(r#""type":"upstream_error""#), "must deliver error frame");
    assert!(!coal_text.contains(r#""finish_reason":"stop""#), "must NOT deliver finish_reason 'stop'");
    assert!(coal_text.ends_with("data: [DONE]\n\n"), "must end with [DONE]");

    task_coal.abort();
}

// ─── Test 8: Missing Usage vs Zero ───────────────────────────────────────────

#[tokio::test]
async fn test_devin_relay_missing_usage_vs_zero() {
    let mock_no_usage = DevinMockProcess::start("default");
    let gw1 = start_devin_gateway("no-usage-acc", "tok-1", &mock_no_usage.url).await;

    let client = reqwest::Client::new();
    let resp1 = client
        .post(format!("{}/v1/chat/completions", gw1.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false
        }))
        .send()
        .await
        .unwrap();

    let val1: serde_json::Value = resp1.json().await.unwrap();
    let usage1 = &val1["usage"];
    assert!(
        usage1.is_null() || usage1["total_tokens"].as_u64().is_none(),
        "missing usage must NOT be fabricated into zeros: {:?}",
        usage1
    );

    let mock_with_usage = DevinMockProcess::start("usage");
    let gw2 = start_devin_gateway("usage-acc", "tok-2", &mock_with_usage.url).await;

    let resp2 = client
        .post(format!("{}/v1/chat/completions", gw2.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false
        }))
        .send()
        .await
        .unwrap();

    let val2: serde_json::Value = resp2.json().await.unwrap();
    assert_eq!(val2["usage"]["prompt_tokens"], 12);
    assert_eq!(val2["usage"]["completion_tokens"], 34);
    assert_eq!(val2["usage"]["total_tokens"], 46);
}

// ─── Test 9: No Retry on Ambiguous Precommit Failure Across Two Accounts ────

#[tokio::test]
async fn test_devin_relay_no_retry_on_ambiguous_precommit_failure() {
    let acc1_calls = Arc::new(AtomicU64::new(0));
    let acc1_calls_clone = Arc::clone(&acc1_calls);
    let acc2_calls = Arc::new(AtomicU64::new(0));
    let acc2_calls_clone = Arc::clone(&acc2_calls);

    // Acc1 mock: drops connection immediately upon receive (ambiguous connection error)
    let listener1 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr1 = listener1.local_addr().unwrap();
    let app1 = Router::new().fallback(post(move || {
        let count = Arc::clone(&acc1_calls_clone);
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            (StatusCode::INTERNAL_SERVER_ERROR, "connection reset")
        }
    }));
    let task1 = tokio::spawn(async move { axum::serve(listener1, app1).await.unwrap() });

    // Acc2 mock: should NOT be called if retry is disabled on ambiguous failures!
    let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr2 = listener2.local_addr().unwrap();
    let app2 = Router::new().fallback(post(move || {
        let count = Arc::clone(&acc2_calls_clone);
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            let f1 = frame_data(b"");
            let end = frame_end_stream(r#"{"error":null}"#);
            let mut wire = f1;
            wire.extend_from_slice(&end);
            (StatusCode::OK, [(header::CONTENT_TYPE, STREAM_CONTENT_TYPE)], wire)
        }
    }));
    let task2 = tokio::spawn(async move { axum::serve(listener2, app2).await.unwrap() });

    let url1 = format!("http://{addr1}");
    let url2 = format!("http://{addr2}");

    let gw = start_devin_gateway_with_accounts(&[
        ("acc-primary", "tok-1", &url1),
        ("acc-backup", "tok-2", &url2),
    ])
    .await;

    let client = reqwest::Client::new();
    let _resp = client
        .post(format!("{}/v1/chat/completions", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false
        }))
        .send()
        .await
        .unwrap();

    let total_calls = acc1_calls.load(Ordering::SeqCst) + acc2_calls.load(Ordering::SeqCst);
    assert_eq!(
        total_calls, 1,
        "exactly 1 account must be attempted; must NOT retry ambiguous failure across accounts"
    );

    task1.abort();
    task2.abort();
}

// ─── Test 10: No Retry After Downstream Commitment Across Two Accounts ───────

#[tokio::test]
async fn test_devin_relay_no_retry_after_downstream_commitment() {
    let acc1_calls = Arc::new(AtomicU64::new(0));
    let acc1_calls_clone = Arc::clone(&acc1_calls);
    let acc2_calls = Arc::new(AtomicU64::new(0));
    let acc2_calls_clone = Arc::clone(&acc2_calls);

    let (chunk2_tx, chunk2_rx) = tokio::sync::oneshot::channel::<()>();
    let chunk2_rx = Arc::new(tokio::sync::Mutex::new(Some(chunk2_rx)));
    let chunk2_rx_clone = Arc::clone(&chunk2_rx);

    // Acc1 mock: sends Chunk 1 (committing downstream), waits for reception, then emits Chunk 2 (error)
    let listener1 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr1 = listener1.local_addr().unwrap();
    let app1 = Router::new().fallback(post(move || {
        let count = Arc::clone(&acc1_calls_clone);
        let rx_holder = Arc::clone(&chunk2_rx_clone);
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            let msg = ChatMessageResponse {
                delta_text: Some("Chunk 1 committing...".to_string()),
                ..Default::default()
            };
            let mut f1_payload = Vec::new();
            msg.encode(&mut f1_payload).unwrap();
            let f1 = frame_data(&f1_payload);
            let f2 = frame_end_stream(r#"{"error":{"code":"internal","message":"stream crashed"}}"#);

            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let _ = tx.send(Ok::<_, std::io::Error>(bytes::Bytes::from(f1)));

            tokio::spawn(async move {
                if let Some(rx) = rx_holder.lock().await.take() {
                    let _ = rx.await;
                }
                let _ = tx.send(Ok(bytes::Bytes::from(f2)));
            });

            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, STREAM_CONTENT_TYPE),
                    (header::HeaderName::from_static("connect-protocol-version"), "1"),
                ],
                axum::body::Body::from_stream(futures::stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|chunk| (chunk, rx))
                })),
            )
        }
    }));
    let task1 = tokio::spawn(async move { axum::serve(listener1, app1).await.unwrap() });

    // Acc2 mock: should NEVER be called after downstream commitment
    let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr2 = listener2.local_addr().unwrap();
    let app2 = Router::new().fallback(post(move || {
        let count = Arc::clone(&acc2_calls_clone);
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            let f1 = frame_data(b"");
            let end = frame_end_stream(r#"{"error":null}"#);
            let mut wire = f1;
            wire.extend_from_slice(&end);
            (StatusCode::OK, [(header::CONTENT_TYPE, STREAM_CONTENT_TYPE)], wire)
        }
    }));
    let task2 = tokio::spawn(async move { axum::serve(listener2, app2).await.unwrap() });

    let url1 = format!("http://{addr1}");
    let url2 = format!("http://{addr2}");

    let gw = start_devin_gateway_with_accounts(&[
        ("acc-stream-1", "tok-1", &url1),
        ("acc-stream-2", "tok-2", &url2),
    ])
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/chat/completions", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "stream retry test"}],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let mut stream = resp.bytes_stream();

    use futures::StreamExt;
    let mut initial_text = String::new();
    while let Some(chunk) = stream.next().await {
        let text = String::from_utf8_lossy(&chunk.unwrap()).to_string();
        initial_text.push_str(&text);
        if initial_text.contains("Chunk 1") {
            break;
        }
    }
    assert!(
        initial_text.contains("Chunk 1"),
        "downstream must receive first text delta, got: {initial_text}"
    );

    // Signal upstream to emit Chunk 2 (error)
    let _ = chunk2_tx.send(());

    // Drain remaining stream
    while let Some(chunk) = stream.next().await {
        let _ = chunk.unwrap();
    }

    let total_calls = acc1_calls.load(Ordering::SeqCst) + acc2_calls.load(Ordering::SeqCst);
    assert_eq!(
        total_calls, 1,
        "exactly 1 account must be called; no cross-account retry after downstream commitment"
    );

    task1.abort();
    task2.abort();
}

// ─── Test 11: Upstream Disconnect, Inflight Cleanup, and History Failure ─────

#[tokio::test]
async fn test_devin_relay_upstream_disconnect_inflight_and_history_failure() {
    let mock = DevinMockProcess::start("transport-cancellation");
    let test_token = "devin-token-cancel";
    let gw = start_devin_gateway("cancel-acc", test_token, &mock.url).await;

    // 1. Establish replay cursor BEFORE request action
    let initial_state = mock.get_state().await;
    let initial_events_count = initial_state["events"].as_array().map(|a| a.len()).unwrap_or(0);

    // 2. Subscribe to exact finalizer notification signal BEFORE triggering cancellation
    let mut finalizer_rx = mahoquot_gateway::relay::subscribe_finalizer(&gw.state, "cancel-acc");

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/chat/completions", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "Cancel me"}],
            "stream": true
        }))
        .send()
        .await
        .expect("stream should open");

    assert_eq!(resp.status(), StatusCode::OK);
    let mut stream = resp.bytes_stream();

    // Read first chunk from the stream
    use futures::StreamExt;
    let first_chunk = stream.next().await;
    assert!(first_chunk.is_some(), "must receive first chunk");

    // Client drops stream early
    drop(stream);

    // 3. Await exact finalizer completion signal with bounded timeout (not scheduling luck)
    let finalizer_completed = tokio::time::timeout(Duration::from_secs(5), finalizer_rx.recv()).await;
    assert!(
        finalizer_completed.is_ok(),
        "finalizer must signal completion on client drop"
    );

    // 4. Verify mock server recorded client_closed according to replay cursor contract
    let wait_res = client
        .post(format!("{}/__control/wait-event", mock.url))
        .json(&json!({
            "type": "client_closed",
            "timeout_ms": 5000,
            "since_timestamp": 0
        }))
        .send()
        .await
        .expect("must post wait-event");
    assert!(wait_res.status().is_success(), "mock server must confirm client_closed event");
    let wait_body: serde_json::Value = wait_res.json().await.unwrap();
    assert_eq!(wait_body["event"]["type"], "client_closed");

    let final_state = mock.get_state().await;
    let events = final_state["events"].as_array().unwrap();
    let has_closed = events.iter().skip(initial_events_count).any(|e| e["type"] == "client_closed");
    assert!(
        has_closed,
        "mock replay cursor must contain client_closed event occurring after start cursor"
    );
    assert!(
        final_state["connection_close_count"].as_u64().unwrap() >= 1,
        "connection_close_count must be at least 1"
    );

    // 5. Assert in-flight count cleanly returned to 0
    let inflight = gw.state.monitor.in_flight();
    assert_eq!(inflight, 0, "in_flight must return to 0 after cancellation");

    // 6. Flush history synchronously through the channel
    gw.state.history.flush().expect("history flush must succeed");

    // 7. Query actual SQLite rows from `usage_events`: cancellation must be logged as failure (succeeded == 0)
    let db_path = gw.config_path.with_file_name("request-history.sqlite");
    assert!(db_path.exists(), "request-history.sqlite must exist");
    let conn = rusqlite::Connection::open(&db_path).expect("open history sqlite");
    let (succeeded, status_val): (i64, i64) = conn
        .query_row(
            "SELECT succeeded, status_code FROM usage_events WHERE account_identifier = 'cancel-acc' ORDER BY occurred_at_ms DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("query cancel-acc row");
    assert_eq!(
        succeeded, 0,
        "canceled stream must be recorded as failure (succeeded == 0) in request history"
    );
    assert_eq!(status_val, 499, "canceled stream must be recorded with status 499");
}

// ─── Test 12: Logs and History Token Non-Disclosure with Real DB & WAL Audit ─

#[tokio::test]
async fn test_devin_relay_token_non_disclosure() {
    let mock = DevinMockProcess::start("default");
    let test_token = "devin-super-secret-token-xyz987";
    let gw = start_devin_gateway("secret-acc", test_token, &mock.url).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/chat/completions", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false
        }))
        .send()
        .await
        .unwrap();

    let body = resp.text().await.unwrap();
    assert!(!body.contains(test_token), "response body must not disclose session token");

    // 1. Flush SQLite history
    gw.state.history.flush().expect("history flush must succeed");

    // 2. Query actual SQLite rows from `usage_events` (must exist and not be empty)
    let db_path = gw.config_path.with_file_name("request-history.sqlite");
    assert!(db_path.exists(), "request-history.sqlite must exist");
    let conn = rusqlite::Connection::open(&db_path).expect("open history sqlite");

    let mut stmt = conn
        .prepare("SELECT event_id, account_identifier, provider, model, key_identifier FROM usage_events")
        .expect("prepare select");
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })
        .expect("query map");

    let mut row_count = 0;
    for row in rows {
        let (event_id, account, provider, model, key_id) = row.expect("read row");
        row_count += 1;
        assert!(!event_id.contains(test_token), "event_id contains secret");
        assert!(!account.contains(test_token), "account contains secret");
        assert!(!provider.contains(test_token), "provider contains secret");
        assert!(!model.contains(test_token), "model contains secret");
        if let Some(k) = key_id {
            assert!(!k.contains(test_token), "key_identifier contains secret");
        }
    }
    assert!(row_count > 0, "request history must contain at least 1 record");

    // 3. Raw byte audit of SQLite DB and WAL files (never unwrap_or_default)
    let db_bytes = std::fs::read(&db_path).expect("read db bytes");
    assert!(
        !db_bytes.windows(test_token.len()).any(|w| w == test_token.as_bytes()),
        "SQLite database raw bytes must not contain token"
    );
    let wal_path = gw.config_path.with_file_name("request-history.sqlite-wal");
    if wal_path.exists() {
        let wal_bytes = std::fs::read(&wal_path).expect("read wal bytes");
        assert!(
            !wal_bytes.windows(test_token.len()).any(|w| w == test_token.as_bytes()),
            "SQLite WAL raw bytes must not contain token"
        );
    }

    // 4. Audit in-memory log tail
    let logs = gw.state.log_tail.snapshot();
    assert!(!logs.is_empty(), "log tail must contain recorded events");
    for line in logs {
        assert!(!line.contains(test_token), "log line discloses token: {line}");
    }

    // 5. Audit disk log files if present
    let log_dir = gw.auth_dir.join("logs");
    if log_dir.exists() {
        for entry in std::fs::read_dir(log_dir).expect("read log dir") {
            let entry = entry.expect("log entry");
            if entry.path().is_file() {
                let log_bytes = std::fs::read(entry.path()).expect("read log file");
                assert!(
                    !log_bytes.windows(test_token.len()).any(|w| w == test_token.as_bytes()),
                    "log file on disk discloses token: {:?}",
                    entry.path()
                );
            }
        }
    }
}

// ─── Test 13: Unsupported Native / Responses Mode Rejected ───────────────────

#[tokio::test]
async fn test_devin_relay_unsupported_native_responses_rejected() {
    let mock = DevinMockProcess::start("default");
    let test_token = "devin-token-native";
    let gw = start_devin_gateway("native-acc", test_token, &mock.url).await;

    let client = reqwest::Client::new();
    // Request to /v1/responses with unsupported stateful previous_response_id
    let resp = client
        .post(format!("{}/v1/responses", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "input": "test responses mode",
            "previous_response_id": "resp_stateful_123"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "Unsupported stateful previous_response_id for Devin must be rejected with 400 Bad Request"
    );
}

// ─── Test 14: Concurrent Discovery Refresh & Credential Rotation Safety ──────

#[tokio::test]
async fn test_devin_relay_concurrent_refresh_and_credential_rotation() {
    let (chunk2_tx, chunk2_rx) = tokio::sync::oneshot::channel::<()>();
    let chunk2_rx = Arc::new(tokio::sync::Mutex::new(Some(chunk2_rx)));
    let chunk2_rx_clone = Arc::clone(&chunk2_rx);

    // Mock upstream that yields Chunk 1, waits on barrier, then emits clean EndStream
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let mock_app = Router::new().fallback(post(move || {
        let rx_holder = Arc::clone(&chunk2_rx_clone);
        async move {
            let msg = ChatMessageResponse {
                delta_text: Some("Mid-stream chunk...".to_string()),
                ..Default::default()
            };
            let mut f1_payload = Vec::new();
            msg.encode(&mut f1_payload).unwrap();
            let f1 = frame_data(&f1_payload);
            let f2 = frame_end_stream(r#"{"error":null}"#);

            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let _ = tx.send(Ok::<_, std::io::Error>(bytes::Bytes::from(f1)));

            tokio::spawn(async move {
                if let Some(rx) = rx_holder.lock().await.take() {
                    let _ = rx.await;
                }
                let _ = tx.send(Ok(bytes::Bytes::from(f2)));
            });

            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, STREAM_CONTENT_TYPE),
                    (header::HeaderName::from_static("connect-protocol-version"), "1"),
                ],
                axum::body::Body::from_stream(futures::stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|chunk| (chunk, rx))
                })),
            )
        }
    }));

    let mock_task = tokio::spawn(async move { axum::serve(listener, mock_app).await.unwrap() });
    let token = "devin-token-stable";
    let gw = start_devin_gateway("concurrent-acc", token, &format!("http://{addr}")).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/chat/completions", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "test mid-stream refresh"}],
            "stream": true
        }))
        .send()
        .await
        .expect("chat should start");

    assert_eq!(resp.status(), StatusCode::OK);
    let mut stream = resp.bytes_stream();

    use futures::StreamExt;
    // Read until mid-stream chunk is received by downstream
    while let Some(chunk) = stream.next().await {
        let text = String::from_utf8_lossy(&chunk.unwrap()).to_string();
        if text.contains("Mid-stream") {
            break;
        }
    }

    // 1. While Chat is running, discovery refresh publishes a new snapshot with cloned member
    let current_pool = gw.state.pool.load();
    let member = current_pool.members.iter().find(|m| m.id == "concurrent-acc").unwrap();
    let cloned_member = Arc::new(member.clone_for_snapshot(None));
    let new_pool = PoolSnapshot::new(
        current_pool.generation + 1,
        vec![cloned_member],
        current_pool.models.clone(),
        current_pool.registry.clone(),
    );
    gw.state.pool.store(Arc::new(new_pool));

    // 2. Complete Chat stream
    let _ = chunk2_tx.send(());
    while let Some(chunk) = stream.next().await {
        let _ = chunk.unwrap();
    }

    // 3. Current pool's member (same credential) must reflect the success exactly once!
    let updated_pool = gw.state.pool.load();
    let current_member = updated_pool.members.iter().find(|m| m.id == "concurrent-acc").unwrap();
    assert_eq!(
        current_member.ok_count.load(Ordering::SeqCst),
        1,
        "same-credential member in current pool must reflect success exactly once"
    );

    // 4. Now test credential rotation: replace credential on current member with new token
    let rotated_member = Arc::new(mahoquot_gateway::account::AccountMember::for_test_with_id(
        "concurrent-acc",
        mahoquot_gateway::account::ProviderAccount::Devin(mahoquot_providers::DevinAccount {
            provider_type: "devin".to_string(),
            identity_slug: "concurrent-acc".to_string(),
            label: None,
            email: None,
            access_token: "devin-token-NEW-ROTATED".to_string(),
            api_server_url: format!("http://{addr}"),
            disabled: false,
        }),
    ));
    let rotated_pool = PoolSnapshot::new(
        updated_pool.generation + 1,
        vec![rotated_member.clone()],
        updated_pool.models.clone(),
        updated_pool.registry.clone(),
    );
    gw.state.pool.store(Arc::new(rotated_pool));

    // Run a stream on the old member that fails with unauthenticated:
    // Because token doesn't match rotated_member's token, rotated_member MUST NOT be marked AuthFailed!
    let (err_tx, err_rx) = tokio::sync::oneshot::channel::<()>();
    let err_rx = Arc::new(tokio::sync::Mutex::new(Some(err_rx)));
    let err_rx_clone = Arc::clone(&err_rx);

    let listener_err = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr_err = listener_err.local_addr().unwrap();
    let mock_app_err = Router::new().fallback(post(move || {
        let rx_holder = Arc::clone(&err_rx_clone);
        async move {
            let msg = ChatMessageResponse {
                delta_text: Some("First chunk before auth failure...".to_string()),
                ..Default::default()
            };
            let mut f1_payload = Vec::new();
            msg.encode(&mut f1_payload).unwrap();
            let f1 = frame_data(&f1_payload);
            let f2 = frame_end_stream(r#"{"error":{"code":"unauthenticated","message":"token revoked"}}"#);

            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let _ = tx.send(Ok::<_, std::io::Error>(bytes::Bytes::from(f1)));

            tokio::spawn(async move {
                if let Some(rx) = rx_holder.lock().await.take() {
                    let _ = rx.await;
                }
                let _ = tx.send(Ok(bytes::Bytes::from(f2)));
            });

            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, STREAM_CONTENT_TYPE),
                    (header::HeaderName::from_static("connect-protocol-version"), "1"),
                ],
                axum::body::Body::from_stream(futures::stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|chunk| (chunk, rx))
                })),
            )
        }
    }));
    let task_err = tokio::spawn(async move { axum::serve(listener_err, mock_app_err).await.unwrap() });

    let gw_rot = start_devin_gateway("rot-acc", "old-token-xyz", &format!("http://{addr_err}")).await;
    let resp_err = client
        .post(format!("{}/v1/chat/completions", gw_rot.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "trigger rot error"}],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    let mut stream_err = resp_err.bytes_stream();
    while let Some(chunk) = stream_err.next().await {
        let text = String::from_utf8_lossy(&chunk.unwrap()).to_string();
        if text.contains("First chunk") {
            break;
        }
    }

    // While stream is in flight, rotate credential in gw_rot's pool!
    let pool_before = gw_rot.state.pool.load();
    let new_member = Arc::new(mahoquot_gateway::account::AccountMember::for_test_with_id(
        "rot-acc",
        mahoquot_gateway::account::ProviderAccount::Devin(mahoquot_providers::DevinAccount {
            provider_type: "devin".to_string(),
            identity_slug: "rot-acc".to_string(),
            label: None,
            email: None,
            access_token: "new-token-fresh-123".to_string(),
            api_server_url: format!("http://{addr_err}"),
            disabled: false,
        }),
    ));
    gw_rot.state.pool.store(Arc::new(PoolSnapshot::new(
        pool_before.generation + 1,
        vec![new_member.clone()],
        pool_before.models.clone(),
        pool_before.registry.clone(),
    )));

    // Finish the failing stream from the old token
    let _ = err_tx.send(());
    while let Some(chunk) = stream_err.next().await {
        let _ = chunk.unwrap();
    }

    // Assert the new rotated member was NOT contaminated!
    assert_eq!(
        new_member.fail_count.load(Ordering::SeqCst),
        0,
        "rotated credential must not be marked failed by old credential's completion"
    );
    assert_eq!(
        new_member.health(),
        Health::Available,
        "rotated credential health must remain Available"
    );

    mock_task.abort();
    task_err.abort();
}

// ─── Test 15: Late Error Exactly-Once Failure Count & Retained Health ───────

#[tokio::test]
async fn test_devin_relay_late_error_exactly_once_failure_and_retained_health() {
    // Test A: Late unauthenticated error must record fail_count == 1, AuthFailed health, and status 401
    let (tx_a, rx_a) = tokio::sync::oneshot::channel::<()>();
    let rx_a = Arc::new(tokio::sync::Mutex::new(Some(rx_a)));
    let listener_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr_a = listener_a.local_addr().unwrap();

    let app_a = Router::new().fallback(post(move || {
        let rx_holder = Arc::clone(&rx_a);
        async move {
            let msg = ChatMessageResponse {
                delta_text: Some("Part 1 before unauth...".to_string()),
                ..Default::default()
            };
            let mut f1_payload = Vec::new();
            msg.encode(&mut f1_payload).unwrap();
            let f1 = frame_data(&f1_payload);
            let f2 = frame_end_stream(r#"{"error":{"code":"unauthenticated","message":"session token revoked"}}"#);

            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let _ = tx.send(Ok::<_, std::io::Error>(bytes::Bytes::from(f1)));

            tokio::spawn(async move {
                if let Some(rx) = rx_holder.lock().await.take() {
                    let _ = rx.await;
                }
                let _ = tx.send(Ok(bytes::Bytes::from(f2)));
            });

            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, STREAM_CONTENT_TYPE),
                    (header::HeaderName::from_static("connect-protocol-version"), "1"),
                ],
                axum::body::Body::from_stream(futures::stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|chunk| (chunk, rx))
                })),
            )
        }
    }));
    let task_a = tokio::spawn(async move { axum::serve(listener_a, app_a).await.unwrap() });
    let gw_a = start_devin_gateway("late-unauth", "tok-unauth-test", &format!("http://{addr_a}")).await;

    let client = reqwest::Client::new();
    let resp_a = client
        .post(format!("{}/v1/chat/completions", gw_a.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp_a.status(), StatusCode::OK);
    let mut stream_a = resp_a.bytes_stream();
    use futures::StreamExt;
    while let Some(chunk) = stream_a.next().await {
        let text = String::from_utf8_lossy(&chunk.unwrap()).to_string();
        if text.contains("Part 1") {
            break;
        }
    }

    // Release upstream to emit terminal error
    let _ = tx_a.send(());
    while let Some(chunk) = stream_a.next().await {
        let _ = chunk.unwrap();
    }

    let member_a = gw_a.state.pool.load().members.iter().find(|m| m.id() == "late-unauth").unwrap().clone();
    assert_eq!(
        member_a.fail_count.load(Ordering::Relaxed),
        1,
        "fail_count must be incremented exactly once for late unauthenticated error"
    );
    assert_eq!(
        member_a.health(),
        Health::AuthFailed,
        "health must be AuthFailed after late unauthenticated error"
    );
    let last_err_a = gw_a.state.monitor.last_error("late-unauth").expect("must record last error in monitor");
    assert_eq!(last_err_a.status, 401, "monitor error status must be 401");
    assert_eq!(
        last_err_a.message,
        "unauthenticated request to upstream",
        "monitor error message must be retained safe Connect error, not generic cancellation"
    );

    task_a.abort();

    // Test B: Late resource_exhausted error must record fail_count == 1, Cooldown health, and status 429
    let (tx_b, rx_b) = tokio::sync::oneshot::channel::<()>();
    let rx_b = Arc::new(tokio::sync::Mutex::new(Some(rx_b)));
    let listener_b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr_b = listener_b.local_addr().unwrap();

    let app_b = Router::new().fallback(post(move || {
        let rx_holder = Arc::clone(&rx_b);
        async move {
            let msg = ChatMessageResponse {
                delta_text: Some("Part 1 before limit...".to_string()),
                ..Default::default()
            };
            let mut f1_payload = Vec::new();
            msg.encode(&mut f1_payload).unwrap();
            let f1 = frame_data(&f1_payload);
            let f2 = frame_end_stream(r#"{"error":{"code":"resource_exhausted","message":"rate limit hit"}}"#);

            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let _ = tx.send(Ok::<_, std::io::Error>(bytes::Bytes::from(f1)));

            tokio::spawn(async move {
                if let Some(rx) = rx_holder.lock().await.take() {
                    let _ = rx.await;
                }
                let _ = tx.send(Ok(bytes::Bytes::from(f2)));
            });

            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, STREAM_CONTENT_TYPE),
                    (header::HeaderName::from_static("connect-protocol-version"), "1"),
                ],
                axum::body::Body::from_stream(futures::stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|chunk| (chunk, rx))
                })),
            )
        }
    }));
    let task_b = tokio::spawn(async move { axum::serve(listener_b, app_b).await.unwrap() });
    let gw_b = start_devin_gateway("late-exhaust", "tok-exhaust-test", &format!("http://{addr_b}")).await;

    let resp_b = client
        .post(format!("{}/v1/chat/completions", gw_b.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp_b.status(), StatusCode::OK);
    let mut stream_b = resp_b.bytes_stream();
    while let Some(chunk) = stream_b.next().await {
        let text = String::from_utf8_lossy(&chunk.unwrap()).to_string();
        if text.contains("Part 1") {
            break;
        }
    }

    let _ = tx_b.send(());
    while let Some(chunk) = stream_b.next().await {
        let _ = chunk.unwrap();
    }

    let member_b = gw_b.state.pool.load().members.iter().find(|m| m.id() == "late-exhaust").unwrap().clone();
    assert_eq!(
        member_b.fail_count.load(Ordering::Relaxed),
        1,
        "fail_count must be incremented exactly once for late resource_exhausted error"
    );
    assert!(
        matches!(member_b.health(), Health::Cooldown { .. }),
        "health must be Cooldown after late resource_exhausted error"
    );
    let last_err_b = gw_b.state.monitor.last_error("late-exhaust").expect("must record last error in monitor");
    assert_eq!(last_err_b.status, 429, "monitor error status must be 429");
    assert_eq!(
        last_err_b.message,
        "upstream quota or rate limit exhausted",
        "monitor error message must be retained safe Connect error"
    );

    task_b.abort();
}

// ─── Test 16: Nonstream Late Connect Error Status Code Mapping ──────────────

#[tokio::test]
async fn test_devin_relay_nonstream_late_connect_error_mapping() {
    // In nonstream mode, when upstream returns 200 with an EndStream error code,
    // gateway must map resource_exhausted to HTTP 429, unauthenticated to HTTP 401,
    // NOT 502 Bad Gateway!
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let app = Router::new().fallback(post(|| async {
        let msg = ChatMessageResponse {
            delta_text: Some("Nonstream text before error".to_string()),
            ..Default::default()
        };
        let mut f1_payload = Vec::new();
        msg.encode(&mut f1_payload).unwrap();
        let f1 = frame_data(&f1_payload);
        let f2 = frame_end_stream(r#"{"error":{"code":"resource_exhausted","message":"nonstream quota exceeded"}}"#);

        let mut combined = f1;
        combined.extend_from_slice(&f2);

        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, STREAM_CONTENT_TYPE),
                (header::HeaderName::from_static("connect-protocol-version"), "1"),
            ],
            combined,
        )
    }));

    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let gw = start_devin_gateway("nonstream-err-acc", "tok-nonstream-err", &format!("http://{addr}")).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/chat/completions", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": false
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "nonstream late resource_exhausted must map to HTTP 429, not 502"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "resource_exhausted");
    assert_eq!(body["error"]["message"], "upstream quota or rate limit exhausted");

    task.abort();
}

// ─── Test 17: Devin Response Wrong Content-Type Rejection ───────────────────

#[tokio::test]
async fn test_devin_relay_response_wrong_content_type() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let app = Router::new().fallback(post(|| async {
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"message": "wrong content type"}"#,
        )
    }));

    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let gw = start_devin_gateway("wrong-ct-acc", "tok-ct-test", &format!("http://{addr}")).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/chat/completions", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let body: serde_json::Value = resp.json().await.unwrap();
    let err_msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        err_msg.contains("application/connect+proto") || err_msg.contains("content-type"),
        "error message must reject non-connect+proto content-type, got: {err_msg}"
    );

    task.abort();
}

// ─── Test 18: Split-Header and Coalesced Oversized Input Regression ─────────

#[tokio::test]
async fn test_devin_relay_split_header_and_coalesced_oversize() {
    // 1. Split header across multiple TCP chunks (e.g. 2 bytes, then 3 bytes, then payload)
    let listener_split = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr_split = listener_split.local_addr().unwrap();

    let app_split = Router::new().fallback(post(|| async {
        let msg = ChatMessageResponse {
            delta_text: Some("Split header survived!".to_string()),
            ..Default::default()
        };
        let mut f1_payload = Vec::new();
        msg.encode(&mut f1_payload).unwrap();
        let f1 = frame_data(&f1_payload);
        let f2 = frame_end_stream("{}");

        // Split f1 header into 2 bytes and 3 bytes:
        let part1 = bytes::Bytes::copy_from_slice(&f1[..2]);
        let part2 = bytes::Bytes::copy_from_slice(&f1[2..5]);
        let part3 = bytes::Bytes::copy_from_slice(&f1[5..]);
        let part4 = bytes::Bytes::from(f2);

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = tx.send(Ok::<_, std::io::Error>(part1));
        let _ = tx.send(Ok(part2));
        let _ = tx.send(Ok(part3));
        let _ = tx.send(Ok(part4));

        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, STREAM_CONTENT_TYPE),
                (header::HeaderName::from_static("connect-protocol-version"), "1"),
            ],
            axum::body::Body::from_stream(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|chunk| (chunk, rx))
            })),
        )
    }));

    let task_split = tokio::spawn(async move { axum::serve(listener_split, app_split).await.unwrap() });
    let gw_split = start_devin_gateway("split-hdr-acc", "tok-split-hdr", &format!("http://{addr_split}")).await;

    let client = reqwest::Client::new();
    let resp_split = client
        .post(format!("{}/v1/chat/completions", gw_split.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp_split.status(), StatusCode::OK);
    let text = resp_split.text().await.unwrap();
    assert!(text.contains("Split header survived!"), "split-header stream must succeed");

    task_split.abort();

    // 2. Coalesced chunk with header declaring oversized payload (> 16 MiB)
    let listener_oversize = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr_oversize = listener_oversize.local_addr().unwrap();

    let app_oversize = Router::new().fallback(post(|| async {
        // Construct 5-byte header claiming 20 MiB payload
        let mut hdr = [0u8; 5];
        hdr[0] = 0x00; // data frame
        let len: u32 = 20 * 1024 * 1024;
        hdr[1..5].copy_from_slice(&len.to_be_bytes());

        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, STREAM_CONTENT_TYPE),
                (header::HeaderName::from_static("connect-protocol-version"), "1"),
            ],
            bytes::Bytes::copy_from_slice(&hdr),
        )
    }));

    let task_oversize = tokio::spawn(async move { axum::serve(listener_oversize, app_oversize).await.unwrap() });
    let gw_oversize = start_devin_gateway("oversize-acc", "tok-oversize", &format!("http://{addr_oversize}")).await;

    let resp_oversize = client
        .post(format!("{}/v1/chat/completions", gw_oversize.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp_oversize.status(), StatusCode::BAD_GATEWAY);
    let body: serde_json::Value = resp_oversize.json().await.unwrap();
    let err_msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        err_msg.contains("connect frame too large"),
        "must reject oversize frame immediately at header, got: {err_msg}"
    );

    task_oversize.abort();
}

// ─── Test 19: Preflight Total Deadline on Headers-Then-No-Body Upstream ──────

#[tokio::test]
async fn test_devin_relay_preflight_total_deadline_headers_then_no_body() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Upstream immediately returns HTTP 200 OK + Content-Type headers, but yields NO body bytes
    let app = Router::new().fallback(post(|| async {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<bytes::Bytes, std::io::Error>>();
        // Hold sender alive so stream does NOT close with immediate EOF
        tokio::spawn(async move {
            let _tx = tx;
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, STREAM_CONTENT_TYPE),
                (header::HeaderName::from_static("connect-protocol-version"), "1"),
            ],
            axum::body::Body::from_stream(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|chunk| (chunk, rx))
            })),
        )
    }));

    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let gw = start_devin_gateway("no-body-acc", "tok-no-body", &format!("http://{addr}")).await;

    let start = std::time::Instant::now();
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/chat/completions", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .header("x-test-preflight-deadline-ms", "250")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true
        }))
        .send()
        .await
        .expect("request should initiate");

    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(200),
        "preflight total deadline must wait for the declared deadline, elapsed: {elapsed:?}"
    );
    assert_eq!(
        resp.status(),
        StatusCode::BAD_GATEWAY,
        "headers-then-no-body must fail with 502 Bad Gateway"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"]["message"],
        "upstream request timed out during preflight",
        "must return safe error without leaking URLs or raw strings"
    );

    task.abort();
}

// ─── Test 20: Absent-Catalog Account B Denied Account A's Discovered Model ────

#[tokio::test]
async fn test_devin_relay_absent_catalog_account_b_denied_account_a_model() {
    let mock = DevinMockProcess::start("default");
    let test_token_a = "token-acc-a";
    let test_token_b = "token-acc-b";

    // Start gateway with Account A and Account B
    let gw = start_devin_gateway_with_accounts(&[
        ("acc-a", test_token_a, &mock.url),
        ("acc-b", test_token_b, &mock.url),
    ])
    .await;

    // Account A discovers "devin/glm-5-2"; Account B has absent catalog
    let discovered_a = vec![mahoquot_gateway::devin_catalog::DiscoveredDevinModel {
        model_uid: "glm-5-2".to_string(),
        public_id: "devin/glm-5-2".to_string(),
        label: "glm-5-2".to_string(),
        supports_images: true,
        is_premium: false,
        is_beta: false,
        is_recommended: false,
        is_new: false,
        is_capacity_limited: false,
        promo_active: false,
        max_tokens: Some(200_000),
        credit_multiplier: Some(1.0),
        description: None,
    }];

    let current = gw.state.pool.load();
    for member in &current.members {
        if member.id() == "acc-a" {
            let key = mahoquot_gateway::devin_catalog::DevinCacheKey::new(
                "acc-a",
                &member.access_token(),
                member.effective_base_url(),
            );
            let cat_state = mahoquot_gateway::devin_catalog::DevinAccountCatalogState::new_success(
                key,
                discovered_a.clone(),
                100,
            );
            member.set_devin_catalog_state(Arc::new(cat_state));
        } else if member.id == "acc-b" {
            // Account B has absent / empty catalog
            let key = mahoquot_gateway::devin_catalog::DevinCacheKey::new(
                "acc-b",
                &member.access_token(),
                member.effective_base_url(),
            );
            let cat_state = mahoquot_gateway::devin_catalog::DevinAccountCatalogState::new_success(
                key,
                vec![],
                100,
            );
            member.set_devin_catalog_state(Arc::new(cat_state));
        }
    }

    // Build registry snapshot with glm-5-2 registered
    let mut builder = RegistryBuilder::new(CatalogVersion::new(1), CatalogSource::Discovered);
    builder.register_provider(ProviderId::devin(), ProviderPolicy::Discovered);
    let disc_reg = vec![DiscoveredModel::new(ModelId::new("devin/glm-5-2").unwrap())
        .with_capabilities([ModelCapability::Chat, ModelCapability::Tools])
        .with_context_limit(200_000)
        .with_authority(AuthorityMask::MODELS_ONLY)];
    builder
        .apply_contribution(
            ProviderContribution::from_discovered_models(ProviderId::devin(), disc_reg)
                .with_policy(ProviderPolicy::Discovered),
        )
        .unwrap();
    let snapshot = builder.build().unwrap();

    let new_pool = PoolSnapshot::new(
        current.generation + 1,
        current.members.clone(),
        vec![mahoquot_gateway::models_route::ModelEntry {
            id: "devin/glm-5-2".to_string(),
            owned_by: "devin".to_string(),
        }],
        Arc::new(snapshot),
    );
    gw.state.pool.store(Arc::new(new_pool));

    // Disable Account A to verify Account B is NOT chosen as fallback
    let current_updated = gw.state.pool.load();
    let mem_a = current_updated.members.iter().find(|m| m.id() == "acc-a").unwrap();
    mem_a.set_health(Health::Disabled);

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/chat/completions", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "messages": [{"role": "user", "content": "test"}]
        }))
        .send()
        .await
        .unwrap();

    // Account B must NOT be selected; request must fail with 503 Service Unavailable
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "Account B with absent catalog must NOT be eligible for Account A's model"
    );
}

// ─── Test 21: Discovered Model with Vision=False Rejects Vision Input ────────

#[tokio::test]
async fn test_devin_relay_discovered_vision_false_rejects_vision() {
    let mock = DevinMockProcess::start("default");
    let test_token = "token-no-vision";
    let gw = start_devin_gateway("no-vision-acc", test_token, &mock.url).await;

    // Discover swe-1-7 with supports_images: false
    let discovered = vec![mahoquot_gateway::devin_catalog::DiscoveredDevinModel {
        model_uid: "swe-1-7".to_string(),
        public_id: "devin/swe-1-7".to_string(),
        label: "swe-1-7".to_string(),
        supports_images: false, // Discovered without vision support
        is_premium: false,
        is_beta: false,
        is_recommended: false,
        is_new: false,
        is_capacity_limited: false,
        promo_active: false,
        max_tokens: Some(200_000),
        credit_multiplier: Some(1.0),
        description: None,
    }];

    let current = gw.state.pool.load();
    for member in &current.members {
        if member.id() == "no-vision-acc" {
            let key = mahoquot_gateway::devin_catalog::DevinCacheKey::new(
                "no-vision-acc",
                &member.access_token(),
                member.effective_base_url(),
            );
            let cat_state = mahoquot_gateway::devin_catalog::DevinAccountCatalogState::new_success(
                key,
                discovered.clone(),
                100,
            );
            member.set_devin_catalog_state(Arc::new(cat_state));
        }
    }

    let mut builder = RegistryBuilder::new(CatalogVersion::new(1), CatalogSource::Discovered);
    builder.register_provider(ProviderId::devin(), ProviderPolicy::Discovered);
    let disc_reg = vec![DiscoveredModel::new(ModelId::new("devin/swe-1-7").unwrap())
        .with_capabilities([ModelCapability::Chat])
        .with_context_limit(200_000)
        .with_authority(AuthorityMask::MODELS_ONLY)];
    builder
        .apply_contribution(
            ProviderContribution::from_discovered_models(ProviderId::devin(), disc_reg)
                .with_policy(ProviderPolicy::Discovered),
        )
        .unwrap();
    let snapshot = builder.build().unwrap();

    let new_pool = PoolSnapshot::new(
        current.generation + 1,
        current.members.clone(),
        vec![mahoquot_gateway::models_route::ModelEntry {
            id: "devin/swe-1-7".to_string(),
            owned_by: "devin".to_string(),
        }],
        Arc::new(snapshot),
    );
    gw.state.pool.store(Arc::new(new_pool));

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/chat/completions", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/swe-1-7",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "look at this"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}}
                ]
            }]
        }))
        .send()
        .await
        .unwrap();

    // Must be rejected with 400 Bad Request because model discovery does not support vision
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "Vision request to model discovered with vision=false must be rejected"
    );
}
