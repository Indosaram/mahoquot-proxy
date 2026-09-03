mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use mahoquot_registry::{
    AuthorityMask, CatalogSource, ModelAliasRule, ModelCapability, ModelDescriptor, ModelId,
    ProviderBinding, ProviderId, ProviderPolicy, RegistrySnapshot,
};
use serde_json::{json, Value};

const SIGNED_IMAGE_MODEL: &str = "fixture-signed-image-model";
const SIGNED_IMAGE_ALIAS: &str = "fixture-image-alias";
const SPARSE_MODEL: &str = "fixture-sparse-discovery-model";
const SAME_ID_MODEL: &str = "fixture-provider-specific-model";
const REALTIME_MODEL: &str = "fixture-realtime-model";
const COUNT_TOKENS_MODEL: &str = "fixture-count-tokens-model";

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
    let codex = ProviderId::codex();

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

    let sparse_binding = ProviderBinding::new(
        antigravity.clone(),
        ProviderPolicy::Discovered,
        CatalogSource::Discovered,
    )
    .with_authority(AuthorityMask::MODELS_ONLY);
    let sparse = descriptor(SPARSE_MODEL, "google", [sparse_binding]);
    registry.models.insert(sparse.id.clone(), sparse);

    let same_id = descriptor(
        SAME_ID_MODEL,
        "mixed",
        [
            signed_binding(codex, &[ModelCapability::Image], 90),
            signed_binding(antigravity.clone(), &[ModelCapability::Chat], 100),
        ],
    );
    registry.models.insert(same_id.id.clone(), same_id);

    let realtime = descriptor(
        REALTIME_MODEL,
        "google",
        [signed_binding(
            antigravity.clone(),
            &[ModelCapability::Realtime],
            100,
        )],
    );
    registry.models.insert(realtime.id.clone(), realtime);

    let count_tokens = descriptor(
        COUNT_TOKENS_MODEL,
        "google",
        [signed_binding(
            antigravity,
            &[ModelCapability::CountTokens],
            100,
        )],
    );
    registry
        .models
        .insert(count_tokens.id.clone(), count_tokens);

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

async fn fixture_gateway(
    upstream: &str,
) -> (
    Arc<AppState>,
    String,
    std::path::PathBuf,
    tokio::task::JoinHandle<()>,
) {
    let auth_dir = common::unique_temp_dir("task-13-capabilities");
    std::fs::write(
        auth_dir.join("antigravity-fixture.json"),
        serde_json::to_vec(&antigravity_credential(upstream)).unwrap(),
    )
    .unwrap();
    let config = GatewayConfig {
        auth_dir: auth_dir.clone(),
        config_path: auth_dir.join("config.yaml"),
        auth_refresh_enabled: false,
        max_failover: 3,
        ..GatewayConfig::default()
    };
    let state = Arc::new(AppState::new(&config).unwrap());
    state
        .runtime_state()
        .update_registry(Arc::new(fixture_registry()))
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = create_app(Arc::clone(&state));
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (state, url, auth_dir, task)
}

#[tokio::test]
async fn signed_image_capability_and_alias_reach_only_the_fixture_upstream() {
    let (upstream, count, upstream_task) = spawn_fixture_upstream().await;
    let (_state, gateway, auth_dir, gateway_task) = fixture_gateway(&upstream).await;
    let client = reqwest::Client::new();

    for model in [SIGNED_IMAGE_MODEL, SIGNED_IMAGE_ALIAS] {
        let response = client
            .post(format!("{gateway}/v1/images/generations"))
            .json(&json!({"model": model, "prompt": "fixture"}))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert_eq!(status, StatusCode::OK, "response: {body}");
        assert_eq!(body["data"][0]["url"], "fixture://image");
    }
    assert_eq!(count.load(Ordering::SeqCst), 2);

    gateway_task.abort();
    upstream_task.abort();
    std::fs::remove_dir_all(auth_dir).ok();
}

#[tokio::test]
async fn text_video_sparse_and_wrong_provider_capabilities_fail_before_upstream() {
    let (upstream, count, upstream_task) = spawn_fixture_upstream().await;
    let (_state, gateway, auth_dir, gateway_task) = fixture_gateway(&upstream).await;
    let client = reqwest::Client::new();

    for model in [
        "gpt-5.6-sol",
        "grok-imagine-video",
        SPARSE_MODEL,
        SAME_ID_MODEL,
    ] {
        let response = client
            .post(format!("{gateway}/v1/images/generations"))
            .json(&json!({"model": model, "prompt": "fixture"}))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert_eq!(status, StatusCode::BAD_REQUEST, "model {model}: {body}");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert!(body["error"].get("code").is_none());
    }
    assert_eq!(count.load(Ordering::SeqCst), 0);

    gateway_task.abort();
    upstream_task.abort();
    std::fs::remove_dir_all(auth_dir).ok();
}

#[tokio::test]
async fn realtime_and_count_token_surfaces_require_explicit_binding_capabilities() {
    let (upstream, _count, upstream_task) = spawn_fixture_upstream().await;
    let (_state, gateway, auth_dir, gateway_task) = fixture_gateway(&upstream).await;
    let client = reqwest::Client::new();

    let allowed_realtime = client
        .post(format!("{gateway}/v1/realtime/sessions"))
        .json(&json!({"model": REALTIME_MODEL}))
        .send()
        .await
        .unwrap();
    assert_eq!(allowed_realtime.status(), StatusCode::OK);

    let rejected_realtime = client
        .post(format!("{gateway}/v1/realtime/sessions"))
        .json(&json!({"model": "gpt-5.6-sol"}))
        .send()
        .await
        .unwrap();
    let rejected_status = rejected_realtime.status();
    let rejected_body: Value = rejected_realtime.json().await.unwrap();
    assert_eq!(rejected_status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(
        rejected_body["error"]["code"],
        "realtime_capability_not_supported"
    );

    let allowed_count = client
        .post(format!("{gateway}/v1/messages/count_tokens"))
        .json(&json!({
            "model": COUNT_TOKENS_MODEL,
            "messages": [{"role": "user", "content": "fixture"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(allowed_count.status(), StatusCode::OK);

    let rejected_count = client
        .post(format!("{gateway}/v1/messages/count_tokens"))
        .json(&json!({
            "model": SPARSE_MODEL,
            "messages": [{"role": "user", "content": "fixture"}]
        }))
        .send()
        .await
        .unwrap();
    let rejected_count_status = rejected_count.status();
    let rejected_count_body: Value = rejected_count.json().await.unwrap();
    assert_eq!(rejected_count_status, StatusCode::BAD_REQUEST);
    assert_eq!(rejected_count_body["type"], "error");
    assert_eq!(
        rejected_count_body["error"]["type"],
        "invalid_request_error"
    );

    gateway_task.abort();
    upstream_task.abort();
    std::fs::remove_dir_all(auth_dir).ok();
}
