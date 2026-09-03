mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use ed25519_dalek::SigningKey;
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

const ADMIN_KEY: &str = "test-admin";
const QA_PORT: u16 = 18875;

struct FixtureServer {
    signature_requests: AtomicU64,
    catalog_requests: AtomicU64,
    signature_started: Notify,
    release_signature: Notify,
    envelope: String,
    payload: Vec<u8>,
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
    state.release_signature.notified().await;
    (StatusCode::OK, state.envelope.as_bytes().to_vec())
}

async fn catalog_fixture(
    axum::extract::State(state): axum::extract::State<Arc<FixtureServer>>,
) -> Vec<u8> {
    state.catalog_requests.fetch_add(1, Ordering::SeqCst);
    state.payload.clone()
}

async fn start_fixture() -> (
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

fn assert_safe_fields(value: &Value, body: &str) {
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
        assert!(
            value.get(field).is_some(),
            "missing required field '{field}' in: {body}"
        );
    }
}

fn assert_no_secrets(text: &str) {
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
        assert!(
            !text.contains(forbidden),
            "forbidden secret leaked: '{forbidden}' in text: {text}"
        );
    }
}

#[tokio::test]
async fn task_14_qa_scenario() {
    let (fixture, fixture_addr, fixture_task) = start_fixture().await;
    let root = common::unique_temp_dir("mahoquot-task-14-qa");
    let auth_dir = root.join("auth");
    std::fs::create_dir_all(&auth_dir).expect("auth dir");

    // Named account secret file - must never be leaked
    std::fs::write(
        auth_dir.join("named-account-secret.json"),
        r#"{"type":"antigravity","identity_slug":"named-account-secret","access_token":"account-access-secret","refresh_token":"account-refresh-secret","email":"secret@example.test","project_id":"project-secret","expired":"2099-01-01T00:00:00Z"}"#,
    )
    .expect("credential fixture");

    let config_path = root.join("config.yaml");
    std::fs::write(
        &config_path,
        format!(
            "port: {QA_PORT}\nauth-dir: {}\napi-keys:\n  - {ADMIN_KEY}\nmodel-catalog:\n  refresh-enabled: true\n  url: http://{fixture_addr}/secret-url-token/models-v1.json\n  signature-url: http://{fixture_addr}/secret-url-token/models-v1.json.sig\n  refresh-interval-secs: 3600\n",
            auth_dir.display()
        ),
    )
    .expect("config fixture");

    let config = GatewayConfig {
        port: QA_PORT,
        auth_dir,
        api_keys: ApiKeys::new(vec![ADMIN_KEY.to_string()]),
        config_path,
        catalog_cache_path: Some(root.join("secret-cache-path/models-v1.signed.json")),
        ..GatewayConfig::default()
    };

    let state = Arc::new(AppState::new(&config).expect("app state created"));
    let app = create_app(Arc::clone(&state));

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{QA_PORT}"))
        .await
        .expect("bind to QA_PORT 18875");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .expect("gateway server");
    });

    // Brief yield for listener to be active
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = reqwest::Client::new();
    let uri = format!("http://127.0.0.1:{QA_PORT}/v0/management/model-registry");
    let mut http_evidence = String::new();

    // -------------------------------------------------------------------------
    // Scenario 1: curl -i http://127.0.0.1:18875/v0/management/model-registry -H 'Authorization: Bearer test-admin'
    // PASS iff 200 and required safe fields exist
    // -------------------------------------------------------------------------
    http_evidence.push_str("GET /v0/management/model-registry HTTP/1.1\n");
    http_evidence.push_str(&format!("Host: 127.0.0.1:{QA_PORT}\n"));
    http_evidence.push_str(&format!("Authorization: Bearer {ADMIN_KEY}\n\n"));

    let resp1 = client
        .get(&uri)
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .expect("get request sent");

    let status1 = resp1.status();
    let headers1 = resp1.headers().clone();
    let body1 = resp1.text().await.expect("body text");

    http_evidence.push_str(&format!("HTTP/1.1 {}\n", status1));
    for (k, v) in &headers1 {
        http_evidence.push_str(&format!("{}: {}\n", k, v.to_str().unwrap_or("")));
    }
    http_evidence.push('\n');
    http_evidence.push_str(&body1);
    http_evidence.push_str("\n\n---\n\n");

    assert_eq!(status1, reqwest::StatusCode::OK);
    let val1: Value = serde_json::from_str(&body1).expect("json body 1");
    assert_safe_fields(&val1, &body1);
    assert_eq!(val1["last-refresh"]["outcome"], "never");
    assert_no_secrets(&body1);

    // -------------------------------------------------------------------------
    // Scenario 2: Repeat without auth
    // PASS iff existing unauthorized status/body (401)
    // -------------------------------------------------------------------------
    http_evidence.push_str("GET /v0/management/model-registry HTTP/1.1\n");
    http_evidence.push_str(&format!("Host: 127.0.0.1:{QA_PORT}\n\n"));

    let resp2 = client
        .get(&uri)
        .send()
        .await
        .expect("unauthorized get sent");

    let status2 = resp2.status();
    let headers2 = resp2.headers().clone();
    let body2 = resp2.text().await.expect("body text");

    http_evidence.push_str(&format!("HTTP/1.1 {}\n", status2));
    for (k, v) in &headers2 {
        http_evidence.push_str(&format!("{}: {}\n", k, v.to_str().unwrap_or("")));
    }
    http_evidence.push('\n');
    http_evidence.push_str(&body2);
    http_evidence.push_str("\n\n---\n\n");

    assert_eq!(status2, reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(
        body2,
        r#"{"error":{"message":"invalid api key","type":"invalid_request_error"}}"#
    );
    assert_no_secrets(&body2);

    // -------------------------------------------------------------------------
    // Scenario 3: POST refresh twice
    // PASS iff one refresh attempt is observed and no secret values appear
    // -------------------------------------------------------------------------
    let completion = state.catalog.refresh_completion_seq();
    let sig_started = fixture.signature_started.notified();
    tokio::pin!(sig_started);

    // 1st POST - accepted, kicks off refresh (which pauses at signature endpoint)
    http_evidence.push_str("POST /v0/management/model-registry HTTP/1.1\n");
    http_evidence.push_str(&format!("Host: 127.0.0.1:{QA_PORT}\n"));
    http_evidence.push_str(&format!("Authorization: Bearer {ADMIN_KEY}\n"));
    http_evidence.push_str("Content-Length: 0\n\n");

    let resp3a = client
        .post(&uri)
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .expect("first post sent");

    let status3a = resp3a.status();
    let headers3a = resp3a.headers().clone();
    let body3a = resp3a.text().await.expect("body text");

    http_evidence.push_str(&format!("HTTP/1.1 {}\n", status3a));
    for (k, v) in &headers3a {
        http_evidence.push_str(&format!("{}: {}\n", k, v.to_str().unwrap_or("")));
    }
    http_evidence.push('\n');
    http_evidence.push_str(&body3a);
    http_evidence.push_str("\n\n---\n\n");

    assert_eq!(status3a, reqwest::StatusCode::ACCEPTED);
    let val3a: Value = serde_json::from_str(&body3a).expect("json body 3a");
    assert_eq!(val3a["accepted"], true);
    assert_eq!(val3a["coalesced"], false);
    assert_safe_fields(&val3a["state"], &body3a);
    assert_no_secrets(&body3a);

    // 2nd POST - coalesced, while 1st refresh is in flight
    http_evidence.push_str("POST /v0/management/model-registry HTTP/1.1\n");
    http_evidence.push_str(&format!("Host: 127.0.0.1:{QA_PORT}\n"));
    http_evidence.push_str(&format!("Authorization: Bearer {ADMIN_KEY}\n"));
    http_evidence.push_str("Content-Length: 0\n\n");

    let resp3b = client
        .post(&uri)
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .expect("second post sent");

    let status3b = resp3b.status();
    let headers3b = resp3b.headers().clone();
    let body3b = resp3b.text().await.expect("body text");

    http_evidence.push_str(&format!("HTTP/1.1 {}\n", status3b));
    for (k, v) in &headers3b {
        http_evidence.push_str(&format!("{}: {}\n", k, v.to_str().unwrap_or("")));
    }
    http_evidence.push('\n');
    http_evidence.push_str(&body3b);
    http_evidence.push_str("\n\n---\n\n");

    assert_eq!(status3b, reqwest::StatusCode::ACCEPTED);
    let val3b: Value = serde_json::from_str(&body3b).expect("json body 3b");
    assert_eq!(val3b["accepted"], false);
    assert_eq!(val3b["coalesced"], true);
    assert_safe_fields(&val3b["state"], &body3b);
    assert_no_secrets(&body3b);

    // Await signature fetch start
    tokio::time::timeout(Duration::from_secs(2), &mut sig_started)
        .await
        .expect("signature fetch started");

    // Only 1 signature request triggered
    assert_eq!(fixture.signature_requests.load(Ordering::SeqCst), 1);

    // Release signature to complete the refresh cycle
    fixture.release_signature.notify_one();

    // Await completion of the refresh
    tokio::time::timeout(
        Duration::from_secs(2),
        state.catalog.wait_for_refresh_after(completion),
    )
    .await
    .expect("refresh completion");

    // Verify exactly one refresh attempt was observed at the fixture
    assert_eq!(fixture.signature_requests.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.catalog_requests.load(Ordering::SeqCst), 1);

    // -------------------------------------------------------------------------
    // Verification GET: verify updated state after successful refresh
    // -------------------------------------------------------------------------
    http_evidence.push_str("GET /v0/management/model-registry HTTP/1.1\n");
    http_evidence.push_str(&format!("Host: 127.0.0.1:{QA_PORT}\n"));
    http_evidence.push_str(&format!("Authorization: Bearer {ADMIN_KEY}\n\n"));

    let resp4 = client
        .get(&uri)
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .expect("post-refresh get sent");

    let status4 = resp4.status();
    let headers4 = resp4.headers().clone();
    let body4 = resp4.text().await.expect("body text");

    http_evidence.push_str(&format!("HTTP/1.1 {}\n", status4));
    for (k, v) in &headers4 {
        http_evidence.push_str(&format!("{}: {}\n", k, v.to_str().unwrap_or("")));
    }
    http_evidence.push('\n');
    http_evidence.push_str(&body4);
    http_evidence.push('\n');

    assert_eq!(status4, reqwest::StatusCode::OK);
    let val4: Value = serde_json::from_str(&body4).expect("json body 4");
    assert_safe_fields(&val4, &body4);
    assert_eq!(val4["source"], "remote_signed");
    assert_eq!(val4["last-refresh"]["outcome"], "success");
    assert_eq!(val4["refresh-in-flight"], false);
    assert_no_secrets(&body4);

    // Overall assert across complete HTTP transcript
    assert_no_secrets(&http_evidence);

    // Graceful shutdown of gateway server
    let _ = shutdown_tx.send(());
    server_task.await.expect("gateway server shut down cleanly");

    // Abort fixture server
    fixture_task.abort();

    // Verify port 18875 is freed
    let check_listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{QA_PORT}")).await;
    assert!(
        check_listener.is_ok(),
        "port 18875 was not freed on shutdown"
    );
    drop(check_listener);

    // Write evidence to both repositories
    let proxy_repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let proxy_evidence = proxy_repo.join(".omo/evidence/model-registry");
    std::fs::create_dir_all(&proxy_evidence).expect("proxy evidence dir created");
    std::fs::write(
        proxy_evidence.join("task-14-management.http"),
        &http_evidence,
    )
    .expect("wrote proxy task-14-management.http");

    let quotio_evidence =
        PathBuf::from("/Users/indo/code/project/quotio-rs/.omo/evidence/model-registry");
    std::fs::create_dir_all(&quotio_evidence).expect("quotio evidence dir created");
    std::fs::write(
        quotio_evidence.join("task-14-management.http"),
        &http_evidence,
    )
    .expect("wrote quotio task-14-management.http");

    // Cleanup temp dir
    std::fs::remove_dir_all(root).ok();
}
