mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::routing::get;
use axum::Router;
use ed25519_dalek::SigningKey;
use http_body_util::BodyExt;
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::inbound::ApiKeys;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use mahoquot_registry::{
    canonicalize_json, CatalogSigner, CatalogSource, CatalogVersion, ModelCapability,
    ModelDescriptor, ModelId, ProviderBinding, ProviderId, ProviderPolicy, RegistryBuilder,
    TEST_KEY_ID_V1,
};
use serde_json::Value;
use tokio::sync::Notify;
use tower::ServiceExt;

const ADMIN_KEY: &str = "test-admin";
const CONTRACT_SCHEMA: &str = include_str!("../../../docs/management-contract-v1.schema.json");
const CONTRACT_EXAMPLES: &str = include_str!("../../../docs/examples/management-contract-v1.json");

struct FixtureServer {
    signature_requests: AtomicU64,
    catalog_requests: AtomicU64,
    signature_started: Notify,
    release_signature: Notify,
    envelope: String,
    payload: Vec<u8>,
    fail_signature: bool,
}

fn signed_antigravity_catalog(version: u64, generated_at: u64) -> (String, Vec<u8>) {
    let mut builder = RegistryBuilder::new(CatalogVersion(version), CatalogSource::RemoteSigned);
    let provider = ProviderId::antigravity();
    builder.register_provider(provider.clone(), ProviderPolicy::Closed);
    let mut model = ModelDescriptor::new(
        ModelId::new("gemini-task-14-fixture").expect("model id"),
        "google",
    );
    model.capabilities.insert(ModelCapability::Chat);
    builder.add_model(model).expect("model");
    builder
        .add_binding(
            ModelId::new("gemini-task-14-fixture").expect("model id"),
            ProviderBinding::new(
                provider,
                ProviderPolicy::Closed,
                CatalogSource::RemoteSigned,
            )
            .with_capabilities([ModelCapability::Chat]),
        )
        .expect("binding");
    let snapshot = builder.build().expect("catalog");
    let payload = canonicalize_json(&serde_json::to_vec(&snapshot).expect("serialize"))
        .expect("canonical payload");

    let seed = [
        0x79, 0x61, 0x29, 0x84, 0x53, 0x40, 0x17, 0x68, 0x40, 0x39, 0x20, 0x19, 0x48, 0x57, 0x39,
        0x20, 0x18, 0x47, 0x59, 0x30, 0x29, 0x18, 0x47, 0x58, 0x39, 0x20, 0x19, 0x48, 0x57, 0x69,
        0x28, 0x34,
    ];
    let signer = CatalogSigner::new(SigningKey::from_bytes(&seed), TEST_KEY_ID_V1);
    let envelope = signer
        .sign_catalog(snapshot.version(), generated_at, None, &payload)
        .expect("sign")
        .to_json()
        .expect("envelope json");
    (envelope, payload)
}

async fn signature_fixture(
    axum::extract::State(state): axum::extract::State<Arc<FixtureServer>>,
) -> (StatusCode, Vec<u8>) {
    state.signature_requests.fetch_add(1, Ordering::SeqCst);
    state.signature_started.notify_waiters();
    if state.fail_signature {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            b"server-signing-secret".to_vec(),
        );
    }
    state.release_signature.notified().await;
    (StatusCode::OK, state.envelope.as_bytes().to_vec())
}

async fn catalog_fixture(
    axum::extract::State(state): axum::extract::State<Arc<FixtureServer>>,
) -> Vec<u8> {
    state.catalog_requests.fetch_add(1, Ordering::SeqCst);
    state.payload.clone()
}

async fn start_fixture(
    fail_signature: bool,
) -> (
    Arc<FixtureServer>,
    std::net::SocketAddr,
    tokio::task::JoinHandle<()>,
) {
    let generated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let version = mahoquot_registry::embedded_snapshot().version().as_u64() + 100;
    let (envelope, payload) = signed_antigravity_catalog(version, generated_at);
    let state = Arc::new(FixtureServer {
        signature_requests: AtomicU64::new(0),
        catalog_requests: AtomicU64::new(0),
        signature_started: Notify::new(),
        release_signature: Notify::new(),
        envelope,
        payload,
        fail_signature,
    });
    let app = Router::new()
        .route(
            "/secret-url-token/models-v1.json.sig",
            get(signature_fixture),
        )
        .route("/secret-url-token/models-v1.json", get(catalog_fixture))
        .with_state(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fixture bind");
    let addr = listener.local_addr().expect("fixture addr");
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("fixture server");
    });
    (state, addr, task)
}

fn test_state(tag: &str, addr: std::net::SocketAddr) -> (Arc<AppState>, std::path::PathBuf) {
    let root = common::unique_temp_dir(&format!("management-registry-{tag}"));
    let auth_dir = root.join("auth");
    std::fs::create_dir_all(&auth_dir).expect("auth dir");
    std::fs::write(
        auth_dir.join("named-account-secret.json"),
        r#"{"type":"antigravity","identity_slug":"named-account-secret","access_token":"account-access-secret","refresh_token":"account-refresh-secret","email":"secret@example.test","project_id":"project-secret","expired":"2099-01-01T00:00:00Z"}"#,
    )
    .expect("credential fixture");
    let config_path = root.join("config.yaml");
    std::fs::write(
        &config_path,
        format!(
            "auth-dir: {}\napi-keys:\n  - {ADMIN_KEY}\nmodel-catalog:\n  refresh-enabled: true\n  url: http://{addr}/secret-url-token/models-v1.json\n  signature-url: http://{addr}/secret-url-token/models-v1.json.sig\n  refresh-interval-secs: 3600\n",
            auth_dir.display()
        ),
    )
    .expect("config fixture");
    let config = GatewayConfig {
        auth_dir,
        api_keys: ApiKeys::new(vec![ADMIN_KEY.to_string()]),
        config_path,
        catalog_cache_path: Some(root.join("secret-cache-path/models-v1.signed.json")),
        ..GatewayConfig::default()
    };
    (Arc::new(AppState::new(&config).expect("state")), root)
}

async fn request(app: &axum::Router, method: Method, authorized: bool) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method(method)
        .uri("/v0/management/model-registry");
    if authorized {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {ADMIN_KEY}"));
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, String::from_utf8(body.to_vec()).expect("utf8 body"))
}

fn assert_safe_status(value: &Value, body: &str) {
    for field in [
        "source",
        "catalog-version",
        "generation",
        "generated-at",
        "loaded-at",
        "stale",
        "last-refresh",
        "provider-count",
        "model-count",
        "refresh-in-flight",
    ] {
        assert!(value.get(field).is_some(), "missing {field}: {body}");
    }
    for forbidden in [
        "secret-url-token",
        "secret-cache-path",
        "named-account-secret",
        "account-access-secret",
        "account-refresh-secret",
        "secret@example.test",
        "project-secret",
        TEST_KEY_ID_V1,
        "signature",
    ] {
        assert!(!body.contains(forbidden), "leaked {forbidden}: {body}");
    }
}

#[test]
fn management_registry_contract_schema_and_fixture_are_compatible() {
    let schema: Value = serde_json::from_str(CONTRACT_SCHEMA).expect("schema json");
    let examples: Value = serde_json::from_str(CONTRACT_EXAMPLES).expect("examples json");
    let registry = &schema["properties"]["model-registry"];
    assert_eq!(registry["type"], "object");
    for required in [
        "source",
        "catalog-version",
        "generation",
        "generated-at",
        "loaded-at",
        "stale",
        "last-refresh",
        "provider-count",
        "model-count",
        "refresh-in-flight",
    ] {
        assert!(
            registry["required"]
                .as_array()
                .expect("required")
                .iter()
                .any(|value| value == required),
            "schema missing {required}"
        );
    }
    assert!(examples["contracts"]["model-registry"].is_object());
    assert_eq!(
        schema["x-route-registration-owners"]["GET /v0/management/model-registry"],
        "crates/gateway/src/management/registry.rs"
    );
    assert_eq!(
        schema["x-route-registration-owners"]["POST /v0/management/model-registry"],
        "crates/gateway/src/management/registry.rs"
    );
}

#[tokio::test]
async fn management_registry_rejects_unauthenticated_and_returns_safe_status() {
    let (_fixture, addr, task) = start_fixture(false).await;
    let (state, root) = test_state("auth-safe", addr);
    let app = create_app(state);

    let (status, body) = request(&app, Method::GET, false).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        body,
        r#"{"error":{"message":"invalid api key","type":"invalid_request_error"}}"#
    );

    let (status, body) = request(&app, Method::GET, true).await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).expect("status json");
    assert_safe_status(&value, &body);
    assert_eq!(value["last-refresh"]["outcome"], "never");

    task.abort();
    std::fs::remove_dir_all(root).ok();
}

#[tokio::test]
async fn management_registry_refresh_is_nonblocking_and_coalesced() {
    let (fixture, addr, task) = start_fixture(false).await;
    let (state, root) = test_state("coalesced", addr);
    let completion = state.catalog.refresh_completion_seq();
    let app = create_app(Arc::clone(&state));
    let signature_started = fixture.signature_started.notified();
    tokio::pin!(signature_started);

    let (first_status, first_body) = request(&app, Method::POST, true).await;
    let (second_status, second_body) = request(&app, Method::POST, true).await;
    assert_eq!(first_status, StatusCode::ACCEPTED);
    assert_eq!(second_status, StatusCode::ACCEPTED);
    let first: Value = serde_json::from_str(&first_body).expect("first json");
    let second: Value = serde_json::from_str(&second_body).expect("second json");
    assert_eq!(first["accepted"], true);
    assert_eq!(second["accepted"], false);
    assert_eq!(second["coalesced"], true);
    assert_safe_status(&first["state"], &first_body);
    assert_safe_status(&second["state"], &second_body);

    tokio::time::timeout(std::time::Duration::from_secs(2), &mut signature_started)
        .await
        .expect("refresh started");
    assert_eq!(fixture.signature_requests.load(Ordering::SeqCst), 1);
    fixture.release_signature.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state.catalog.wait_for_refresh_after(completion),
    )
    .await
    .expect("refresh completion");

    let (status, body) = request(&app, Method::GET, true).await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).expect("status json");
    assert_safe_status(&value, &body);
    assert_eq!(value["source"], "remote_signed");
    assert_eq!(value["last-refresh"]["outcome"], "success");
    assert_eq!(value["refresh-in-flight"], false);
    assert_eq!(fixture.signature_requests.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.catalog_requests.load(Ordering::SeqCst), 1);

    let metrics = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("metrics");
    let metrics = String::from_utf8(
        metrics
            .into_body()
            .collect()
            .await
            .expect("metrics body")
            .to_bytes()
            .to_vec(),
    )
    .expect("metrics utf8");
    assert!(metrics.contains("mahoquot_model_registry_refresh_attempts_total 1"));
    assert!(metrics.contains("outcome=\"success\"} 1"));
    assert!(metrics.contains("reason=\"coalesced\"} 1"));
    assert!(metrics.contains("source=\"remote_signed\"} 1"));

    task.abort();
    std::fs::remove_dir_all(root).ok();
}

#[tokio::test]
async fn management_registry_reports_stale_error_without_leaking_details() {
    let (fixture, addr, task) = start_fixture(true).await;
    let (state, root) = test_state("error", addr);
    let completion = state.catalog.refresh_completion_seq();
    let app = create_app(Arc::clone(&state));

    let (status, body) = request(&app, Method::POST, true).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let accepted: Value = serde_json::from_str(&body).expect("accepted json");
    assert_eq!(accepted["accepted"], true);
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state.catalog.wait_for_refresh_after(completion),
    )
    .await
    .expect("failed refresh completion");

    let (status, body) = request(&app, Method::GET, true).await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).expect("status json");
    assert_safe_status(&value, &body);
    assert_eq!(value["stale"], true);
    assert_eq!(value["last-refresh"]["outcome"], "error");
    assert_eq!(value["last-refresh"]["rejection-reason"], "http");
    assert!(!body.contains("server-signing-secret"));
    assert!(!body.contains(&addr.to_string()));
    assert_eq!(fixture.signature_requests.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.catalog_requests.load(Ordering::SeqCst), 0);

    task.abort();
    std::fs::remove_dir_all(root).ok();
}
