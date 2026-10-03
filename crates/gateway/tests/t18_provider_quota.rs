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

#[tokio::test]
async fn reset_credit_route_selects_codex_when_another_provider_shares_id() {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use mahoquot_gateway::account::ProviderKind;
    use mahoquot_gateway::inbound::ApiKeys;
    use tower::ServiceExt;

    // Given a non-Codex account appears first with the same runtime id.
    let auth_dir = unique_temp_dir("reset-provider-collision");
    for (name, provider) in [("a-other.json", "antigravity"), ("b-codex.json", "codex")] {
        std::fs::write(auth_dir.join(name), serde_json::to_vec(&serde_json::json!({
            "type": provider,
            "identity_slug": "shared-reset",
            "access_token": "token",
            "refresh_token": "refresh",
            "project_id": "reset-project",
            "account_id": "account",
            "email": "reset@example.test",
            "expired": "2099-01-01T00:00:00Z",
            "id_token": "id",
            "last_refresh": "2026-01-01T00:00:00Z",
            "upstream_override": "http://127.0.0.1:18899"
        })).unwrap()).unwrap();
    }
    let state = Arc::new(AppState::new(&GatewayConfig {
        auth_dir: auth_dir.clone(),
        config_path: auth_dir.join("config.yaml"),
        api_keys: ApiKeys::new(vec!["reset-test".to_string()]),
        auth_refresh_enabled: false,
        ..GatewayConfig::default()
    }).unwrap());
    assert_eq!(state.find_member("shared-reset").unwrap().kind(), ProviderKind::Antigravity);
    let codex = state.pool.load().members.iter().find(|m| m.kind() == ProviderKind::Codex).unwrap().clone();
    assert_eq!(codex.id, "shared-reset");
    codex.set_usage(mahoquot_gateway::usage::AccountUsage {
        reset_credits_available: Some(0),
        credits_balance: Some(0.0),
        ..Default::default()
    });

    // When the real reset route resolves the shared id (no upstream needed).
    let response = mahoquot_gateway::routes::create_app(state).oneshot(
        Request::builder().method("POST").uri("/admin/accounts/shared-reset/reset")
            .header("Authorization", "Bearer reset-test").body(Body::empty()).unwrap()
    ).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    std::fs::remove_dir_all(auth_dir).unwrap();

    // Then Codex's empty banked-reset balance is reported, not unsupported provider.
    assert_eq!(status, StatusCode::CONFLICT, "{}", String::from_utf8_lossy(&body));
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

#[test]
fn partial_header_updates_preserve_reset_credits_and_poll_freshness() {
    let auth_dir = unique_temp_dir("codex-partial-headers");
    std::fs::write(
        auth_dir.join("codex-plain.json"),
        common::create_auth_file_json("quota-partial", "acc-123", "token", Some("http://127.0.0.1:18899")),
    )
    .unwrap();
    let state = AppState::new(&GatewayConfig {
        auth_dir: auth_dir.clone(),
        config_path: auth_dir.join("config.yaml"),
        auth_refresh_enabled: false,
        ..GatewayConfig::default()
    })
    .unwrap();
    let member = state
        .pool
        .load()
        .members
        .iter()
        .find(|m| m.kind() == mahoquot_gateway::account::ProviderKind::Codex)
        .unwrap()
        .clone();

    // 1. Establish full baseline usage (simulating a full poll)
    member.set_usage(mahoquot_gateway::usage::AccountUsage {
        plan_type: Some("pro".to_string()),
        primary: mahoquot_gateway::usage::QuotaWindow {
            used_percent: Some(25.0),
            window_minutes: Some(300),
            reset_after_seconds: Some(12000),
            reset_at_unix: Some(1_800_012_000),
            limit_name: None,
        },
        reset_credits_available: Some(2),
        reset_credits: vec![mahoquot_gateway::usage::ResetCredit {
            granted_at_unix: Some(1_790_000_000),
            expires_at_unix: Some(1_792_000_000),
            status: Some("active".to_string()),
        }],
        refreshed_at_unix: Some(1_800_000_000),
        observed_at_unix: Some(1_800_000_000),
        refresh_status: Some("ok".to_string()),
        ..Default::default()
    });

    // 2. Partial header update arrives on a request
    let partial_header_usage = mahoquot_gateway::usage::AccountUsage {
        primary: mahoquot_gateway::usage::QuotaWindow {
            used_percent: Some(40.0),
            window_minutes: Some(300),
            reset_after_seconds: Some(11000),
            reset_at_unix: Some(1_800_012_000),
            limit_name: None,
        },
        observed_at_unix: Some(1_800_001_000),
        ..Default::default()
    };
    member.update_usage_from_headers(partial_header_usage);

    let snapshot = member.usage_snapshot();
    // Headers updated observed_at and primary used_percent:
    assert_eq!(snapshot.primary.used_percent, Some(40.0));
    assert_eq!(snapshot.observed_at_unix, Some(1_800_001_000));

    // Crucial: poll-only fields are preserved intact!
    assert_eq!(snapshot.reset_credits_available, Some(2));
    assert_eq!(snapshot.reset_credits.len(), 1);
    assert_eq!(snapshot.refreshed_at_unix, Some(1_800_000_000));
    assert_eq!(snapshot.refresh_status.as_deref(), Some("ok"));

    std::fs::remove_dir_all(auth_dir).ok();
}

#[test]
fn successful_snapshot_clears_absent_fields_and_header_update_merges_by_window() {
    use mahoquot_gateway::usage::{AccountUsage, QuotaWindow};

    let member = mahoquot_gateway::account::AccountMember::for_test_with_id(
        "quota-snapshot",
        mahoquot_gateway::account::ProviderAccount::Generic(
            mahoquot_gateway::account::GenericAccount {
                identity_slug: "quota-snapshot".to_string(),
                provider: "openai".to_string(),
                label: "test".to_string(),
                email: String::new(),
                adapter: "openai-chat".to_string(),
                base_url: "https://example.test".to_string(),
                api_key: "key".to_string(),
                auth_mode: "key".to_string(),
                refresh_token: String::new(),
                expired: String::new(),
                token_url: String::new(),
                client_id: String::new(),
                project_id: String::new(),
                models: Vec::new(),
                static_headers: Default::default(),
                disabled: false,
            },
        ),
    );
    member.record_quota_refresh_failure("old error");
    member.set_usage(AccountUsage {
        refreshed_at_unix: Some(1000),
        last_refresh_error: Some("old error".into()),
        refresh_status: Some("ok".into()),
        groups: vec![mahoquot_gateway::usage::QuotaGroup {
            models: Some("old-model".into()),
            ..Default::default()
        }],
        model_availability: Some(Default::default()),
        ..Default::default()
    });
    member.set_usage(AccountUsage {
        refreshed_at_unix: Some(1100),
        refresh_status: Some("ok".into()),
        ..Default::default()
    });
    let snapshot = member.usage_snapshot_at(1100);
    assert_eq!(snapshot.last_refresh_error, None);
    assert!(snapshot.groups.is_empty());
    assert_eq!(snapshot.model_availability, None);

    let mut windows = AccountUsage {
        primary: QuotaWindow {
            used_percent: Some(10.0),
            window_minutes: Some(300),
            reset_at_unix: Some(2000),
            ..Default::default()
        },
        ..Default::default()
    };
    windows.apply_header_update(AccountUsage {
        primary: QuotaWindow {
            used_percent: Some(45.0),
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(windows.primary.used_percent, Some(45.0));
    assert_eq!(windows.primary.window_minutes, Some(300));
    assert_eq!(windows.primary.reset_at_unix, Some(2000));
    windows.apply_header_update(AccountUsage {
        secondary: QuotaWindow {
            used_percent: Some(5.0),
            window_minutes: Some(10080),
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(windows.primary.window_minutes, Some(300));
    assert_eq!(windows.secondary.window_minutes, Some(10080));

    let mut short_only = AccountUsage {
        primary: QuotaWindow {
            used_percent: Some(20.0),
            window_minutes: Some(300),
            ..Default::default()
        },
        ..Default::default()
    };
    short_only.apply_header_update(AccountUsage {
        primary: QuotaWindow {
            used_percent: Some(60.0),
            window_minutes: Some(10080),
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(short_only.primary.window_minutes, Some(300));
    assert_eq!(short_only.primary.used_percent, Some(20.0));
    assert_eq!(short_only.secondary.window_minutes, Some(10080));
    assert_eq!(short_only.secondary.used_percent, Some(60.0));
}

#[test]
fn quota_snapshot_age_marks_only_successful_snapshot_stale() {
    let member = mahoquot_gateway::account::AccountMember::for_test_with_id(
        "quota-age",
        mahoquot_gateway::account::ProviderAccount::Generic(
            mahoquot_gateway::account::GenericAccount {
                identity_slug: "quota-age".to_string(),
                provider: "openai".to_string(),
                label: "test".to_string(),
                email: String::new(),
                adapter: "openai-chat".to_string(),
                base_url: "https://example.test".to_string(),
                api_key: "key".to_string(),
                auth_mode: "key".to_string(),
                refresh_token: String::new(),
                expired: String::new(),
                token_url: String::new(),
                client_id: String::new(),
                project_id: String::new(),
                models: Vec::new(),
                static_headers: Default::default(),
                disabled: false,
            },
        ),
    );
    member.set_usage(mahoquot_gateway::usage::AccountUsage {
        refreshed_at_unix: Some(1000),
        refresh_status: Some("ok".into()),
        ..Default::default()
    });
    assert_eq!(member.usage_snapshot_at(1360).refresh_status.as_deref(), Some("ok"));
    assert_eq!(member.usage_snapshot_at(1361).refresh_status.as_deref(), Some("stale"));
    member.record_quota_refresh_failure("429");
    assert_eq!(member.usage_snapshot_at(2000).refresh_status.as_deref(), Some("error"));
}

#[test]
fn quota_refresh_failure_records_error_without_discarding_last_valid_snapshot() {
    let auth_dir = unique_temp_dir("codex-refresh-failure");
    std::fs::write(
        auth_dir.join("codex-plain.json"),
        common::create_auth_file_json("quota-failure", "acc-123", "token", Some("http://127.0.0.1:18899")),
    )
    .unwrap();
    let state = AppState::new(&GatewayConfig {
        auth_dir: auth_dir.clone(),
        config_path: auth_dir.join("config.yaml"),
        auth_refresh_enabled: false,
        ..GatewayConfig::default()
    })
    .unwrap();
    let member = state
        .pool
        .load()
        .members
        .iter()
        .find(|m| m.kind() == mahoquot_gateway::account::ProviderKind::Codex)
        .unwrap()
        .clone();

    member.set_usage(mahoquot_gateway::usage::AccountUsage {
        plan_type: Some("plus".to_string()),
        primary: mahoquot_gateway::usage::QuotaWindow {
            used_percent: Some(30.0),
            window_minutes: Some(300),
            ..Default::default()
        },
        reset_credits_available: Some(1),
        refreshed_at_unix: Some(1_790_000_000),
        refresh_status: Some("ok".to_string()),
        ..Default::default()
    });

    // Simulate poll error
    member.record_quota_refresh_failure("Rate limited (429)");

    let snapshot = member.usage_snapshot();
    // Error recorded:
    assert_eq!(snapshot.last_refresh_error.as_deref(), Some("Rate limited (429)"));
    assert_eq!(snapshot.refresh_status.as_deref(), Some("error"));

    // Previous valid snapshot preserved:
    assert_eq!(snapshot.primary.used_percent, Some(30.0));
    assert_eq!(snapshot.reset_credits_available, Some(1));
    assert_eq!(snapshot.refreshed_at_unix, Some(1_790_000_000));
    assert_eq!(snapshot.plan_type.as_deref(), Some("plus"));

    std::fs::remove_dir_all(auth_dir).ok();
}
