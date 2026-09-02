mod common;

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use common::unique_temp_dir;
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::quota::{
    consume_reset_credit, reset_attempt_policy, retain_redeem_request_id, ResetAttemptPolicy,
};
use mahoquot_gateway::state::AppState;
use mahoquot_gateway::usage::{parse_cursor_usage_summary, parse_kiro_usage_summary};

#[test]
fn cursor_and_kiro_quota_payloads_normalize_to_account_usage() {
    let cursor = parse_cursor_usage_summary(
        &serde_json::json!({
            "membershipType": "pro",
            "billingCycleEnd": "2030-01-01T00:00:00Z",
            "individualUsage": {
                "plan": { "enabled": true, "limit": 1000, "remaining": 250 },
                "onDemand": { "enabled": true, "limit": 100, "remaining": 80 }
            }
        }),
        1_800_000_000,
    );
    assert_eq!(cursor.plan_type.as_deref(), Some("pro"));
    assert_eq!(cursor.groups[0].buckets[0].used_percent, Some(75.0));
    assert_eq!(cursor.groups[0].buckets[1].used_percent, Some(20.0));

    let kiro = parse_kiro_usage_summary(
        &serde_json::json!({
            "usageBreakdownList": [
                { "displayName": "Agentic requests", "currentUsage": 30, "usageLimit": 100, "nextDateReset": 1900000000 }
            ]
        }),
        1_800_000_000,
    );
    assert_eq!(kiro.groups[0].buckets[0].used_percent, Some(30.0));
    assert_eq!(kiro.groups[0].buckets[0].reset_at_unix, Some(1_900_000_000));
}

#[derive(Clone, Default)]
struct ResetMockState {
    attempts: Arc<Mutex<Vec<(String, String)>>>,
}

#[tokio::test]
async fn reset_credit_refresh_retry_is_idempotent() {
    let mock_state = ResetMockState::default();
    let attempts = Arc::clone(&mock_state.attempts);
    let mock = Router::new()
        .route(
            "/backend-api/wham/rate-limit-reset-credits/consume",
            post(
                |State(state): State<ResetMockState>, headers: HeaderMap, body: String| async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    let redeem_id = serde_json::from_str::<serde_json::Value>(&body).unwrap()
                        ["redeem_request_id"]
                        .as_str()
                        .unwrap()
                        .to_string();
                    let mut calls = state.attempts.lock().unwrap();
                    calls.push((auth.clone(), redeem_id));
                    if auth == "Bearer stale-token" {
                        (StatusCode::UNAUTHORIZED, "token expired")
                    } else {
                        (StatusCode::OK, "")
                    }
                },
            ),
        )
        .route(
            "/backend-api/wham/usage",
            get(|| async {
                axum::Json(serde_json::json!({
                    "rate_limit_reset_credits": { "available_count": 0 }
                }))
            }),
        )
        .route(
            "/oauth/token",
            post(|| async {
                axum::Json(serde_json::json!({
                    "access_token": "fresh-token",
                    "refresh_token": "fresh-refresh",
                    "expires_in": 3600
                }))
            }),
        )
        .with_state(mock_state);
    let mut listener = None;
    for port in 18840..=18899 {
        if let Ok(bound) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            listener = Some(bound);
            break;
        }
    }
    let listener = listener.expect("reserved test port range 18840-18899 is exhausted");
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

    let auth_dir = unique_temp_dir("reset-refresh-retry");
    std::fs::write(
        auth_dir.join("codex-reset.json"),
        serde_json::to_vec(&serde_json::json!({
            "access_token": "stale-token",
            "refresh_token": "refresh-token",
            "account_id": "account-1",
            "email": "reset@example.test",
            "expired": "2099-01-01T00:00:00Z",
            "id_token": "id",
            "last_refresh": "2026-01-01T00:00:00Z",
            "type": "plus",
            "upstream_override": base
        }))
        .unwrap(),
    )
    .unwrap();
    let state = AppState::new(&GatewayConfig {
        auth_dir: auth_dir.clone(),
        config_path: auth_dir.join("config.yaml"),
        refresh_url: format!("{base}/oauth/token"),
        auth_refresh_enabled: true,
        ..GatewayConfig::default()
    })
    .unwrap();
    let member = state.pool.load().members[0].clone();

    consume_reset_credit(&state, &member).await.unwrap();

    let calls = attempts.lock().unwrap();
    assert_eq!(calls.len(), 2, "reset performs at most one retry");
    assert_eq!(calls[0].0, "Bearer stale-token");
    assert_eq!(calls[1].0, "Bearer fresh-token");
    assert_eq!(calls[0].1, calls[1].1, "retry reuses redeem_request_id");
    assert_eq!(member.access_token(), "fresh-token");
    assert_eq!(member.usage_snapshot().reset_credits_available, Some(0));
    std::fs::remove_dir_all(auth_dir).ok();
}

#[test]
fn reset_credit_policy_retains_redeem_id() {
    assert_eq!(
        reset_attempt_policy(StatusCode::UNAUTHORIZED, false, "token expired"),
        ResetAttemptPolicy::RefreshAndRetry
    );
    assert_eq!(
        retain_redeem_request_id("redeem-first", "redeem-second"),
        "redeem-first"
    );
}

#[tokio::test]
async fn reset_credit_errors_are_truthful_and_distinct() {
    let auth_dir = unique_temp_dir("reset-distinct-errors");
    std::fs::write(
        auth_dir.join("codex-reset.json"),
        serde_json::to_vec(&serde_json::json!({
            "access_token": "token",
            "refresh_token": "refresh",
            "account_id": "account-1",
            "email": "reset@example.test",
            "expired": "2099-01-01T00:00:00Z",
            "id_token": "id",
            "last_refresh": "2026-01-01T00:00:00Z",
            "type": "plus"
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        auth_dir.join("claude-plain.json"),
        serde_json::to_vec(&serde_json::json!({
            "type": "claude",
            "access_token": "token",
            "refresh_token": "refresh",
            "email": "claude@example.test",
            "expired": "2099-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    let state = AppState::new(&GatewayConfig {
        auth_dir: auth_dir.clone(),
        config_path: auth_dir.join("config.yaml"),
        auth_refresh_enabled: false,
        ..GatewayConfig::default()
    })
    .unwrap();
    let codex_member = state
        .pool
        .load()
        .members
        .iter()
        .find(|m| m.kind() == mahoquot_gateway::account::ProviderKind::Codex)
        .unwrap()
        .clone();
    let claude_member = state
        .pool
        .load()
        .members
        .iter()
        .find(|m| m.kind() == mahoquot_gateway::account::ProviderKind::Claude)
        .unwrap()
        .clone();

    assert!(matches!(
        consume_reset_credit(&state, &claude_member).await,
        Err(mahoquot_gateway::quota::QuotaError::Unsupported)
    ));

    codex_member.set_usage(mahoquot_gateway::usage::AccountUsage {
        reset_credits_available: Some(0),
        ..Default::default()
    });
    assert!(matches!(
        consume_reset_credit(&state, &codex_member).await,
        Err(mahoquot_gateway::quota::QuotaError::NoCredit)
    ));
    std::fs::remove_dir_all(auth_dir).ok();
}

#[test]
fn reset_credit_no_credit_is_distinct() {
    assert_eq!(
        reset_attempt_policy(StatusCode::BAD_REQUEST, false, "no reset credits available"),
        ResetAttemptPolicy::NoCredit
    );
}

#[test]
fn reset_credit_other_failure_is_upstream_error() {
    assert_eq!(
        reset_attempt_policy(StatusCode::SERVICE_UNAVAILABLE, false, "offline"),
        ResetAttemptPolicy::Upstream
    );
}
