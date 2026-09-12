use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::StreamExt;
use http_body_util::BodyExt;
use mahoquot_types::PoolMember;
use mahoquot_gateway::account::{AccountMember, ProviderAccount};
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::devin_catalog::{
    fetch_devin_model_catalog, DevinAccountCatalogState, DevinCacheKey, DevinDiscoveryError,
    DiscoveredDevinModel,
};
use mahoquot_gateway::inbound::ApiKeys;
use mahoquot_gateway::management::settings::ScopedApiKey;
use mahoquot_gateway::models_route::scoped_model_entries;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use mahoquot_providers::devin::DevinAccount;
use mahoquot_registry::{
    ModelCapability, ModelId, ProviderId,
};
use prost::Message;
use serde_json::{json, Value};
use tower::ServiceExt;

#[path = "common/mod.rs"]
mod common;
use common::unique_temp_dir;

struct ChildGuard {
    child: Option<std::process::Child>,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct MockServerHandle {
    _guard: ChildGuard,
    _drain_handle: Option<std::thread::JoinHandle<()>>,
    pub url: String,
}

impl MockServerHandle {
    fn start(scenario: &str) -> Self {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let project_root = std::path::Path::new(manifest_dir)
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let script_path = project_root.join("scripts/devin-mock.mjs");
        let mut child = std::process::Command::new("bun")
            .arg(&script_path)
            .arg("--port")
            .arg("0")
            .arg("--scenario")
            .arg(scenario)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn devin-mock.mjs");

        let stdout = child.stdout.take().expect("capture stdout");
        let mut guard = ChildGuard { child: Some(child) };

        let (tx, rx) = std::sync::mpsc::channel();
        let drain_handle = std::thread::spawn(move || {
            use std::io::BufRead;
            let mut reader = std::io::BufReader::new(stdout);
            let mut line = String::new();
            if reader.read_line(&mut line).is_ok() {
                let _ = tx.send(line);
            }
            use std::io::Read;
            let mut discard = [0u8; 1024];
            while let Ok(n) = reader.read(&mut discard) {
                if n == 0 {
                    break;
                }
            }
        });

        let first_line = rx.recv_timeout(Duration::from_secs(5)).unwrap_or_else(|_| {
            let mut err_msg = String::new();
            if let Some(mut child) = guard.child.take() {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(mut stderr) = child.stderr.take() {
                    use std::io::Read;
                    let _ = stderr.read_to_string(&mut err_msg);
                }
            }
            panic!("timed out waiting for devin-mock readiness. Stderr: {err_msg}");
        });

        let readiness: Value = serde_json::from_str(&first_line)
            .unwrap_or_else(|e| panic!("failed to parse mock readiness JSON '{first_line}': {e}"));
        let url = readiness["url"].as_str().expect("url").to_string();

        Self {
            _guard: guard,
            _drain_handle: Some(drain_handle),
            url,
        }
    }
}

fn test_gateway_config(auth_dir: &Path) -> GatewayConfig {
    GatewayConfig {
        port: 0,
        auth_dir: auth_dir.to_path_buf(),
        strategy: mahoquot_types::Strategy::StrictRoundRobin,
        max_failover: 3,
        log_level: "info".to_string(),
        api_keys: ApiKeys::new(vec!["test-mgmt-key".to_string()]),
        models_env: None,
        refresh_url: mahoquot_providers::refresh::REFRESH_TOKEN_URL.to_string(),
        auth_refresh_enabled: false,
        usage_poll_secs: 120,
        config_path: auth_dir.join("config.yaml"),
        catalog_cache_path: None,
        history_queue_capacity: 1024,
        history_batch_size: 64,
    }
}

fn create_devin_account(id: &str, token: &str, base_url: &str, disabled: bool) -> DevinAccount {
    DevinAccount {
        provider_type: "devin".to_string(),
        identity_slug: id.to_string(),
        label: Some(id.to_string()),
        email: None,
        access_token: token.to_string(),
        api_server_url: base_url.to_string(),
        disabled,
    }
}

fn create_devin_member(
    id: &str,
    token: &str,
    base_url: &str,
    disabled: bool,
) -> Arc<AccountMember> {
    let account = create_devin_account(id, token, base_url, disabled);
    Arc::new(AccountMember::for_test_with_id(
        id,
        ProviderAccount::Devin(account),
    ))
}

fn create_test_model(uid: &str, label: &str, images: bool, premium: bool) -> DiscoveredDevinModel {
    DiscoveredDevinModel {
        model_uid: uid.to_string(),
        public_id: format!("devin/{uid}"),
        label: label.to_string(),
        supports_images: images,
        is_premium: premium,
        is_beta: false,
        is_recommended: false,
        is_new: false,
        is_capacity_limited: false,
        promo_active: false,
        max_tokens: Some(200000),
        credit_multiplier: Some(1.0f32),
        description: Some(format!("{label} test model")),
    }
}

#[tokio::test]
async fn test_management_contract_schema_and_parity_matrix_for_devin_models_refresh() {
    let schema_str = include_str!("../../../docs/management-contract-v1.schema.json");
    let schema: Value = serde_json::from_str(schema_str).expect("parse schema");

    let devin_refresh = &schema["$defs"]["devin-models-refresh"];
    assert!(
        devin_refresh.is_object(),
        "schema must define $defs['devin-models-refresh']"
    );

    let req = &devin_refresh["properties"]["request"];
    assert_eq!(req["type"], "object");

    let resp = &devin_refresh["properties"]["response"];
    assert_eq!(resp["type"], "object");
    assert_eq!(resp["properties"]["status"]["type"], "string");
    assert_eq!(resp["properties"]["generation"]["type"], "integer");
    assert_eq!(resp["properties"]["accounts"]["type"], "array");

    assert_eq!(
        schema["x-route-registration-owners"]["POST /v0/management/devin/models/refresh"],
        "crates/gateway/src/management/registry.rs"
    );

    let matrix = include_str!("../../../docs/parity-matrix.md");
    assert!(
        matrix.contains("POST /v0/management/devin/models/refresh"),
        "parity matrix must document Devin refresh endpoint"
    );
}

#[tokio::test]
async fn test_devin_catalog_wire_codec_and_transport_invariants() {
    let mock = MockServerHandle::start("default");
    let client = reqwest::Client::builder()
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("build client");

    // 1. Successful unframed protobuf discovery
    let models = fetch_devin_model_catalog(&client, &mock.url, "devin-dummy-account-a")
        .await
        .expect("fetch catalog");

    assert!(!models.is_empty(), "mock should return models");
    let glm = models.iter().find(|m| m.model_uid == "glm-5-2");
    assert!(glm.is_some(), "expected glm-5-2 model");
    let glm = glm.unwrap();
    assert_eq!(glm.public_id, "devin/glm-5-2");
    assert_eq!(glm.label, "GLM-5.2");
    assert!(glm.supports_images);
    assert_eq!(glm.max_tokens, Some(200000));
    assert_eq!(glm.credit_multiplier, None);

    // 2. HTTP error propagation (401 / 403 on invalid credentials)
    let err_server = axum::Router::new().route(
        "/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs",
        axum::routing::post(|| async {
            axum::http::Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(axum::body::Body::from("Unauthorized"))
                .unwrap()
        }),
    );
    let err_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let err_addr = err_listener.local_addr().unwrap();
    let err_handle = tokio::spawn(async move {
        axum::serve(err_listener, err_server).await.unwrap();
    });
    let auth_err = fetch_devin_model_catalog(&client, &format!("http://{err_addr}"), "invalid-token")
        .await
        .unwrap_err();
    assert!(matches!(auth_err, DevinDiscoveryError::HttpStatus { status } if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN));
    err_handle.abort();

    // 3. Rejection of non-proto content-type
    let non_proto_server = axum::Router::new().route(
        "/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs",
        axum::routing::post(|| async {
            axum::http::Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/json")
                .body(axum::body::Body::from("{}"))
                .unwrap()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, non_proto_server).await.unwrap();
    });
    let non_proto_err = fetch_devin_model_catalog(&client, &format!("http://{addr}"), "dummy-token")
        .await
        .unwrap_err();
    assert!(matches!(non_proto_err, DevinDiscoveryError::InvalidContentType));
    handle.abort();
}

#[tokio::test]
async fn test_devin_disjoint_account_routing_and_pool_snapshot() {
    let member_a = create_devin_member("devin-a", "token-a", "http://127.0.0.1:18899", false);
    let member_b = create_devin_member("devin-b", "token-b", "http://127.0.0.1:18899", false);

    let model_glm = create_test_model("glm-5-2", "GLM-5.2", true, true);
    let mut model_glm_max = create_test_model("glm-5-2-max", "GLM-5.2-Max", true, true);
    model_glm_max.credit_multiplier = Some(1.5f32);
    let model_swe = create_test_model("swe-1-7", "SWE-1.7", false, false);

    let now_unix = 1726000000;
    let key_a = DevinCacheKey::new("devin-a", "token-a", "http://127.0.0.1:18899");
    let key_b = DevinCacheKey::new("devin-b", "token-b", "http://127.0.0.1:18899");

    member_a.set_devin_catalog_state(Arc::new(DevinAccountCatalogState::new_success(
        key_a,
        vec![model_glm.clone(), model_glm_max.clone()],
        now_unix,
    )));

    member_b.set_devin_catalog_state(Arc::new(DevinAccountCatalogState::new_success(
        key_b,
        vec![model_swe.clone()],
        now_unix,
    )));

    let members = vec![member_a.clone(), member_b.clone()];
    let base_registry = Arc::new(mahoquot_registry::embedded_registry_snapshot().unwrap());

    let candidate = mahoquot_gateway::runtime_state::compute_candidate_composition(
        1,
        members.clone(),
        base_registry,
        None,
    )
    .expect("compute composition");

    // 1. Snapshot generation is atomic
    assert_eq!(candidate.generation(), 1);

    // 2. Models list in snapshot contains union of models with devin/<uid> public IDs
    let model_ids: Vec<String> = candidate.models().iter().map(|m| m.id.clone()).collect();
    assert!(
        model_ids.contains(&"devin/glm-5-2".to_string()),
        "candidate models must contain devin/glm-5-2"
    );
    assert!(
        model_ids.contains(&"devin/glm-5-2-max".to_string()),
        "candidate models must preserve suffix variant devin/glm-5-2-max"
    );
    assert!(
        model_ids.contains(&"devin/swe-1-7".to_string()),
        "candidate models must contain devin/swe-1-7"
    );

    // Verify owned_by is "devin"
    for m in candidate.models() {
        if m.id.starts_with("devin/") {
            assert_eq!(m.owned_by, "devin");
        }
    }

    // 3. Routing isolation: routable_accounts_for_model
    let routable_glm = candidate.routable_accounts_for_model("devin/glm-5-2");
    assert_eq!(routable_glm.len(), 1);
    assert_eq!(routable_glm[0].id, "devin-a");

    let routable_glm_max = candidate.routable_accounts_for_model("devin/glm-5-2-max");
    assert_eq!(routable_glm_max.len(), 1);
    assert_eq!(routable_glm_max[0].id, "devin-a");

    let routable_swe = candidate.routable_accounts_for_model("devin/swe-1-7");
    assert_eq!(routable_swe.len(), 1);
    assert_eq!(routable_swe[0].id, "devin-b");

    // 4. Base model does not alias suffix variant
    let unresolved_err = candidate
        .registry()
        .resolve("devin/glm-5-2-none")
        .unwrap_err();
    assert!(
        matches!(unresolved_err, mahoquot_registry::RegistryError::UnknownModel(_)),
        "unobserved suffix variant must not be resolved from base model"
    );
}

#[tokio::test]
async fn test_devin_account_pinning_scoped_keys_exclusions_unsupported() {
    let member_a = create_devin_member("devin-a", "token-a", "http://127.0.0.1:18899", false);
    let member_b = create_devin_member("devin-b", "token-b", "http://127.0.0.1:18899", false);

    let model_glm = create_test_model("glm-5-2", "GLM-5.2", true, true);
    let model_swe = create_test_model("swe-1-7", "SWE-1.7", false, false);

    let now_unix = 1726000000;
    member_a.set_devin_catalog_state(Arc::new(DevinAccountCatalogState::new_success(
        DevinCacheKey::new("devin-a", "token-a", "http://127.0.0.1:18899"),
        vec![model_glm.clone()],
        now_unix,
    )));

    member_b.set_devin_catalog_state(Arc::new(DevinAccountCatalogState::new_success(
        DevinCacheKey::new("devin-b", "token-b", "http://127.0.0.1:18899"),
        vec![model_swe.clone()],
        now_unix,
    )));

    let base_registry = Arc::new(mahoquot_registry::embedded_registry_snapshot().unwrap());
    let candidate = mahoquot_gateway::runtime_state::compute_candidate_composition(
        1,
        vec![member_a.clone(), member_b.clone()],
        Arc::clone(&base_registry),
        None,
    )
    .expect("composition");

    // 1. Scoped Key: Admitting only devin-a sees only devin/glm-5-2
    let scoped_a = ScopedApiKey {
        id: "scoped-key-a".to_string(),
        name: "Scope A".to_string(),
        key_identifier: "key_id_a".to_string(),
        key_prefix: "sk-a".to_string(),
        raw_key: None,
        allowed_providers: vec!["devin".to_string()],
        allowed_accounts: vec!["devin-a".to_string()],
        allowed_models: vec![],
        token_limit: 0,
        token_used: 0,
        is_active: true,
        created_at_ms: 0,
        expires_at_ms: None,
    };
    let entries_a = scoped_model_entries(&candidate, &scoped_a);
    let ids_a: Vec<&str> = entries_a.iter().map(|e| e.id.as_str()).collect();
    assert!(ids_a.contains(&"devin/glm-5-2"));
    assert!(!ids_a.contains(&"devin/swe-1-7"));

    // 2. Unsupported models cache: Marking glm-5-2 unsupported on member_a removes it from routing
    assert!(member_a.supports_devin_model("devin/glm-5-2", "devin/glm-5-2", "glm-5-2"));
    member_a.mark_model_unsupported("devin/glm-5-2");
    assert!(!member_a.supports_devin_model("devin/glm-5-2", "devin/glm-5-2", "glm-5-2"));
    let routable = candidate.routable_accounts_for_model("devin/glm-5-2");
    assert!(routable.is_empty());

    // 3. Exclusions: Exclusion rule prevents model from routing even if discovered
    let mut reg_with_exclusion = (*base_registry).clone();
    reg_with_exclusion.exclusions.insert(mahoquot_registry::ModelExclusionRule {
        model_id: ModelId::new("devin/swe-1-7").unwrap(),
        provider_id: Some(ProviderId::devin()),
    });
    let comp_with_exclusion = mahoquot_gateway::runtime_state::compute_candidate_composition(
        2,
        vec![member_b.clone()],
        Arc::new(reg_with_exclusion),
        None,
    )
    .unwrap();
    let routable_swe = comp_with_exclusion.routable_accounts_for_model("devin/swe-1-7");
    assert!(routable_swe.is_empty());
}

#[tokio::test]
async fn test_devin_unknown_model_no_codex_generic_fallback() {
    let base_registry = mahoquot_registry::embedded_snapshot();
    let err = base_registry.resolve("devin/unknown-model").unwrap_err();
    assert!(
        matches!(err, mahoquot_registry::RegistryError::UnknownModel(_)),
        "unknown devin model must not fall back to codex"
    );
}

#[tokio::test]
async fn test_devin_supports_images_vision_not_image_generation() {
    let member = create_devin_member("devin-vision", "token-v", "http://127.0.0.1:18899", false);
    let model = create_test_model("glm-5-2", "GLM-5.2", true, false);

    let now_unix = 1726000000;
    member.set_devin_catalog_state(Arc::new(DevinAccountCatalogState::new_success(
        DevinCacheKey::new("devin-vision", "token-v", "http://127.0.0.1:18899"),
        vec![model],
        now_unix,
    )));

    let base_registry = Arc::new(mahoquot_registry::embedded_registry_snapshot().unwrap());
    let candidate = mahoquot_gateway::runtime_state::compute_candidate_composition(
        1,
        vec![member.clone()],
        base_registry,
        None,
    )
    .expect("candidate composition");

    let resolved = candidate.registry().resolve("devin/glm-5-2").unwrap();
    assert!(resolved.effective_capabilities.contains(&ModelCapability::Chat));
    assert!(resolved.effective_capabilities.contains(&ModelCapability::Tools));
    assert!(
        !resolved.effective_capabilities.contains(&ModelCapability::Image),
        "supports_images must NOT grant ModelCapability::Image (image generation)"
    );
    assert!(member.devin_model_supports_vision("devin/glm-5-2"));
}

#[tokio::test]
async fn test_devin_cache_ttl_and_transient_failure_preservation() {
    let key = DevinCacheKey::new("devin-cache-test", "token-1", "http://127.0.0.1:18899");
    let model = create_test_model("glm-5-2", "GLM-5.2", true, false);

    let now_unix = 1726000000;

    // 1. Initial successful cache entry
    let mut state = DevinAccountCatalogState::new_success(
        key.clone(),
        vec![model.clone()],
        now_unix,
    );

    assert!(!state.is_stale(now_unix + 100));
    assert!(!state.is_stale(now_unix + 299));
    assert!(state.is_stale(now_unix + 300));
    assert!(state.is_stale(now_unix + 301));

    // 2. Transient failure preserves last known good (LKG) models and marks stale
    state.record_transient_failure("upstream 503 Service Unavailable", now_unix + 350);
    assert!(state.stale);
    assert_eq!(
        state.last_error.as_deref(),
        Some("upstream 503 Service Unavailable")
    );
    assert_eq!(state.models.len(), 1);
    assert_eq!(state.models[0].public_id, "devin/glm-5-2");
}

#[tokio::test]
async fn test_devin_management_refresh_endpoint_flow() {
    let mock = MockServerHandle::start("disjoint-catalogs");
    let auth_dir = unique_temp_dir("devin-mgmt-refresh");

    let acct_a = json!({
        "type": "devin",
        "identity_slug": "devin-a",
        "label": "Account A",
        "access_token": "devin-dummy-account-a",
        "api_server_url": mock.url,
        "disabled": false
    });
    std::fs::write(auth_dir.join("devin-a.json"), serde_json::to_string(&acct_a).unwrap()).unwrap();

    let config = test_gateway_config(&auth_dir);
    let state = Arc::new(AppState::new(&config).expect("create state"));
    let app = create_app(Arc::clone(&state));

    // 1. Missing Authorization header rejected with 401
    let missing_auth_req = Request::builder()
        .method("POST")
        .uri("/v0/management/devin/models/refresh")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&json!({"identity_slug": "devin-a"})).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(missing_auth_req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // 2. Wrong Authorization key rejected with 401
    let wrong_auth_req = Request::builder()
        .method("POST")
        .uri("/v0/management/devin/models/refresh")
        .header("authorization", "Bearer wrong-key")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&json!({"identity_slug": "devin-a"})).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(wrong_auth_req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // 3. Valid Authorization key succeeds with 200 OK
    let req = Request::builder()
        .method("POST")
        .uri("/v0/management/devin/models/refresh")
        .header("authorization", "Bearer test-mgmt-key")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&json!({"identity_slug": "devin-a"})).unwrap()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let res: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(res["status"], "ok");
    assert_eq!(res["outcome"], "success");
    assert!(res["generation"].as_u64().unwrap() > 0);

    let models = res["models"].as_array().unwrap();
    let model_names: Vec<&str> = models.iter().filter_map(|v| v.as_str()).collect();
    assert!(model_names.contains(&"devin/glm-5-2"));

    // 4. Verify /v1/models reflects the refreshed Devin models with owned_by: devin
    let models_req = Request::builder()
        .method("GET")
        .uri("/v1/models")
        .header("authorization", "Bearer test-mgmt-key")
        .body(Body::empty())
        .unwrap();
    let models_resp = app.clone().oneshot(models_req).await.unwrap();
    assert_eq!(models_resp.status(), StatusCode::OK);
    let models_body = models_resp.into_body().collect().await.unwrap().to_bytes();
    let models_json: Value = serde_json::from_slice(&models_body).unwrap();
    assert_eq!(models_json["object"], "list");
    let model_items = models_json["data"].as_array().expect("data array");
    let found_glm = model_items.iter().any(|m| m["id"] == "devin/glm-5-2" && m["owned_by"] == "devin");
    assert!(found_glm, "devin/glm-5-2 must be visible in /v1/models with owned_by: devin");

    // Check GET /admin/stats serialization
    let stats = state.get_stats();
    let acct_stats = stats.accounts.iter().find(|a| a.id == "devin-a").expect("find account");
    assert_eq!(acct_stats.provider, "devin");
    assert!(acct_stats.models.is_some());
    let models_stats = acct_stats.models.as_ref().unwrap();
    assert!(models_stats.contains(&"devin/glm-5-2".to_string()));
}

#[tokio::test]
async fn test_devin_old_snapshot_hold_immutability() {
    let mock = MockServerHandle::start("disjoint-catalogs");
    let auth_dir = unique_temp_dir("devin-snap-hold");

    let acct_a = json!({
        "type": "devin",
        "identity_slug": "devin-a",
        "label": "Account A",
        "access_token": "devin-dummy-account-a",
        "api_server_url": mock.url,
        "disabled": false
    });
    std::fs::write(auth_dir.join("devin-a.json"), serde_json::to_string(&acct_a).unwrap()).unwrap();

    let config = test_gateway_config(&auth_dir);
    let state = Arc::new(AppState::new(&config).expect("create state"));

    // Hold Generation 0 snapshot Arc
    let snap_v0 = state.pool.load_full();
    let initial_gen = snap_v0.generation();
    assert!(snap_v0.models().is_empty());
    assert_eq!(snap_v0.members()[0].devin_models(), None);

    // Execute refresh to discover models
    let member = state.find_member("devin-a").expect("find member");
    let client = state.devin_client_for_member(&member).expect("devin client");
    let new_cat = mahoquot_gateway::devin_catalog::refresh_account_models(&member, &client, Some(&state.devin_cache))
        .await
        .expect("refresh account");
    let rev = new_cat.key.credential_revision.clone();
    let comp_v1 = state.runtime.publish_devin_member_catalog(
        "devin-a",
        &rev,
        new_cat,
        &state.devin_cache,
    ).expect("publish runtime");

    // New snapshot has incremented generation and discovered models
    assert_eq!(comp_v1.generation(), initial_gen + 1);
    let new_models: Vec<String> = comp_v1.models().iter().map(|m| m.id.clone()).collect();
    assert!(new_models.contains(&"devin/glm-5-2".to_string()));

    // CRITICAL INVARIANT: Held snap_v0 must remain completely unchanged!
    assert_eq!(snap_v0.generation(), initial_gen);
    assert!(snap_v0.models().is_empty());
    assert_eq!(snap_v0.members()[0].devin_models(), None);
}

#[test]
fn test_devin_concurrent_discovery_cache_rcu() {
    use mahoquot_gateway::devin_catalog::DevinDiscoveryCache;
    use std::sync::Barrier;

    let cache = Arc::new(DevinDiscoveryCache::new());
    let now_unix = 1726000000;
    const NUM_THREADS: usize = 50;
    let barrier = Arc::new(Barrier::new(NUM_THREADS));
    let mut handles = Vec::with_capacity(NUM_THREADS);

    for i in 0..NUM_THREADS {
        let cache_clone = Arc::clone(&cache);
        let barrier_clone = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let key = DevinCacheKey::new(format!("devin-{i}"), &format!("tok-{i}"), "http://127.0.0.1:18899");
            let state = Arc::new(DevinAccountCatalogState::new_success(
                key.clone(),
                vec![create_test_model(&format!("model-{i}"), &format!("Model {i}"), false, false)],
                now_unix,
            ));
            // Synchronize all threads to fire simultaneously at the barrier
            barrier_clone.wait();
            cache_clone.insert(key, state);
        }));
    }

    for h in handles {
        h.join().expect("thread join");
    }

    // Assert that zero updates were lost due to CAS/rcu
    for i in 0..NUM_THREADS {
        let key = DevinCacheKey::new(format!("devin-{i}"), &format!("tok-{i}"), "http://127.0.0.1:18899");
        let entry = cache.get(&key);
        assert!(entry.is_some(), "entry {i} must exist in cache");
        assert_eq!(entry.unwrap().models[0].public_id, format!("devin/model-{i}"));
    }
}

#[tokio::test]
async fn test_devin_known_empty_catalog_is_valid_stale_lkg() {
    use axum::routing::post;
    use mahoquot_gateway::compat::devin_proto::GetCascadeModelConfigsResponse;

    let request_count = Arc::new(AtomicUsize::new(0));
    let count_clone = Arc::clone(&request_count);

    let app = axum::Router::new().route(
        "/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs",
        post(move || {
            let count = count_clone.fetch_add(1, Ordering::SeqCst);
            async move {
                if count == 0 {
                    // First request: 200 OK with empty proto models list
                    let resp_proto = GetCascadeModelConfigsResponse {
                        client_model_configs: Vec::new(),
                    };
                    let mut body = Vec::new();
                    resp_proto.encode(&mut body).unwrap();
                    axum::http::Response::builder()
                        .status(StatusCode::OK)
                        .header("Content-Type", "application/proto")
                        .body(axum::body::Body::from(body))
                        .unwrap()
                } else {
                    // Subsequent request: transient HTTP 503 error
                    axum::http::Response::builder()
                        .status(StatusCode::SERVICE_UNAVAILABLE)
                        .body(axum::body::Body::from("Service Unavailable"))
                        .unwrap()
                }
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let member = create_devin_member("devin-empty", "tok", &format!("http://{addr}"), false);
    let client = reqwest::Client::builder()
        .http1_only()
        .no_proxy()
        .build()
        .unwrap();
    let cache = mahoquot_gateway::devin_catalog::DevinDiscoveryCache::new();

    // 1. First real refresh via HTTP: returns 200 OK with 0 models
    let state1 = mahoquot_gateway::devin_catalog::refresh_account_models(&member, &client, Some(&cache))
        .await
        .expect("initial empty discovery must succeed");
    assert!(!state1.stale);
    assert!(state1.has_succeeded);
    assert!(state1.models.is_empty());
    assert_eq!(state1.last_error, None);
    member.set_devin_catalog_state(Arc::clone(&state1));
    cache.insert(state1.key.clone(), Arc::clone(&state1));
    assert_eq!(member.devin_models(), Some(vec![]));

    // 2. Second real refresh via HTTP: transient 503 failure
    // Known-empty catalog must be preserved as valid stale LKG!
    let state2 = mahoquot_gateway::devin_catalog::refresh_account_models(&member, &client, Some(&cache))
        .await
        .expect("transient failure with existing empty catalog must return stale LKG Ok");
    assert!(state2.stale);
    assert!(state2.has_succeeded);
    assert!(state2.models.is_empty());
    assert!(state2.last_error.is_some());
    member.set_devin_catalog_state(Arc::clone(&state2));
    assert_eq!(member.devin_models(), Some(vec![]));

    server.abort();
}

#[tokio::test]
async fn test_devin_channel_gated_credential_rotation_race() {
    use axum::routing::post;
    use tokio::sync::oneshot;

    let (rpc_entered_tx, rpc_entered_rx) = oneshot::channel::<()>();
    let (rpc_release_tx, rpc_release_rx) = oneshot::channel::<()>();
    let entered_tx = Arc::new(tokio::sync::Mutex::new(Some(rpc_entered_tx)));
    let release_rx = Arc::new(tokio::sync::Mutex::new(Some(rpc_release_rx)));

    let app = axum::Router::new().route(
        "/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs",
        post(move || {
            let entered = Arc::clone(&entered_tx);
            let release = Arc::clone(&release_rx);
            async move {
                if let Some(tx) = entered.lock().await.take() {
                    let _ = tx.send(());
                }
                if let Some(rx) = release.lock().await.take() {
                    let _ = rx.await;
                }
                use mahoquot_gateway::compat::devin_proto::GetCascadeModelConfigsResponse;
                let resp_proto = GetCascadeModelConfigsResponse {
                    client_model_configs: Vec::new(),
                };
                let mut body = Vec::new();
                resp_proto.encode(&mut body).unwrap();
                axum::http::Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "application/proto")
                    .body(axum::body::Body::from(body))
                    .unwrap()
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let member = create_devin_member("devin-race", "token-initial", &format!("http://{addr}"), false);
    let client = reqwest::Client::builder()
        .http1_only()
        .no_proxy()
        .build()
        .unwrap();

    // Spawn discovery in background
    let member_clone = Arc::clone(&member);
    let refresh_task = tokio::spawn(async move {
        mahoquot_gateway::devin_catalog::refresh_account_models(&member_clone, &client, None).await
    });

    // Wait until RPC has entered and is in flight
    rpc_entered_rx.await.unwrap();

    // Rotate credential while RPC is in flight
    {
        let mut guard = member.inner.write().unwrap();
        *guard = ProviderAccount::Devin(create_devin_account(
            "devin-race",
            "token-rotated",
            &format!("http://{addr}"),
            false,
        ));
    }

    // Release RPC response
    rpc_release_tx.send(()).unwrap();

    // Refresh must reject publication due to credential revision mismatch
    let err = refresh_task.await.unwrap().unwrap_err();
    assert!(
        matches!(err, DevinDiscoveryError::StalePublication),
        "in-flight credential rotation must reject stale publication, got: {err:?}"
    );

    server.abort();
}

#[tokio::test]
async fn test_devin_channel_gated_account_disabled_race() {
    use axum::routing::post;
    use tokio::sync::oneshot;

    let (rpc_entered_tx, rpc_entered_rx) = oneshot::channel::<()>();
    let (rpc_release_tx, rpc_release_rx) = oneshot::channel::<()>();
    let entered_tx = Arc::new(tokio::sync::Mutex::new(Some(rpc_entered_tx)));
    let release_rx = Arc::new(tokio::sync::Mutex::new(Some(rpc_release_rx)));

    let app = axum::Router::new().route(
        "/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs",
        post(move || {
            let entered = Arc::clone(&entered_tx);
            let release = Arc::clone(&release_rx);
            async move {
                if let Some(tx) = entered.lock().await.take() {
                    let _ = tx.send(());
                }
                if let Some(rx) = release.lock().await.take() {
                    let _ = rx.await;
                }
                use mahoquot_gateway::compat::devin_proto::GetCascadeModelConfigsResponse;
                let resp_proto = GetCascadeModelConfigsResponse {
                    client_model_configs: Vec::new(),
                };
                let mut body = Vec::new();
                resp_proto.encode(&mut body).unwrap();
                axum::http::Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "application/proto")
                    .body(axum::body::Body::from(body))
                    .unwrap()
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let member = create_devin_member("devin-disabled-race", "token-init", &format!("http://{addr}"), false);
    let client = reqwest::Client::builder()
        .http1_only()
        .no_proxy()
        .build()
        .unwrap();

    let member_clone = Arc::clone(&member);
    let refresh_task = tokio::spawn(async move {
        mahoquot_gateway::devin_catalog::refresh_account_models(&member_clone, &client, None).await
    });

    rpc_entered_rx.await.unwrap();

    // Disable member while RPC is in flight
    member.set_health(mahoquot_types::Health::Disabled);

    rpc_release_tx.send(()).unwrap();

    let err = refresh_task.await.unwrap().unwrap_err();
    assert!(
        matches!(err, DevinDiscoveryError::StalePublication),
        "disabling member during RPC must reject stale publication, got: {err:?}"
    );

    server.abort();
}

#[tokio::test]
async fn test_devin_same_token_endpoint_change_race() {
    let auth_dir = unique_temp_dir("devin-endpoint-race");
    let acct = json!({
        "type": "devin",
        "identity_slug": "devin-endpoint",
        "label": "Account Endpoint",
        "access_token": "devin-token-stable",
        "api_server_url": "http://127.0.0.1:18899",
        "disabled": false
    });
    std::fs::write(auth_dir.join("devin-endpoint.json"), serde_json::to_string(&acct).unwrap()).unwrap();

    let config = test_gateway_config(&auth_dir);
    let state = Arc::new(AppState::new(&config).expect("create state"));

    let initial_key = DevinCacheKey::new("devin-endpoint", "devin-token-stable", "http://127.0.0.1:18899");
    let catalog = Arc::new(DevinAccountCatalogState::new_success(
        initial_key.clone(),
        vec![create_test_model("model-1", "Model 1", false, false)],
        1726000000,
    ));

    // Given an endpoint replacement loaded through the real credential path.
    let mut replacement = acct;
    replacement["api_server_url"] = json!("http://127.0.0.1:28899");
    std::fs::write(
        auth_dir.join("devin-endpoint.json"),
        serde_json::to_string(&replacement).unwrap(),
    ).unwrap();
    state.rescan_pool().unwrap();

    // When an in-flight catalog attempts publication with the old endpoint
    let rev = mahoquot_gateway::devin_catalog::compute_credential_revision("devin-token-stable");
    let res = state.runtime.publish_devin_member_catalog(
        "devin-endpoint",
        &rev,
        catalog,
        &state.devin_cache,
    );

    // Then the endpoint mismatch rejects publication even though the token is unchanged.
    assert!(
        matches!(res, Err(DevinDiscoveryError::StalePublication)),
        "publication must reject catalog with mismatched effective base URL"
    );
    std::fs::remove_dir_all(auth_dir).unwrap();
}

#[tokio::test]
async fn test_devin_chat_completion_runtime_identity_preserved_across_catalog_refresh() {
    let mock = MockServerHandle::start("disjoint-catalogs");
    let auth_dir = unique_temp_dir("devin-identity-preserve");

    let acct = json!({
        "type": "devin",
        "identity_slug": "devin-chat",
        "label": "Account Chat",
        "access_token": "devin-dummy-account-a",
        "api_server_url": mock.url,
        "disabled": false
    });
    std::fs::write(auth_dir.join("devin-chat.json"), serde_json::to_string(&acct).unwrap()).unwrap();

    let config = test_gateway_config(&auth_dir);
    let state = Arc::new(AppState::new(&config).expect("create state"));

    // Hold Generation 0 member (as an in-flight Chat completion would)
    let member_v0 = state.find_member("devin-chat").expect("find member");
    assert_eq!(member_v0.ok_count.load(Ordering::Relaxed), 0);
    let initial_gen = state.runtime.generation();

    // Refresh discovery, publishing incremented snapshot
    let client = state.devin_client_for_member(&member_v0).expect("client");
    let new_cat = mahoquot_gateway::devin_catalog::refresh_account_models(&member_v0, &client, Some(&state.devin_cache))
        .await
        .expect("refresh");
    let rev = new_cat.key.credential_revision.clone();
    let comp_v1 = state.runtime.publish_devin_member_catalog(
        "devin-chat",
        &rev,
        new_cat,
        &state.devin_cache,
    ).expect("publish runtime");
    assert_eq!(comp_v1.generation(), initial_gen + 1);

    // Get Generation 1 member from current pool
    let member_v1 = comp_v1.find_member("devin-chat").expect("find member v1");

    // In-flight Chat completes on member_v0!
    member_v0.record_ok();
    member_v0.record_ok();
    member_v0.record_fail();
    member_v0.set_health(mahoquot_types::Health::Cooldown { until_unix_ms: 9999999999 });

    // CRITICAL INVARIANT: The mutable runtime identity is preserved!
    // Current pool member_v1 sees the ok/fail counts and cooldown immediately!
    assert!(member_v0.same_credential_identity(&member_v1));
    assert!(member_v0.shares_runtime_identity(&member_v1));
    assert_eq!(member_v1.ok_count.load(Ordering::Relaxed), 2);
    assert_eq!(member_v1.fail_count.load(Ordering::Relaxed), 1);
    assert_eq!(
        member_v1.health(),
        mahoquot_types::Health::Cooldown { until_unix_ms: 9999999999 }
    );

    let stats = state.get_stats();
    let stat = stats.accounts.iter().find(|a| a.id == "devin-chat").unwrap();
    assert_eq!(stat.ok, 2);
    assert_eq!(stat.fails, 1);
}

#[tokio::test]
async fn test_devin_credential_replacement_creates_distinct_runtime_identity() {
    let auth_dir = unique_temp_dir("devin-replacement-distinct");

    let file_path = auth_dir.join("devin-test.json");
    let acct1 = json!({
        "type": "devin",
        "identity_slug": "devin-test",
        "label": "Account 1",
        "access_token": "token-a",
        "api_server_url": "https://endpoint-a.devin.ai",
        "disabled": false
    });
    std::fs::write(&file_path, serde_json::to_string(&acct1).unwrap()).unwrap();

    let config = test_gateway_config(&auth_dir);
    let state = Arc::new(AppState::new(&config).expect("create state"));

    let member1 = state.find_member("devin-test").expect("find member1");
    member1.record_ok();
    member1.record_ok();
    member1.record_fail();
    assert_eq!(member1.ok_count.load(Ordering::Relaxed), 2);
    assert_eq!(member1.fail_count.load(Ordering::Relaxed), 1);

    // Case 1: Same token, but endpoint URL changed (re-pointed upstream)
    let acct2 = json!({
        "type": "devin",
        "identity_slug": "devin-test",
        "label": "Account 2",
        "access_token": "token-a", // same token!
        "api_server_url": "https://endpoint-b.devin.ai", // different endpoint
        "disabled": false
    });
    std::fs::write(&file_path, serde_json::to_string(&acct2).unwrap()).unwrap();

    state.rescan_pool().expect("rescan after endpoint change");
    let member2 = state.find_member("devin-test").expect("find member2");

    // True credential replacement: must NOT share runtime identity cells
    assert!(!member1.same_credential_identity(&member2));
    assert!(!member1.shares_runtime_identity(&member2));
    assert_eq!(member2.ok_count.load(Ordering::Relaxed), 0);
    assert_eq!(member2.fail_count.load(Ordering::Relaxed), 0);

    // Case 2: Endpoint same, but token changed (rotation)
    let acct3 = json!({
        "type": "devin",
        "identity_slug": "devin-test",
        "label": "Account 3",
        "access_token": "token-b", // rotated token
        "api_server_url": "https://endpoint-b.devin.ai",
        "disabled": false
    });
    std::fs::write(&file_path, serde_json::to_string(&acct3).unwrap()).unwrap();

    state.rescan_pool().expect("rescan after rotation");
    let member3 = state.find_member("devin-test").expect("find member3");

    assert!(!member2.same_credential_identity(&member3));
    assert!(!member2.shares_runtime_identity(&member3));
    assert_eq!(member3.ok_count.load(Ordering::Relaxed), 0);
    assert_eq!(member3.fail_count.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn test_devin_generation_consistent_snapshot_permissions_isolation() {
    let auth_dir = unique_temp_dir("devin-gen-isolation");

    let file_path = auth_dir.join("devin-iso.json");
    let acct_gen0 = json!({
        "type": "devin",
        "identity_slug": "devin-iso",
        "label": "Account Gen0",
        "access_token": "token-gen0",
        "api_server_url": "http://endpoint-gen0.example.com",
        "disabled": false
    });
    std::fs::write(&file_path, serde_json::to_string(&acct_gen0).unwrap()).unwrap();

    let config = test_gateway_config(&auth_dir);
    let state = Arc::new(AppState::new(&config).expect("create state"));

    // Set up Gen0 catalog: contains only devin/glm-5-2 with vision
    let _member_gen0 = state.find_member("devin-iso").expect("find member");
    let key_gen0 = DevinCacheKey::new("devin-iso", "token-gen0", "http://endpoint-gen0.example.com");
    let cat_gen0 = Arc::new(DevinAccountCatalogState::new_success(
        key_gen0.clone(),
        vec![create_test_model("glm-5-2", "GLM 5.2", true, false)],
        1726000000,
    ));
    let rev_gen0 = mahoquot_gateway::devin_catalog::compute_credential_revision("token-gen0");
    let snap_gen0 = state.runtime.publish_devin_member_catalog(
        "devin-iso",
        &rev_gen0,
        cat_gen0,
        &state.devin_cache,
    ).expect("publish gen0");

    let gen0_num = snap_gen0.generation();

    // Verify Gen0 permissions
    assert_eq!(snap_gen0.routable_accounts_for_model("devin/glm-5-2").len(), 1);
    assert_eq!(snap_gen0.routable_accounts_for_model("devin/swe-1-7").len(), 0);
    assert_eq!(snap_gen0.devin_credential_revision("devin-iso"), Some("token-gen0"));
    assert_eq!(snap_gen0.devin_effective_base_url("devin-iso"), Some("http://endpoint-gen0.example.com"));
    assert!(snap_gen0.devin_model_supports_vision("devin-iso", "devin/glm-5-2"));
    assert!(!snap_gen0.devin_model_supports_vision("devin-iso", "devin/swe-1-7"));

    // Step 2: Credential replacement + different models in Gen1
    let acct_gen1 = json!({
        "type": "devin",
        "identity_slug": "devin-iso",
        "label": "Account Gen1",
        "access_token": "token-gen1",
        "api_server_url": "http://endpoint-gen1.example.com",
        "disabled": false
    });
    std::fs::write(&file_path, serde_json::to_string(&acct_gen1).unwrap()).unwrap();
    state.rescan_pool().expect("rescan pool");

    // Publish Gen1 catalog: contains only devin/swe-1-7 without vision
    let _member_gen1 = state.find_member("devin-iso").expect("find member gen1");
    let key_gen1 = DevinCacheKey::new("devin-iso", "token-gen1", "http://endpoint-gen1.example.com");
    let cat_gen1 = Arc::new(DevinAccountCatalogState::new_success(
        key_gen1.clone(),
        vec![create_test_model("swe-1-7", "SWE 1.7", false, false)],
        1726000005,
    ));
    let rev_gen1 = mahoquot_gateway::devin_catalog::compute_credential_revision("token-gen1");
    let snap_gen1 = state.runtime.publish_devin_member_catalog(
        "devin-iso",
        &rev_gen1,
        cat_gen1,
        &state.devin_cache,
    ).expect("publish gen1");

    assert!(snap_gen1.generation() > gen0_num);

    // Verify Gen1 permissions
    assert_eq!(snap_gen1.routable_accounts_for_model("devin/swe-1-7").len(), 1);
    assert_eq!(snap_gen1.routable_accounts_for_model("devin/glm-5-2").len(), 0);
    assert_eq!(snap_gen1.devin_credential_revision("devin-iso"), Some("token-gen1"));
    assert_eq!(snap_gen1.devin_effective_base_url("devin-iso"), Some("http://endpoint-gen1.example.com"));
    assert!(!snap_gen1.devin_model_supports_vision("devin-iso", "devin/swe-1-7"));

    // Step 3: CRITICAL INVARIANT: snap_gen0 held across replacement remains completely isolated!
    // It must NEVER mix credentials, endpoints, or model permissions from Gen1!
    assert_eq!(snap_gen0.generation(), gen0_num);
    assert_eq!(snap_gen0.routable_accounts_for_model("devin/glm-5-2").len(), 1);
    assert_eq!(snap_gen0.routable_accounts_for_model("devin/swe-1-7").len(), 0);
    assert_eq!(snap_gen0.devin_credential_revision("devin-iso"), Some("token-gen0"));
    assert_eq!(snap_gen0.devin_effective_base_url("devin-iso"), Some("http://endpoint-gen0.example.com"));
    assert!(snap_gen0.devin_model_supports_vision("devin-iso", "devin/glm-5-2"));
    assert!(!snap_gen0.devin_model_supports_vision("devin-iso", "devin/swe-1-7"));
}

#[tokio::test]
async fn test_devin_invalid_credentialed_proxy_prevents_direct_leak_and_masks_credentials() {
    let requests_received = Arc::new(AtomicUsize::new(0));
    let req_counter = Arc::clone(&requests_received);

    // Mock an upstream server
    let app = axum::Router::new().route(
        "/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs",
        axum::routing::post(move || {
            req_counter.fetch_add(1, Ordering::SeqCst);
            async { axum::http::StatusCode::OK }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let auth_dir = unique_temp_dir("devin-proxy-leak");
    let acct = json!({
        "type": "devin",
        "identity_slug": "devin-leak-test",
        "label": "Account Leak Test",
        "access_token": "devin-token",
        "api_server_url": format!("http://{upstream_addr}"),
        "disabled": false
    });
    std::fs::write(auth_dir.join("devin-leak-test.json"), serde_json::to_string(&acct).unwrap()).unwrap();

    let config = test_gateway_config(&auth_dir);
    let state = Arc::new(AppState::new(&config).expect("create state"));

    // Set invalid proxy with secret password in settings
    let secret_pass = "super_secret_password_xyz_9988";
    let proxy_with_creds = format!("http://leak_user:{secret_pass}@127.0.0.1:65534");
    state.settings.mutate(|s| {
        let mut providers = s.proxy_providers.clone();
        providers.insert(
            "devin".to_string(),
            mahoquot_gateway::management::settings::ProviderProxyPolicy {
                enabled: true,
                url: proxy_with_creds.clone(),
                sticky: false,
                ttl_secs: 0,
            },
        );
        s.proxy_providers = providers;
    }).unwrap();

    let member = state.find_member("devin-leak-test").unwrap();
    let client = state.devin_client_for_member(&member).expect("client built");

    // Execute discovery through the proxied client
    let fetch_res = fetch_devin_model_catalog(&client, &format!("http://{upstream_addr}"), "devin-token").await;

    // Must fail due to proxy connection failure
    let err = fetch_res.unwrap_err();

    // CRITICAL INVARIANT 1: Zero direct upstream traffic occurred!
    assert_eq!(
        requests_received.load(Ordering::SeqCst),
        0,
        "direct upstream must not receive traffic when proxy is configured"
    );

    // CRITICAL INVARIANT 2: Error string does not contain password or userinfo
    let err_str = err.to_string();
    let safe_msg = err.safe_message();
    assert!(
        !err_str.contains(secret_pass),
        "error display must not leak proxy password"
    );
    assert!(
        !safe_msg.contains(secret_pass),
        "safe_message must not leak proxy password"
    );
    assert!(
        !err_str.contains("leak_user"),
        "error display must not leak proxy username"
    );

    server.abort();
}

#[tokio::test]
async fn test_devin_oversized_streaming_body_rejected_incrementally() {
    use axum::routing::post;
    use bytes::Bytes;
    use futures::stream;

    // Mock an upstream that streams chunks exceeding 16 MiB
    let app = axum::Router::new().route(
        "/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs",
        post(|| async {
            // Stream 17 chunks of 1 MiB each = 17 MiB > 16 MiB
            let chunk = Bytes::from(vec![0u8; 1024 * 1024]);
            let stream = stream::repeat(chunk).take(17);
            let body = axum::body::Body::from_stream(stream.map(Ok::<_, std::io::Error>));
            axum::http::Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/proto")
                .body(body)
                .unwrap()
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::builder()
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let err = fetch_devin_model_catalog(&client, &format!("http://{addr}"), "tok")
        .await
        .unwrap_err();

    assert!(
        matches!(err, DevinDiscoveryError::BodyTooLarge),
        "oversized body must be rejected with BodyTooLarge, got: {err:?}"
    );

    server.abort();
}

#[tokio::test]
async fn test_devin_discovery_timeout_bounds_body_reading() {
    use axum::routing::post;

    // Mock an upstream that delays emitting chunks beyond timeout
    let app = axum::Router::new().route(
        "/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs",
        post(|| async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            axum::http::Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/proto")
                .body(axum::body::Body::from("too-late"))
                .unwrap()
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::builder()
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_millis(100))
        .build()
        .unwrap();

    let err = fetch_devin_model_catalog(&client, &format!("http://{addr}"), "tok")
        .await
        .unwrap_err();

    assert!(
        matches!(err, DevinDiscoveryError::Timeout),
        "delayed body reading must time out, got: {err:?}"
    );

    server.abort();
}

#[test]
fn test_account_snapshot_permissions_debug_redacts_token() {
    let perms = mahoquot_gateway::runtime_state::AccountSnapshotPermissions {
        identity_slug: "devin-test".to_string(),
        provider_kind: mahoquot_gateway::account::ProviderKind::Devin,
        disabled: false,
        access_token: "super-secret-devin-token-xyz-12345".to_string(),
        effective_base_url: "https://api.devin.example.com".to_string(),
        devin_models: Some(vec!["devin/glm-5-2".to_string()]),
        devin_discovered: None,
        unsupported_models: vec![],
    };

    let debug_repr = format!("{perms:?}");
    assert!(
        !debug_repr.contains("super-secret-devin-token-xyz-12345"),
        "AccountSnapshotPermissions Debug representation MUST NOT leak access_token! Got: {debug_repr}"
    );
    assert!(
        debug_repr.contains("[REDACTED]"),
        "AccountSnapshotPermissions Debug representation must contain [REDACTED] for access_token! Got: {debug_repr}"
    );
}

#[tokio::test]
async fn test_devin_discovery_exact_mime_essence_validation() {
    let current_ct = Arc::new(std::sync::Mutex::new("application/proto".to_string()));
    let current_ct_clone = Arc::clone(&current_ct);

    let server = axum::Router::new().route(
        "/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs",
        axum::routing::post(move || {
            let ct = current_ct_clone.lock().unwrap().clone();
            async move {
                axum::http::Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", ct)
                    .body(axum::body::Body::from(vec![]))
                    .unwrap()
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, server).await.unwrap();
    });

    let client = reqwest::Client::builder()
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("build client");

    let base_url = format!("http://{addr}");

    // Exact match: application/proto
    *current_ct.lock().unwrap() = "application/proto".to_string();
    let res = fetch_devin_model_catalog(&client, &base_url, "dummy-tok").await;
    assert!(res.is_ok(), "application/proto must be accepted");

    // Exact MIME essence with parameter: application/proto; charset=utf-8
    *current_ct.lock().unwrap() = "application/proto; charset=utf-8".to_string();
    let res = fetch_devin_model_catalog(&client, &base_url, "dummy-tok").await;
    assert!(res.is_ok(), "application/proto; charset=utf-8 must be accepted");

    // Invalid suffix MIME type: application/protobuf
    *current_ct.lock().unwrap() = "application/protobuf".to_string();
    let res = fetch_devin_model_catalog(&client, &base_url, "dummy-tok").await;
    assert!(
        matches!(res, Err(DevinDiscoveryError::InvalidContentType)),
        "application/protobuf must be rejected with InvalidContentType, got: {res:?}"
    );

    // Invalid suffix MIME type: application/proto-custom
    *current_ct.lock().unwrap() = "application/proto-custom".to_string();
    let res = fetch_devin_model_catalog(&client, &base_url, "dummy-tok").await;
    assert!(
        matches!(res, Err(DevinDiscoveryError::InvalidContentType)),
        "application/proto-custom must be rejected with InvalidContentType, got: {res:?}"
    );

    // Invalid suffix MIME type: application/protocol-buffers
    *current_ct.lock().unwrap() = "application/protocol-buffers".to_string();
    let res = fetch_devin_model_catalog(&client, &base_url, "dummy-tok").await;
    assert!(
        matches!(res, Err(DevinDiscoveryError::InvalidContentType)),
        "application/protocol-buffers must be rejected with InvalidContentType, got: {res:?}"
    );

    server_handle.abort();
}

#[tokio::test]
async fn test_devin_initial_refresh_failure_publishes_catalog_and_subsequent_get_reports_error() {
    // Upstream server returns 401 Unauthorized
    let err_server = axum::Router::new().route(
        "/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs",
        axum::routing::post(|| async {
            axum::http::Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(axum::body::Body::from("Unauthorized"))
                .unwrap()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, err_server).await.unwrap();
    });

    let auth_dir = unique_temp_dir("devin-initial-fail-test");
    let acct = json!({
        "type": "devin",
        "identity_slug": "devin-fail",
        "label": "Account Fail",
        "access_token": "token-fail",
        "api_server_url": format!("http://{addr}"),
        "disabled": false
    });
    std::fs::write(auth_dir.join("devin-fail.json"), serde_json::to_string(&acct).unwrap()).unwrap();

    let config = test_gateway_config(&auth_dir);
    let state = Arc::new(AppState::new(&config).expect("create state"));
    let app = create_app(Arc::clone(&state));

    // Initial refresh attempt fails
    let req = Request::builder()
        .method("POST")
        .uri("/v0/management/devin/models/refresh")
        .header("authorization", "Bearer test-mgmt-key")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&json!({"identity_slug": "devin-fail"})).unwrap()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let res: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(res["outcome"], "error");

    // Subsequent GET /v0/management/devin/models/status MUST reflect the recorded failure, not uninitialized/null
    let status_req = Request::builder()
        .method("GET")
        .uri("/v0/management/devin/models/status")
        .header("authorization", "Bearer test-mgmt-key")
        .body(Body::empty())
        .unwrap();

    let status_resp = app.clone().oneshot(status_req).await.unwrap();
    assert_eq!(status_resp.status(), StatusCode::OK);
    let status_body = status_resp.into_body().collect().await.unwrap().to_bytes();
    let status_json: Value = serde_json::from_slice(&status_body).unwrap();
    let accts = status_json["accounts"].as_array().expect("accounts array");
    let acct_status = accts.iter().find(|a| a["identity_slug"] == "devin-fail").expect("found devin-fail");

    assert_ne!(
        acct_status["status"], "uninitialized",
        "subsequent GET must not report uninitialized after failed refresh"
    );
    assert_eq!(acct_status["status"], "error");
    assert!(acct_status["error"].is_string(), "subsequent GET must report recorded error message, got: {:?}", acct_status["error"]);
    assert_eq!(
        acct_status["error"].as_str().unwrap(),
        "upstream authentication failed (HTTP 401)"
    );

    handle.abort();
}

#[tokio::test]
async fn test_devin_cached_transient_failure_refresh_returns_outcome_error_when_all_fail() {
    let mock = MockServerHandle::start("default");
    let auth_dir = unique_temp_dir("devin-transient-fail-test");
    let acct = json!({
        "type": "devin",
        "identity_slug": "devin-transient",
        "label": "Account Transient",
        "access_token": "devin-dummy-account-a",
        "api_server_url": mock.url,
        "disabled": false
    });
    std::fs::write(auth_dir.join("devin-transient.json"), serde_json::to_string(&acct).unwrap()).unwrap();

    let config = test_gateway_config(&auth_dir);
    let state = Arc::new(AppState::new(&config).expect("create state"));
    let app = create_app(Arc::clone(&state));

    // 1. First refresh succeeds and populates cache
    let req1 = Request::builder()
        .method("POST")
        .uri("/v0/management/devin/models/refresh")
        .header("authorization", "Bearer test-mgmt-key")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&json!({"identity_slug": "devin-transient"})).unwrap()))
        .unwrap();

    let resp1 = app.clone().oneshot(req1).await.unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);
    let body1 = resp1.into_body().collect().await.unwrap().to_bytes();
    let res1: Value = serde_json::from_slice(&body1).unwrap();
    assert_eq!(res1["outcome"], "success");
    assert!(!res1["models"].as_array().unwrap().is_empty());

    // 2. Terminate the mock process so subsequent refresh experiences a network failure
    drop(mock);

    let req2 = Request::builder()
        .method("POST")
        .uri("/v0/management/devin/models/refresh")
        .header("authorization", "Bearer test-mgmt-key")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&json!({"identity_slug": "devin-transient"})).unwrap()))
        .unwrap();

    let resp2 = app.clone().oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    let body2 = resp2.into_body().collect().await.unwrap().to_bytes();
    let res2: Value = serde_json::from_slice(&body2).unwrap();

    // The refresh failed for all accounts: outcome MUST be "error", NOT "success"!
    assert_eq!(
        res2["outcome"], "error",
        "all-account-failed refresh must return outcome: error even when preserving stale LKG, got: {res2:?}"
    );
    assert_eq!(res2["accounts"][0]["status"], "error");
    assert_eq!(res2["accounts"][0]["stale"], true);
    // Cached models are preserved as LKG
    assert!(!res2["models"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_devin_stale_get_schedules_async_refresh() {
    let mock = MockServerHandle::start("default");
    let auth_dir = unique_temp_dir("devin-stale-get-test");
    let acct = json!({
        "type": "devin",
        "identity_slug": "devin-stale-get",
        "label": "Account Stale GET",
        "access_token": "devin-dummy-account-a",
        "api_server_url": mock.url,
        "disabled": false
    });
    std::fs::write(auth_dir.join("devin-stale-get.json"), serde_json::to_string(&acct).unwrap()).unwrap();

    let config = test_gateway_config(&auth_dir);
    let state = Arc::new(AppState::new(&config).expect("create state"));
    let app = create_app(Arc::clone(&state));

    // Subscribe to refresh event signal before triggering GET request
    let mut rx = state.subscribe_finalizer("devin-stale-get");

    // Initially, account is uninitialized/stale
    let status_req = Request::builder()
        .method("GET")
        .uri("/v0/management/devin/models/status")
        .header("authorization", "Bearer test-mgmt-key")
        .body(Body::empty())
        .unwrap();

    let status_resp = app.clone().oneshot(status_req).await.unwrap();
    assert_eq!(status_resp.status(), StatusCode::OK);

    // Bounded wait on the exact event subscription
    tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("timed out waiting for async refresh signal")
        .expect("channel closed without event");

    let member = state.find_member("devin-stale-get").expect("found member");
    assert!(
        member.devin_catalog_state().is_some_and(|cat| !cat.models.is_empty()),
        "catalog state must be populated by async refresh scheduled by stale GET"
    );
}

#[tokio::test]
async fn test_devin_overlapping_refresh_out_of_order_stale_overwrite_prevented() {
    let barrier_req1_started = Arc::new(tokio::sync::Notify::new());
    let barrier_req1_started_clone = Arc::clone(&barrier_req1_started);

    let barrier_req1_allow_finish = Arc::new(tokio::sync::Notify::new());
    let barrier_req1_allow_finish_clone = Arc::clone(&barrier_req1_allow_finish);

    let req_count = Arc::new(AtomicUsize::new(0));
    let req_count_clone = Arc::clone(&req_count);

    // Mock upstream server that pauses the first (older) request until explicitly released
    let server = axum::Router::new().route(
        "/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs",
        axum::routing::post(move || {
            let count = req_count_clone.fetch_add(1, Ordering::SeqCst);
            let b_started = Arc::clone(&barrier_req1_started_clone);
            let b_finish = Arc::clone(&barrier_req1_allow_finish_clone);

            async move {
                let model_uid = if count == 0 {
                    // Older request: signal that it has started, then wait for barrier
                    b_started.notify_one();
                    b_finish.notified().await;
                    "glm-5-2-old"
                } else {
                    // Newer request: returns immediately
                    "glm-5-2-new"
                };

                let resp = mahoquot_gateway::compat::devin_proto::GetCascadeModelConfigsResponse {
                    client_model_configs: vec![
                        mahoquot_gateway::compat::devin_proto::ClientModelConfig {
                            model_uid: Some(model_uid.to_string()),
                            label: Some(model_uid.to_string()),
                            disabled: Some(false),
                            ..Default::default()
                        },
                    ],
                };
                let mut buf = Vec::new();
                resp.encode(&mut buf).unwrap();

                axum::http::Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "application/proto")
                    .body(axum::body::Body::from(buf))
                    .unwrap()
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, server).await.unwrap();
    });

    let auth_dir = unique_temp_dir("devin-overlap-race-test");
    let acct = json!({
        "type": "devin",
        "identity_slug": "devin-overlap",
        "label": "Account Overlap",
        "access_token": "token-overlap",
        "api_server_url": format!("http://{addr}"),
        "disabled": false
    });
    std::fs::write(auth_dir.join("devin-overlap.json"), serde_json::to_string(&acct).unwrap()).unwrap();

    let config = test_gateway_config(&auth_dir);
    let state = Arc::new(AppState::new(&config).expect("create state"));

    let member = state.find_member("devin-overlap").expect("find member");
    let client = state.devin_client_for_member(&member).expect("devin client");

    // 1. Launch Request 1 (older request) in background
    let member_clone1 = Arc::clone(&member);
    let client_clone1 = client.clone();
    let state_cache1 = Arc::clone(&state.devin_cache);
    let task_req1 = tokio::spawn(async move {
        mahoquot_gateway::devin_catalog::refresh_account_models(&member_clone1, &client_clone1, Some(&state_cache1)).await
    });

    // Await Request 1 reaching the upstream server and pausing at the barrier
    barrier_req1_started.notified().await;

    // 2. Launch Request 2 (newer request) while Request 1 is paused in-flight
    let cat2 = mahoquot_gateway::devin_catalog::refresh_account_models(&member, &client, Some(&state.devin_cache))
        .await
        .expect("request 2 refresh");
    assert_eq!(cat2.models[0].model_uid, "glm-5-2-new");

    // Publish Request 2's catalog
    let rev2 = cat2.key.credential_revision.clone();
    let snap2 = state.runtime.publish_devin_member_catalog(
        "devin-overlap",
        &rev2,
        cat2,
        &state.devin_cache,
    ).expect("publish request 2 catalog");

    assert_eq!(snap2.routable_accounts_for_model("devin/glm-5-2-new").len(), 1);
    assert_eq!(snap2.routable_accounts_for_model("devin/glm-5-2-old").len(), 0);

    // 3. Now release Request 1 (the older request) to complete its RPC late
    barrier_req1_allow_finish.notify_one();
    let cat1 = task_req1.await.unwrap().expect("request 1 refresh");
    assert_eq!(cat1.models[0].model_uid, "glm-5-2-old");

    // 4. CRITICAL INVARIANT: Request 1 must be rejected by publication as stale,
    // and CANNOT overwrite Request 2's newer catalog!
    let rev1 = cat1.key.credential_revision.clone();
    let pub1_res = state.runtime.publish_devin_member_catalog(
        "devin-overlap",
        &rev1,
        cat1,
        &state.devin_cache,
    );

    assert!(
        matches!(pub1_res, Err(DevinDiscoveryError::StalePublication)),
        "older overlapping request finishing late MUST be rejected with StalePublication, is_ok: {}",
        pub1_res.is_ok()
    );

    // Verify current snapshot still serves the newer model and not the older model
    let current_snap = state.pool.load();
    assert_eq!(
        current_snap.routable_accounts_for_model("devin/glm-5-2-new").len(),
        1,
        "newer model devin/glm-5-2-new must be preserved"
    );
    assert_eq!(
        current_snap.routable_accounts_for_model("devin/glm-5-2-old").len(),
        0,
        "older model devin/glm-5-2-old must not be routed"
    );

    server_handle.abort();
}

