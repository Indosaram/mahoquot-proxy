//! Phase-2 shared-integration contracts: the membership reconcile fan-out, the
//! inference admission gate, and worker teardown. Every test is deterministic
//! (no sleeps, no polling) and makes no external network call — an empty pool
//! answers inference requests without a single upstream attempt.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::inbound::ApiKeys;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use mahoquot_gateway::usage::UsageSample;
use tower::ServiceExt;

const TEST_API_KEY: &str = "t36-integration-key";

struct TestDir(std::path::PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "mahoquot-t36-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create auth dir");
        Self(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn codex_credential() -> &'static str {
    r#"{"identity_slug":"codex-seed","access_token":"tok-codex",
        "refresh_token":"ref-codex","email":"codex-seed@example.test",
        "expired":"2030-01-01T00:00:00Z","type":"codex",
        "account_id":"acc-seed","id_token":"idt",
        "upstream_override":"http://127.0.0.1:18849",
        "last_refresh":"2026-01-01T00:00:00Z"}"#
}

fn claude_credential() -> serde_json::Value {
    serde_json::json!({
        "type": "claude",
        "access_token": "tok-claude-import",
        "refresh_token": "ref-claude-import",
        "email": "claude-import@example.test",
        "expired": "2030-01-01T00:00:00Z",
        "identity_slug": "claude-import",
        "upstream_override": "http://127.0.0.1:18849",
        "disabled": false
    })
}

fn config_for(dir: &TestDir) -> GatewayConfig {
    GatewayConfig {
        auth_dir: dir.path().to_path_buf(),
        api_keys: ApiKeys::new(vec![TEST_API_KEY.to_string()]),
        config_path: dir.path().join("config.yaml"),
        auth_refresh_enabled: false,
        // A small ceiling keeps this fixture cheap; production defaults far higher.
        max_concurrent_inference: 4,
        ..GatewayConfig::default()
    }
}

fn state_with_two_credentials(dir: &TestDir) -> Arc<AppState> {
    std::fs::write(dir.path().join("codex-seed.json"), codex_credential()).expect("seed codex");
    std::fs::write(
        dir.path().join("claude-imported.json"),
        claude_credential().to_string(),
    )
    .expect("seed claude");
    Arc::new(AppState::new(&config_for(dir)).expect("state"))
}

fn member_id_for_provider(state: &AppState, provider: &str) -> String {
    state
        .pool
        .load()
        .members
        .iter()
        .find(|member| member.provider_name() == provider)
        .unwrap_or_else(|| panic!("no member for provider {provider}"))
        .id
        .clone()
}

fn sample() -> UsageSample {
    UsageSample {
        unix: 1_800_000,
        requests: 1,
        tokens: 10,
        cost_usd: None,
    }
}

/// Seeds every reconciled store with state for `id`. Each seed asserts its own
/// precondition, so a silently ungated write fails loudly instead of leaving an
/// assertion that can never distinguish the fix from the bug.
fn seed_store_state(state: &AppState, id: &str) {
    assert!(!state.usage_samples.push(id, sample()).is_empty());
    assert!(state.monitor.record_ttft(id, 5.0));
    assert!(state.monitor.record_error(id, 500, "seeded"));
    assert!(state
        .warmup
        .record_probe(id, (tokio::time::Instant::now(), 1_800_000, None)));
    assert!(state.scheduler.reserve(&format!("inst-{id}"), id).is_ok());
    state
        .usage_poll_backoff
        .lock()
        .unwrap()
        .insert(id.to_string(), 4_200_000_000);
    let key = mahoquot_gateway::devin_catalog::DevinCacheKey::new(id, "fixture-token", "http://127.0.0.1:18849");
    let catalog = mahoquot_gateway::devin_catalog::DevinAccountCatalogState::new_success(
        key.clone(),
        Vec::new(),
        1_800_000,
    );
    state.devin_cache.insert(key, Arc::new(catalog));
}

async fn delete_credential(app: axum::Router, file_name: &str) -> StatusCode {
    app.oneshot(
        Request::builder()
            .method(Method::DELETE)
            .uri(format!("/v0/management/auth-files?name={file_name}"))
            .header(header::AUTHORIZATION, format!("Bearer {TEST_API_KEY}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .expect("delete request")
    .status()
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

#[tokio::test]
async fn deleting_an_account_prunes_seeded_stores_and_keeps_the_survivor() {
    let dir = TestDir::new("prune");
    let state = state_with_two_credentials(&dir);

    let removed = member_id_for_provider(&state, "codex");
    let kept = member_id_for_provider(&state, "claude");
    seed_store_state(&state, &removed);
    seed_store_state(&state, &kept);
    assert!(state.is_active_account(&removed));
    assert!(state.is_active_account(&kept));

    let status = delete_credential(create_app(Arc::clone(&state)), "codex-seed.json").await;
    assert_eq!(status, StatusCode::OK);

    assert!(!state.is_active_account(&removed));
    assert!(state.is_active_account(&kept));
    assert_eq!(state.usage_samples.tracked_accounts(), vec![kept.clone()]);
    assert_eq!(state.monitor.tracked_accounts(), vec![kept.clone()]);
    assert_eq!(state.warmup.tracked_accounts(), vec![kept.clone()]);
    assert_eq!(state.devin_cache.tracked_accounts(), vec![kept.clone()]);
    let reservations = state.scheduler.reservations();
    let reserved: Vec<&String> = reservations.values().collect();
    assert_eq!(reserved, vec![&kept]);
    let backoff = state.usage_poll_backoff.lock().unwrap();
    assert!(backoff.get(&removed).is_none());
    assert!(backoff.get(&kept).is_some());
}

#[tokio::test]
async fn an_in_flight_completion_after_deletion_cannot_resurrect_state() {
    let dir = TestDir::new("inflight");
    let state = state_with_two_credentials(&dir);

    let removed = member_id_for_provider(&state, "codex");
    seed_store_state(&state, &removed);

    let status = delete_credential(create_app(Arc::clone(&state)), "codex-seed.json").await;
    assert_eq!(status, StatusCode::OK);

    assert!(state.usage_samples.push(&removed, sample()).is_empty());
    assert!(!state.monitor.record_ttft(&removed, 9.0));
    assert!(!state.monitor.record_error(&removed, 503, "late"));
    assert!(!state
        .warmup
        .record_probe(&removed, (tokio::time::Instant::now(), 1_800_060, None)));

    assert!(state.usage_samples.tracked_accounts().is_empty());
    assert!(state.devin_cache.tracked_accounts().is_empty());
    assert!(state.monitor.tracked_accounts().is_empty());
    assert!(state.warmup.tracked_accounts().is_empty());
    assert!(state.monitor.account_ttft(&removed).is_none());
}

#[tokio::test]
async fn a_rescan_without_removal_keeps_every_store_intact() {
    let dir = TestDir::new("rescan-keep");
    let state = state_with_two_credentials(&dir);

    let codex = member_id_for_provider(&state, "codex");
    let claude = member_id_for_provider(&state, "claude");
    seed_store_state(&state, &codex);
    seed_store_state(&state, &claude);

    state.rescan_pool().expect("rescan");

    let mut tracked = state.usage_samples.tracked_accounts();
    tracked.sort();
    let mut expected = vec![claude.clone(), codex.clone()];
    expected.sort();
    assert_eq!(tracked, expected);
    assert_eq!(state.monitor.tracked_accounts().len(), 2);
    assert_eq!(state.warmup.tracked_accounts().len(), 2);
    assert_eq!(state.scheduler.reservations().len(), 2);
    assert_eq!(state.usage_poll_backoff.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn router_admission_rejects_unpolled_body_past_ceiling_and_releases_exactly_one_slot() {
    let dir = TestDir::new("gate");
    let state = Arc::new(AppState::new(&config_for(&dir)).expect("state"));
    let app = create_app(Arc::clone(&state));
    let gate = state.inference_gate();
    let ceiling = gate.available_permits();
    let polled = Arc::new(tokio::sync::Semaphore::new(0));
    let mut held = Vec::new();
    for _ in 0..ceiling {
        let body = pending_body(Arc::clone(&polled));
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header(header::AUTHORIZATION, format!("Bearer {TEST_API_KEY}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .unwrap();
        held.push(tokio::spawn(app.clone().oneshot(request)));
    }
    for _ in 0..ceiling {
        tokio::time::timeout(Duration::from_secs(5), polled.acquire())
            .await
            .expect("each admitted request must reach body extraction")
            .unwrap()
            .forget();
    }

    let overflow_signal = Arc::new(tokio::sync::Semaphore::new(0));
    let overflow_body = pending_body(Arc::clone(&overflow_signal));
    let overflow = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_API_KEY}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(overflow_body)
        .unwrap();
    let busy = app.clone().oneshot(overflow).await.expect("oneshot");
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    let busy_body = body_json(busy).await;
    assert_eq!(busy_body["error"]["code"], "gateway_busy");
    assert_eq!(busy_body["error"]["retryable"], true);
    assert_eq!(overflow_signal.available_permits(), 0, "rejected body was polled");

    let management = app.clone().oneshot(
        Request::builder()
            .method(Method::GET)
            .uri("/v0/management/config")
            .header(header::AUTHORIZATION, format!("Bearer {TEST_API_KEY}"))
            .body(Body::empty())
            .unwrap(),
    ).await.expect("management request");
    assert_eq!(management.status(), StatusCode::OK);

    held[0].abort();
    let _ = (&mut held[0]).await;
    assert_eq!(gate.available_permits(), 1, "aborting one request releases one slot");

    let replacement_signal = Arc::new(tokio::sync::Semaphore::new(0));
    let body = pending_body(Arc::clone(&replacement_signal));
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_API_KEY}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .unwrap();
    let replacement = tokio::spawn(app.oneshot(request));
    tokio::time::timeout(Duration::from_secs(5), replacement_signal.acquire())
        .await
        .expect("released slot admits replacement body")
        .unwrap()
        .forget();
    replacement.abort();
    let _ = replacement.await;
    for task in held.into_iter().skip(1) {
        task.abort();
        let _ = task.await;
    }
}

/// A hundred concurrent inference requests are ordinary traffic, not overload.
/// The shipped default ceiling must admit every one of them: a request refused
/// by admission never reaches body extraction, so counting one poll signal per
/// request proves all hundred were admitted rather than answered 503.
#[tokio::test]
async fn default_ceiling_admits_a_hundred_concurrent_inference_requests() {
    let dir = TestDir::new("burst");
    let state = Arc::new(
        AppState::new(&GatewayConfig {
            max_concurrent_inference: mahoquot_gateway::state::MAX_CONCURRENT_INFERENCE_REQUESTS,
            ..config_for(&dir)
        })
        .expect("state"),
    );
    let app = create_app(Arc::clone(&state));
    let gate = state.inference_gate();
    assert!(
        gate.available_permits() >= 100,
        "the shipped ceiling must cover a hundred concurrent requests, got {}",
        gate.available_permits()
    );

    const BURST: usize = 100;
    let polled = Arc::new(tokio::sync::Semaphore::new(0));
    let mut held = Vec::new();
    for _ in 0..BURST {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header(header::AUTHORIZATION, format!("Bearer {TEST_API_KEY}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(pending_body(Arc::clone(&polled)))
            .unwrap();
        held.push(tokio::spawn(app.clone().oneshot(request)));
    }
    for admitted in 0..BURST {
        tokio::time::timeout(Duration::from_secs(5), polled.acquire())
            .await
            .expect("every request inside the ceiling must reach body extraction")
            .unwrap()
            .forget();
        assert!(
            !held[admitted].is_finished(),
            "request {admitted} answered early instead of being admitted"
        );
    }

    for task in held {
        task.abort();
        let _ = task.await;
    }
}

#[tokio::test]
async fn declared_oversized_request_is_rejected_without_polling_body() {
    let dir = TestDir::new("oversized");
    let state = Arc::new(AppState::new(&config_for(&dir)).expect("state"));
    let polled = Arc::new(tokio::sync::Semaphore::new(0));
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        create_app(state).oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header(header::AUTHORIZATION, format!("Bearer {TEST_API_KEY}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONTENT_LENGTH, "536870913")
                .body(pending_body(Arc::clone(&polled)))
                .expect("request"),
        ),
    )
    .await
    .expect("declared size rejects before body extraction")
    .expect("response");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(polled.available_permits(), 0);
}

fn pending_body(polled: Arc<tokio::sync::Semaphore>) -> Body {
    let mut signaled = false;
    Body::from_stream(futures::stream::poll_fn(move |_cx| {
        if !signaled {
            signaled = true;
            polled.add_permits(1);
        }
        std::task::Poll::<Option<Result<bytes::Bytes, std::convert::Infallible>>>::Pending
    }))
}

#[tokio::test]
async fn worker_shutdowns_join_instead_of_detaching() {
    let dir = TestDir::new("workers");
    let state = Arc::new(AppState::new(&config_for(&dir)).expect("state"));
    assert!(state.pool.load().members.is_empty());

    let poller = mahoquot_gateway::quota::spawn_usage_poller(
        Arc::clone(&state),
        Duration::from_secs(3600),
    );
    let flush = state
        .telemetry
        .spawn_flush_worker(Duration::from_secs(3600));

    tokio::time::timeout(Duration::from_secs(5), poller.shutdown())
        .await
        .expect("usage poller shutdown must not hang");
    tokio::time::timeout(Duration::from_secs(5), flush.shutdown())
        .await
        .expect("flush worker shutdown must not hang");
}
