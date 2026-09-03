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
