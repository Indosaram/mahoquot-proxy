mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use mahoquot_registry::{
    CatalogSource, ModelAliasRule, ModelCapability, ModelDescriptor, ModelId, ProviderBinding,
    ProviderId, ProviderPolicy, RegistrySnapshot,
};
use serde_json::{json, Value};

const QA_PORT: u16 = 18874;
const SIGNED_IMAGE_MODEL: &str = "fixture-signed-image-model";
const SIGNED_IMAGE_ALIAS: &str = "fixture-image-alias";

fn antigravity_credential(upstream: &str) -> Value {
    json!({
        "type": "antigravity",
        "identity_slug": "fixture-antigravity",
        "access_token": "fixture-access",
        "refresh_token": "fixture-refresh",
        "project_id": "fixture-project",
        "email": "fixture@example.invalid",
        "expired": "2099-01-01T00:00:00Z",
        "upstream_override": upstream
    })
}

fn signed_binding(
    provider: ProviderId,
    capabilities: &[ModelCapability],
    priority: i32,
) -> ProviderBinding {
    ProviderBinding::new(
        provider,
        ProviderPolicy::Closed,
        CatalogSource::RemoteSigned,
    )
    .with_capabilities(capabilities.iter().copied())
    .with_priority(priority)
}

fn descriptor(
    id: &str,
    owner: &str,
    bindings: impl IntoIterator<Item = ProviderBinding>,
) -> ModelDescriptor {
    let mut model = ModelDescriptor::new(ModelId::new(id).unwrap(), owner);
    for binding in bindings {
        model.bindings.insert(binding.provider_id.clone(), binding);
    }
    model
}

fn fixture_registry() -> RegistrySnapshot {
    let mut registry = mahoquot_registry::embedded_registry_snapshot().unwrap();
    registry.version = mahoquot_registry::CatalogVersion(13);
    registry.source = CatalogSource::RemoteSigned;

    let antigravity = ProviderId::antigravity();

    let image = descriptor(
        SIGNED_IMAGE_MODEL,
        "google",
        [signed_binding(
            antigravity.clone(),
            &[ModelCapability::Image],
            100,
        )],
    );
    registry.models.insert(image.id.clone(), image);
    registry.aliases.insert(
        ModelId::new(SIGNED_IMAGE_ALIAS).unwrap(),
        ModelAliasRule {
            alias: ModelId::new(SIGNED_IMAGE_ALIAS).unwrap(),
            target: ModelId::new(SIGNED_IMAGE_MODEL).unwrap(),
            provider_id: Some(antigravity.clone()),
        },
    );

    registry.validate().unwrap();
    registry
}

async fn spawn_fixture_upstream() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    async fn image() -> Response {
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"created":13,"data":[{"url":"fixture://image"}]}"#,
            ))
            .unwrap()
    }

    let count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&count);
    let app = Router::new().route(
        "/v1/images/generations",
        post(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            image()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, count, task)
}

#[tokio::test]
async fn task_13_http_qa_scenario() {
    let (upstream_url, upstream_count, upstream_task) = spawn_fixture_upstream().await;

    let auth_dir = common::unique_temp_dir("mahoquot-task-13-qa");
    std::fs::write(
        auth_dir.join("antigravity-fixture.json"),
        serde_json::to_vec(&antigravity_credential(&upstream_url)).unwrap(),
    )
    .unwrap();

    let gateway_config = GatewayConfig {
        port: QA_PORT,
        auth_dir: auth_dir.clone(),
        config_path: auth_dir.join("config.yaml"),
        auth_refresh_enabled: false,
        max_failover: 3,
        ..GatewayConfig::default()
    };

    let state = Arc::new(AppState::new(&gateway_config).expect("app state created"));
    state
        .runtime_state()
        .update_registry(Arc::new(fixture_registry()))
        .expect("updated registry");

    let app = create_app(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{QA_PORT}"))
        .await
        .expect("bound to 127.0.0.1:18874");

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

    let client = reqwest::Client::new();
    let images_url = format!("http://127.0.0.1:{QA_PORT}/v1/images/generations");

    let mut http_evidence = String::new();

    // 1. Ineligible model request: gpt-5.6-sol on image surface
    // MUST return local 400 and upstream count must remain 0
    let ineligible_payload = json!({
        "model": "gpt-5.6-sol",
        "prompt": "fixture"
    });

    http_evidence.push_str("POST /v1/images/generations HTTP/1.1\n");
    http_evidence.push_str(&format!("Host: 127.0.0.1:{QA_PORT}\n"));
    http_evidence.push_str("Content-Type: application/json\n\n");
    http_evidence.push_str(&serde_json::to_string_pretty(&ineligible_payload).unwrap());
    http_evidence.push_str("\n\n");

    let resp1 = client
        .post(&images_url)
        .header("Content-Type", "application/json")
        .json(&ineligible_payload)
        .send()
        .await
        .expect("sent request 1");

    let status1 = resp1.status();
    let headers1 = resp1.headers().clone();
    let body_text1 = resp1.text().await.expect("text 1");

    http_evidence.push_str(&format!("HTTP/1.1 {}\n", status1));
    for (k, v) in &headers1 {
        http_evidence.push_str(&format!("{}: {}\n", k, v.to_str().unwrap_or("")));
    }
    http_evidence.push('\n');
    http_evidence.push_str(&body_text1);
    http_evidence.push_str("\n\n---\n\n");

    assert_eq!(status1, reqwest::StatusCode::BAD_REQUEST);
    let parsed1: Value = serde_json::from_str(&body_text1).unwrap();
    assert_eq!(parsed1["error"]["type"], "invalid_request_error");
    assert_eq!(
        parsed1["error"]["message"],
        "Model gpt-5.6-sol is not supported on /v1/images/generations or /v1/images/edits. Use gpt-image-1.5, gpt-image-2, or a configured openai-compatibility image model."
    );
    assert_eq!(
        upstream_count.load(Ordering::SeqCst),
        0,
        "fixture upstream count must be 0 for gated request"
    );

    // 2. Fixture-only image-capable model: must pass gate and reach fake upstream
    let capable_payload = json!({
        "model": SIGNED_IMAGE_MODEL,
        "prompt": "fixture"
    });

    http_evidence.push_str("POST /v1/images/generations HTTP/1.1\n");
    http_evidence.push_str(&format!("Host: 127.0.0.1:{QA_PORT}\n"));
    http_evidence.push_str("Content-Type: application/json\n\n");
    http_evidence.push_str(&serde_json::to_string_pretty(&capable_payload).unwrap());
    http_evidence.push_str("\n\n");

    let resp2 = client
        .post(&images_url)
        .header("Content-Type", "application/json")
        .json(&capable_payload)
        .send()
        .await
        .expect("sent request 2");

    let status2 = resp2.status();
    let headers2 = resp2.headers().clone();
    let body_text2 = resp2.text().await.expect("text 2");

    http_evidence.push_str(&format!("HTTP/1.1 {}\n", status2));
    for (k, v) in &headers2 {
        http_evidence.push_str(&format!("{}: {}\n", k, v.to_str().unwrap_or("")));
    }
    http_evidence.push('\n');
    http_evidence.push_str(&body_text2);
    http_evidence.push('\n');

    assert_eq!(status2, reqwest::StatusCode::OK);
    let parsed2: Value = serde_json::from_str(&body_text2).unwrap();
    assert_eq!(parsed2["data"][0]["url"], "fixture://image");
    assert_eq!(
        upstream_count.load(Ordering::SeqCst),
        1,
        "fixture upstream count must be 1 after capable model request"
    );

    // 3. Image alias request: must also pass gate and reach fake upstream
    let alias_payload = json!({
        "model": SIGNED_IMAGE_ALIAS,
        "prompt": "fixture"
    });

    http_evidence.push_str("\n---\n\n");
    http_evidence.push_str("POST /v1/images/generations HTTP/1.1\n");
    http_evidence.push_str(&format!("Host: 127.0.0.1:{QA_PORT}\n"));
    http_evidence.push_str("Content-Type: application/json\n\n");
    http_evidence.push_str(&serde_json::to_string_pretty(&alias_payload).unwrap());
    http_evidence.push_str("\n\n");

    let resp3 = client
        .post(&images_url)
        .header("Content-Type", "application/json")
        .json(&alias_payload)
        .send()
        .await
        .expect("sent request 3");

    let status3 = resp3.status();
    let headers3 = resp3.headers().clone();
    let body_text3 = resp3.text().await.expect("text 3");

    http_evidence.push_str(&format!("HTTP/1.1 {}\n", status3));
    for (k, v) in &headers3 {
        http_evidence.push_str(&format!("{}: {}\n", k, v.to_str().unwrap_or("")));
    }
    http_evidence.push('\n');
    http_evidence.push_str(&body_text3);
    http_evidence.push('\n');

    assert_eq!(status3, reqwest::StatusCode::OK);
    assert_eq!(
        upstream_count.load(Ordering::SeqCst),
        2,
        "fixture upstream count must be 2 after alias request"
    );

    // Graceful shutdown
    let _ = shutdown_tx.send(());
    server_handle.await.expect("server stopped");
    upstream_task.abort();
    std::fs::remove_dir_all(auth_dir).ok();

    // Write evidence files to both repositories
    let proxy_evidence_dir =
        PathBuf::from("/Users/indo/code/project/mahoquot-proxy/.omo/evidence/model-registry");
    let quotio_evidence_dir =
        PathBuf::from("/Users/indo/code/project/quotio-rs/.omo/evidence/model-registry");
    std::fs::create_dir_all(&proxy_evidence_dir).expect("proxy evidence dir");
    std::fs::create_dir_all(&quotio_evidence_dir).expect("quotio evidence dir");

    std::fs::write(
        proxy_evidence_dir.join("task-13-capabilities.http"),
        &http_evidence,
    )
    .expect("wrote proxy evidence");

    std::fs::write(
        quotio_evidence_dir.join("task-13-capabilities.http"),
        &http_evidence,
    )
    .expect("wrote quotio evidence");
}
