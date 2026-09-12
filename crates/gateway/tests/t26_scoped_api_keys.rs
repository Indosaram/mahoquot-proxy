use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::management::settings::{ScopedApiKey, Settings};
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use tower::ServiceExt;

mod common;

const MASTER: &str = "master-key";
const SCOPED_RAW: &str = "scoped-raw-key";

fn scoped_key(is_active: bool, expires_at_ms: Option<i64>) -> ScopedApiKey {
    ScopedApiKey {
        id: "sk-1".to_string(),
        name: "delegated".to_string(),
        key_identifier: mahoquot_gateway::request_history::stable_key_identifier(SCOPED_RAW),
        key_prefix: "scoped-r".to_string(),
        raw_key: Some(SCOPED_RAW.to_string()),
        allowed_providers: vec!["anthropic".to_string()],
        allowed_accounts: Vec::new(),
        allowed_models: Vec::new(),
        token_limit: 1_000,
        token_used: 0,
        is_active,
        created_at_ms: 0,
        expires_at_ms,
    }
}

fn state_with(scoped: Vec<ScopedApiKey>) -> Arc<AppState> {
    let dir = common::unique_temp_dir("qg-t26-scoped");
    let config_path = dir.join("config.yaml");
    let settings = Settings {
        api_keys: vec![MASTER.to_string()],
        scoped_api_keys: scoped,
        auth_dir: dir.to_string_lossy().to_string(),
        ..Settings::default()
    };
    settings.persist(&config_path).expect("persist config");
    let config = GatewayConfig {
        auth_dir: dir.clone(),
        config_path,
        ..GatewayConfig::default()
    };
    Arc::new(AppState::new(&config).expect("state"))
}

async fn status_for(state: Arc<AppState>, path: &str, key: &str) -> StatusCode {
    let app = create_app(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    response.status()
}

#[tokio::test]
async fn a_scoped_key_authenticates_on_the_relay_surface() {
    // given a gateway with one active scoped key
    let state = state_with(vec![scoped_key(true, None)]);
    // when it calls a non-management authed route
    let status = status_for(state, "/v1/models", SCOPED_RAW).await;
    // then it is accepted
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_scoped_key_is_forbidden_on_management_and_admin_routes() {
    // given a gateway with one active scoped key
    let state = state_with(vec![scoped_key(true, None)]);
    // when it reaches for the control plane
    for path in ["/v0/management/api-keys", "/admin/stats"] {
        let status = status_for(Arc::clone(&state), path, SCOPED_RAW).await;
        // then it is refused with 403 rather than served
        assert_eq!(status, StatusCode::FORBIDDEN, "path: {path}");
    }
}

#[tokio::test]
async fn a_master_key_retains_control_plane_access() {
    // given the same gateway
    let state = state_with(vec![scoped_key(true, None)]);
    // when the master key calls the control plane
    for path in ["/v0/management/api-keys", "/admin/stats"] {
        let status = status_for(Arc::clone(&state), path, MASTER).await;
        // then it is served
        assert_eq!(status, StatusCode::OK, "path: {path}");
    }
}

#[tokio::test]
async fn inactive_and_expired_scoped_keys_are_rejected() {
    // given an inactive key and an already-expired key
    let inactive = state_with(vec![scoped_key(false, None)]);
    let expired = state_with(vec![scoped_key(true, Some(1))]);
    // when either is presented
    let inactive_status = status_for(inactive, "/v1/models", SCOPED_RAW).await;
    let expired_status = status_for(expired, "/v1/models", SCOPED_RAW).await;
    // then neither authenticates
    assert_eq!(inactive_status, StatusCode::UNAUTHORIZED);
    assert_eq!(expired_status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_tracker_indexes_and_charges_scoped_keys_in_place() {
    // given a tracked scoped key
    let state = state_with(vec![scoped_key(true, None)]);
    let identifier = mahoquot_gateway::request_history::stable_key_identifier(SCOPED_RAW);
    let entry = state.scoped_keys.get(&identifier).expect("indexed");
    assert_eq!(entry.token_used(), 0);
    assert!(!entry.is_exhausted());

    // when usage is recorded against it
    state.scoped_keys.record_usage(Some(&identifier), 600);
    state.scoped_keys.record_usage(Some(&identifier), 600);

    // then the counter moves and the limit trips
    let entry = state.scoped_keys.get(&identifier).expect("indexed");
    assert_eq!(entry.token_used(), 1_200);
    assert!(entry.is_exhausted());
}

#[tokio::test]
async fn revoking_a_scoped_key_through_settings_takes_effect_without_a_restart() {
    // given a live scoped key
    let state = state_with(vec![scoped_key(true, None)]);
    let identifier = mahoquot_gateway::request_history::stable_key_identifier(SCOPED_RAW);
    assert!(state.scoped_keys.get(&identifier).is_some());

    // when it is dropped from the settings document
    state
        .settings
        .mutate(|settings| settings.scoped_api_keys.clear())
        .expect("mutates");

    // then the index drops it and the key no longer authenticates
    assert!(state.scoped_keys.get(&identifier).is_none());
    assert_eq!(
        status_for(state, "/v1/models", SCOPED_RAW).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn reconcile_carries_live_usage_across_a_settings_republish() {
    // given a tracker with spent allowance
    let state = state_with(vec![scoped_key(true, None)]);
    let identifier = mahoquot_gateway::request_history::stable_key_identifier(SCOPED_RAW);
    state.scoped_keys.record_usage(Some(&identifier), 250);

    // when the settings document is republished with a renamed key
    let mut renamed = scoped_key(true, None);
    renamed.name = "renamed".to_string();
    state.scoped_keys.reconcile(&[renamed]);

    // then the live counter survives rather than resetting to the persisted 0
    let entry = state.scoped_keys.get(&identifier).expect("indexed");
    assert_eq!(entry.token_used(), 250);
    assert_eq!(entry.key.name, "renamed");
}

#[test]
fn rotation_shares_stable_id_counter_with_in_flight_request() {
    let key = scoped_key(true, None);
    let tracker = mahoquot_gateway::state::ScopedKeyTracker::new(std::slice::from_ref(&key));
    let in_flight = tracker.lookup_raw(SCOPED_RAW).unwrap();
    in_flight.consume(450);
    let mut rotated = key;
    rotated.key_identifier = mahoquot_gateway::request_history::stable_key_identifier("rotated");
    tracker.reconcile(&[rotated.clone()]);
    let current = tracker.lookup_raw("rotated").unwrap();
    assert_eq!(current.token_used(), 450, "rotation retains live usage");
    assert!(tracker.lookup_raw(SCOPED_RAW).is_none());
    in_flight.consume(600);
    assert_eq!(current.token_used(), 1050, "in-flight completion charges the same counter");
    assert!(current.is_exhausted());
    rotated.token_used = 1100;
    tracker.reconcile(&[rotated]);
    in_flight.consume(20);
    assert_eq!(tracker.lookup_raw("rotated").unwrap().token_used(), 1120);
}

#[test]
fn rotation_persists_in_flight_stream_usage_across_restart() {
    use http_body_util::BodyExt;
    let dir = common::unique_temp_dir("review-rotation");
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
    let mut bound = None;
    for port in 18840..=18899 {
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => { bound = Some(listener); break; }
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => panic!("mock bind failed: {error}"),
        }
    }
    let listener = bound.expect("no available mock port in 18840-18899");
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut credential: serde_json::Value = serde_json::from_str(&common::create_auth_file_json("rotation", "rotation", "mock", Some(&url))).unwrap();
    credential["usage_override"] = serde_json::json!(url);
    std::fs::write(dir.join("codex-rotation.json"), credential.to_string()).unwrap();
    let mut key = scoped_key(true, None);
    key.allowed_providers.clear();
    let config = GatewayConfig { auth_dir: dir.clone(), config_path: dir.join("config.yaml"), auth_refresh_enabled: false, ..GatewayConfig::default() };
    Settings { api_keys: vec![MASTER.into()], scoped_api_keys: vec![key], auth_dir: dir.to_string_lossy().into_owned(), ..Settings::default() }.persist(&config.config_path).unwrap();
    let state = Arc::new(AppState::new(&config).unwrap());
    state.scoped_keys.lookup_raw(SCOPED_RAW).unwrap().consume(450);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(2);
    let rx = Arc::new(tokio::sync::Mutex::new(Some(rx)));
    let upstream = axum::Router::new().route(common::CODEX_PATH, axum::routing::post(move || {
        let rx = rx.clone();
        async move {
            let rx = rx.lock().await.take().unwrap();
            axum::response::Response::builder().header("content-type", "text/event-stream")
                .body(Body::from_stream(futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) }))).unwrap()
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap(); });
    tx.send(Ok(bytes::Bytes::from_static(b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n"))).await.unwrap();
    let app = create_app(state.clone());
    let response = app.clone().oneshot(Request::post(common::CODEX_PATH).header("Authorization", format!("Bearer {SCOPED_RAW}")).header("content-type", "application/json").body(Body::from(r#"{"model":"gpt-5.6-sol","stream":true,"input":"hi"}"#)).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    tokio::time::timeout(std::time::Duration::from_secs(5), body.frame()).await.unwrap().unwrap().unwrap();
    let response = app.clone().oneshot(Request::patch("/v0/management/scoped-keys/sk-1").header("Authorization", format!("Bearer {MASTER}")).header("content-type", "application/json").body(Body::from(r#"{"raw_key":"rotated-secret"}"#)).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let view: serde_json::Value = serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(view["key"]["token_used"], 450);
    let (persisted_tx, persisted_rx) = tokio::sync::oneshot::channel();
    let persisted_tx = std::sync::Mutex::new(Some(persisted_tx));
    state.settings.add_observer(Arc::new(move |settings| {
        if settings.scoped_api_keys[0].token_used == 1050 {
            if let Some(tx) = persisted_tx.lock().unwrap().take() { tx.send(()).unwrap(); }
        }
    }));
    tx.send(Ok(bytes::Bytes::from_static(b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"usage\":{\"input_tokens\":400,\"output_tokens\":200}}}\n\n"))).await.unwrap();
    drop(tx);
    tokio::time::timeout(std::time::Duration::from_secs(5), body.collect()).await.unwrap().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), persisted_rx).await.unwrap().unwrap();
    assert_eq!(state.scoped_keys.lookup_raw("rotated-secret").unwrap().token_used(), 1050);
    assert_eq!(status_for(state.clone(), "/v1/models", SCOPED_RAW).await, StatusCode::UNAUTHORIZED);
    drop(app);
    drop(state);
    let restarted = Arc::new(AppState::new(&config).unwrap());
    let entry = restarted.scoped_keys.lookup_raw("rotated-secret").unwrap();
    assert_eq!(entry.token_used(), 1050);
    assert!(entry.is_exhausted());
    assert!(entry.key.raw_key.is_none(), "raw secrets are never persisted");
    let denied = create_app(restarted.clone()).oneshot(Request::post(common::CODEX_PATH).header("Authorization", "Bearer rotated-secret").header("content-type", "application/json").body(Body::from(r#"{"model":"gpt-5.6-sol","input":"hi"}"#)).unwrap()).await.unwrap();
    assert_eq!(denied.status(), StatusCode::TOO_MANY_REQUESTS);
    drop(restarted);
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    });
    drop(runtime);
    std::fs::remove_dir_all(dir).unwrap();
}
