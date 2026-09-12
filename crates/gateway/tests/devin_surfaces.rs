mod common;

#[path = "devin_regressions/surface_selection.rs"]
mod surface_selection;

use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::http::{header, StatusCode};
use serde_json::json;

use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::runtime_state::PoolSnapshot;
use mahoquot_gateway::state::AppState;
use mahoquot_registry::{
    AuthorityMask, CatalogSource, CatalogVersion, DiscoveredModel, ModelCapability, ModelId,
    ProviderContribution, ProviderId, ProviderPolicy, RegistryBuilder,
};

static TEST_SEQ: AtomicU64 = AtomicU64::new(100);

// ─── Process-based Mock Harness ──────────────────────────────────────────────

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

// ─── Test Gateway Helpers ───────────────────────────────────────────────────

fn register_devin_models(state: &AppState, models: &[&str]) {
    let mut builder = RegistryBuilder::new(CatalogVersion::new(1), CatalogSource::Discovered);
    builder.register_provider(ProviderId::devin(), ProviderPolicy::Discovered);

    let discovered: Vec<DiscoveredModel> = models
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
            ProviderContribution::from_discovered_models(ProviderId::devin(), discovered)
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
    #[allow(dead_code)]
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
    let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let auth_dir = common::unique_temp_dir(&format!("devin-surfaces-test-{seq}"));
    let config_path = auth_dir.join("config.yaml");

    let cred_file = auth_dir.join(format!("devin-{identity}.json"));
    std::fs::write(&cred_file, devin_credential(identity, token, upstream_url)).unwrap();

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

// ─── Surface 1: Responses API (Stream, Non-stream, Aliases) ──────────────────

#[tokio::test]
async fn test_devin_surfaces_responses_stream_and_nonstream() {
    let mock = DevinMockProcess::start("default");
    let gw = start_devin_gateway("acc-resp", "devin-tok-resp", &mock.url).await;
    let client = reqwest::Client::new();

    // 1. Streaming POST /v1/responses
    let stream_res = client
        .post(format!("{}/v1/responses", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "input": "Explain quantum theory",
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(stream_res.status(), StatusCode::OK);
    let sse_body = stream_res.text().await.unwrap();
    assert!(sse_body.contains("event: response.created"));
    assert!(sse_body.contains("event: response.output_item.added"));
    assert!(sse_body.contains("event: response.completed"));

    // 2. Non-streaming POST /v1/responses
    let nonstream_res = client
        .post(format!("{}/v1/responses", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "input": "Explain quantum theory",
            "stream": false
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(nonstream_res.status(), StatusCode::OK);
    let json_body: serde_json::Value = nonstream_res.json().await.unwrap();
    assert_eq!(json_body["object"], "response");
    assert_eq!(json_body["status"], "completed");
    assert_eq!(json_body["model"], "devin/glm-5-2");
    assert!(!json_body["output"].as_array().unwrap().is_empty());

    // 3. Alias POST /responses
    let alias_res = client
        .post(format!("{}/responses", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "input": "Hello alias",
            "stream": false
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(alias_res.status(), StatusCode::OK);

    // 4. Codex alias POST /backend-api/codex/responses
    let codex_alias_res = client
        .post(format!("{}/backend-api/codex/responses", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "input": "Hello codex alias",
            "stream": false
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(codex_alias_res.status(), StatusCode::OK);
}

// ─── Surface 2: Responses API Two-Turn Tools & Rejections ───────────────────

#[tokio::test]
async fn test_devin_surfaces_responses_two_turn_tools() {
    let mock = DevinMockProcess::start("tools");
    let gw = start_devin_gateway("acc-resp-tool", "devin-tok-tool", &mock.url).await;
    let client = reqwest::Client::new();

    // Turn 1: request that triggers tool call
    let t1_res = client
        .post(format!("{}/v1/responses", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "input": "What is the weather in Seoul?",
            "tools": [{
                "type": "function",
                "name": "get_weather",
                "description": "get weather for city",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }],
            "stream": false
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(t1_res.status(), StatusCode::OK);
    let t1_body: serde_json::Value = t1_res.json().await.unwrap();
    let output = t1_body["output"].as_array().expect("output array");
    let tool_item = output
        .iter()
        .find(|item| item["type"] == "function_call")
        .expect("must emit function_call item");
    assert_eq!(tool_item["name"], "get_weather");
    let call_id = tool_item["call_id"].as_str().unwrap();

    // Turn 2: submit tool result
    let t2_res = client
        .post(format!("{}/v1/responses", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "input": [
                {
                    "type": "function_call",
                    "call_id": call_id,
                    "name": "get_weather",
                    "arguments": "{\"city\":\"Seoul\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": "22C and sunny"
                }
            ],
            "stream": false
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(t2_res.status(), StatusCode::OK);
    let t2_body: serde_json::Value = t2_res.json().await.unwrap();
    assert_eq!(t2_body["status"], "completed");
    let t2_output = t2_body["output"].as_array().expect("t2 output array");
    let msg_item = t2_output
        .iter()
        .find(|item| item["type"] == "message")
        .expect("must emit message item in turn 2");
    assert!(msg_item["content"][0]["text"].as_str().unwrap().contains("Seoul"));
}

#[tokio::test]
async fn test_devin_surfaces_responses_unsupported_rejections() {
    let mock = DevinMockProcess::start("default");
    let gw = start_devin_gateway("acc-resp-rej", "devin-tok-rej", &mock.url).await;
    let client = reqwest::Client::new();

    // 1. previous_response_id must return 400
    let res_prev = client
        .post(format!("{}/v1/responses", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "previous_response_id": "resp_state_123",
            "input": "continue"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res_prev.status(), StatusCode::BAD_REQUEST);

    // 2. background=true must return 400
    let res_bg = client
        .post(format!("{}/v1/responses", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "background": true,
            "input": "bg work"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res_bg.status(), StatusCode::BAD_REQUEST);

    // 3. store=true must return 400
    let res_store = client
        .post(format!("{}/v1/responses", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "store": true,
            "input": "persist"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res_store.status(), StatusCode::BAD_REQUEST);
}

// ─── Surface 3: Anthropic Messages API (Stream, Non-stream, Tools, Thinking) ─

#[tokio::test]
async fn test_devin_surfaces_anthropic_stream_and_nonstream() {
    let mock = DevinMockProcess::start("default");
    let gw = start_devin_gateway("acc-anth", "devin-tok-anth", &mock.url).await;
    let client = reqwest::Client::new();

    // 1. Streaming POST /v1/messages
    let stream_res = client
        .post(format!("{}/v1/messages", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello Devin via Claude API"}],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(stream_res.status(), StatusCode::OK);
    let sse_text = stream_res.text().await.unwrap();
    assert!(sse_text.contains("event: message_start"));
    assert!(sse_text.contains("event: content_block_start"));
    assert!(sse_text.contains("event: message_stop"));

    // 2. Non-streaming POST /v1/messages
    let nonstream_res = client
        .post(format!("{}/v1/messages", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello Devin nonstream"}],
            "stream": false
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(nonstream_res.status(), StatusCode::OK);
    let body: serde_json::Value = nonstream_res.json().await.unwrap();
    assert_eq!(body["type"], "message");
    assert_eq!(body["role"], "assistant");
    assert_eq!(body["model"], "devin/glm-5-2");

    // 3. Alias POST /messages
    let alias_res = client
        .post(format!("{}/messages", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello alias"}],
            "stream": false
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(alias_res.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_devin_surfaces_anthropic_two_turn_tools_and_error_flag() {
    let mock = DevinMockProcess::start("tools");
    let gw = start_devin_gateway("acc-anth-tools", "devin-tok-anth-tools", &mock.url).await;
    let client = reqwest::Client::new();

    // Turn 1: tool call
    let t1_res = client
        .post(format!("{}/v1/messages", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Weather in Seoul?"}],
            "tools": [{
                "name": "get_weather",
                "description": "get weather for city",
                "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}
            }],
            "stream": false
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(t1_res.status(), StatusCode::OK);
    let t1_body: serde_json::Value = t1_res.json().await.unwrap();
    let content = t1_body["content"].as_array().expect("content array");
    let tool_use = content
        .iter()
        .find(|b| b["type"] == "tool_use")
        .expect("must emit tool_use block");
    assert_eq!(tool_use["name"], "get_weather");
    let tool_id = tool_use["id"].as_str().unwrap();

    // Turn 2: tool_result with is_error: true
    let t2_res = client
        .post(format!("{}/v1/messages", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "Weather in Seoul?"},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "Let me look up the weather", "signature": "sig_turn1"},
                        {"type": "tool_use", "id": tool_id, "name": "get_weather", "input": {"city": "Seoul"}}
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": tool_id,
                            "content": "service temporarily unavailable",
                            "is_error": true
                        }
                    ]
                }
            ],
            "stream": false
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(t2_res.status(), StatusCode::OK);
    let t2_body: serde_json::Value = t2_res.json().await.unwrap();
    assert_eq!(t2_body["stop_reason"], "end_turn");

    // Verify mock capture for Turn 2: has_tool_results was true
    let mock_state = mock.get_state().await;
    let captures = mock_state["captures"].as_array().expect("captures");
    let last_capture = captures.last().expect("last capture");
    assert_eq!(
        last_capture["sanitized_body"]["has_tool_results"],
        true,
        "mock must confirm tool results arrived in Turn 2"
    );
}

// ─── Surface 4: Google Gemini v1beta Surface ─────────────────────────────────

#[tokio::test]
async fn test_devin_surfaces_gemini_native_stream_and_nonstream() {
    let mock = DevinMockProcess::start("default");
    let gw = start_devin_gateway("acc-gemini", "devin-tok-gemini", &mock.url).await;
    let client = reqwest::Client::new();

    // 1. Streaming POST /v1beta/models/devin/glm-5-2:streamGenerateContent
    let stream_res = client
        .post(format!(
            "{}/v1beta/models/devin/glm-5-2:streamGenerateContent",
            gw.base_url
        ))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "contents": [{
                "role": "user",
                "parts": [{"text": "Hello Gemini Devin"}]
            }]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(stream_res.status(), StatusCode::OK);
    let sse_text = stream_res.text().await.unwrap();
    assert!(sse_text.contains("candidates"));

    // 2. Non-streaming POST /v1beta/models/devin/glm-5-2:generateContent
    let nonstream_res = client
        .post(format!(
            "{}/v1beta/models/devin/glm-5-2:generateContent",
            gw.base_url
        ))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "contents": [{
                "role": "user",
                "parts": [{"text": "Hello Gemini Devin nonstream"}]
            }]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(nonstream_res.status(), StatusCode::OK);
    let body: serde_json::Value = nonstream_res.json().await.unwrap();
    assert!(body.get("candidates").is_some());
}

// ─── Surface 5: Legacy Text Completions Surface ─────────────────────────────

#[tokio::test]
async fn test_devin_surfaces_legacy_completions() {
    let mock = DevinMockProcess::start("default");
    let gw = start_devin_gateway("acc-legacy", "devin-tok-legacy", &mock.url).await;
    let client = reqwest::Client::new();

    // 1. POST /v1/completions with prompt
    let res = client
        .post(format!("{}/v1/completions", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "prompt": "Complete this sentence: In a galaxy far far",
            "stream": false
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["object"], "text_completion");
    assert!(body["choices"][0]["text"].as_str().is_some());

    // 2. Alias POST /completions
    let alias_res = client
        .post(format!("{}/completions", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "prompt": "Complete this sentence",
            "stream": false
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(alias_res.status(), StatusCode::OK);

    // 3. Reject tools on legacy completions
    let rej_res = client
        .post(format!("{}/v1/completions", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "prompt": "Hello",
            "tools": [{"type": "function", "name": "f"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(rej_res.status(), StatusCode::BAD_REQUEST);
}

// ─── Surface 6: Connect HTTP 200 Error Propagation Across Surfaces ───────────

#[tokio::test]
async fn test_devin_surfaces_error_propagation_across_surfaces() {
    let mock = DevinMockProcess::start("code-only-error");
    let gw = start_devin_gateway("acc-err", "devin-tok-err", &mock.url).await;
    let client = reqwest::Client::new();

    // 1. Anthropic surface error propagation
    let anth_res = client
        .post(format!("{}/v1/messages", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "error test"}],
            "stream": false
        }))
        .send()
        .await
        .unwrap();
    // Connect resource_exhausted must map to HTTP 429
    assert_eq!(anth_res.status(), StatusCode::TOO_MANY_REQUESTS);

    // Reset account health from Cooldown back to Available for the next test surface
    gw.state.pool.load().members[0].set_health(mahoquot_types::Health::Available);

    // 2. Responses surface error propagation
    let resp_res = client
        .post(format!("{}/v1/responses", gw.base_url))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "model": "devin/glm-5-2",
            "input": "error test",
            "stream": false
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp_res.status(), StatusCode::TOO_MANY_REQUESTS);

    // Reset account health from Cooldown back to Available for the next test surface
    gw.state.pool.load().members[0].set_health(mahoquot_types::Health::Available);

    // 3. Gemini surface error propagation
    let gem_res = client
        .post(format!(
            "{}/v1beta/models/devin/glm-5-2:generateContent",
            gw.base_url
        ))
        .header(header::AUTHORIZATION, "Bearer test-inbound-key")
        .json(&json!({
            "contents": [{
                "role": "user",
                "parts": [{"text": "error test"}]
            }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(gem_res.status(), StatusCode::TOO_MANY_REQUESTS);
}
