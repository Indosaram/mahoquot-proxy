mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::StatusCode;
use axum::routing::post;
use axum::Router;
use common::{create_auth_file_json, unique_temp_dir};
use mahoquot_gateway::{
    config::GatewayConfig,
    routes::create_app,
    state::AppState,
    usage::{AccountUsage, QuotaBucket, QuotaGroup, QuotaWindow},
};
use mahoquot_types::Strategy;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn bind_fixture_listener() -> tokio::net::TcpListener {
    for port in 18840..=18899 {
        match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => return listener,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => panic!("fixture bind failed on {port}: {error}"),
        }
    }
    panic!("no available fixture port in 18840-18899");
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[tokio::test]
async fn management_endpoints_mutate_and_persist_opt_in() {
    let temp_dir = unique_temp_dir("codex-credits-mgmt");
    let json_1 = create_auth_file_json("codex-1", "acc_1", "tok_1", None);
    let json_2 = create_auth_file_json("codex-2", "acc_2", "tok_2", None);
    std::fs::write(temp_dir.join("codex-1.json"), json_1).unwrap();
    std::fs::write(temp_dir.join("codex-2.json"), json_2).unwrap();
    std::fs::write(temp_dir.join("antigravity-duplicate.json"), json!({
        "type": "antigravity",
        "identity_slug": "codex-1",
        "access_token": "other-token",
        "refresh_token": "other-refresh",
        "project_id": "test-project",
        "email": "other@example.test",
        "expired": "2099-01-01T00:00:00Z"
    }).to_string()).unwrap();

    let claude_json = serde_json::json!({
        "type": "claude",
        "email": "claude@example.com",
        "access_token": "tok_claude"
    });
    std::fs::write(temp_dir.join("claude-1.json"), claude_json.to_string()).unwrap();

    let config_path = temp_dir.join("config.yaml");
    std::fs::write(&config_path, "logging-to-file: false\n").unwrap();

    let state = Arc::new(
        AppState::new(&GatewayConfig {
            auth_dir: temp_dir.clone(),
            config_path: config_path.clone(),
            api_keys: mahoquot_gateway::inbound::ApiKeys::from_env_value("test-key"),
            auth_refresh_enabled: false,
            ..GatewayConfig::default()
        })
        .unwrap(),
    );
    let app = create_app(Arc::clone(&state));
    assert_eq!(state.find_member("codex-1").unwrap().kind(), mahoquot_gateway::account::ProviderKind::Antigravity);

    let res = app
        .clone()
        .oneshot(
            axum::http::Request::get("/v0/management/accounts/credits")
                .header("authorization", "Bearer test-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 100_000).await.unwrap()).unwrap();
    assert_eq!(body["ids"], json!([]));
    assert!(!state.settings.current().is_codex_account_credit_enabled("codex-1"));

    let res = app
        .clone()
        .oneshot(
            axum::http::Request::put("/v0/management/accounts/credits")
                .header("authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(Body::from(json!({
                    "id": "codex-1",
                    "credits_after_limit": true
                }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 100_000).await.unwrap()).unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["id"], "codex-1");
    assert_eq!(body["credits_after_limit"], true);
    assert_eq!(body["creditsAfterLimit"], true);
    assert!(state.settings.current().is_codex_account_credit_enabled("codex-1"));
    assert!(!state.settings.current().is_codex_account_credit_enabled("codex-2"));

    let saved_yaml = std::fs::read_to_string(&config_path).unwrap();
    assert!(saved_yaml.contains("credit-codex-account-ids:"), "persisted to yaml: {saved_yaml}");
    assert!(saved_yaml.contains("codex-1"), "persisted to yaml: {saved_yaml}");

    let res = app
        .clone()
        .oneshot(
            axum::http::Request::put("/api/codex-auth/accounts/credits")
                .header("authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(Body::from(json!({
                    "id": "codex-1",
                    "creditsAfterLimit": false
                }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(!state.settings.current().is_codex_account_credit_enabled("codex-1"));

    let res = app
        .clone()
        .oneshot(
            axum::http::Request::put("/v0/management/accounts/credits")
                .header("authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(Body::from(json!({
                    "id": "../traversal",
                    "credits_after_limit": true
                }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    let res = app
        .clone()
        .oneshot(
            axum::http::Request::put("/v0/management/accounts/credits")
                .header("authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(Body::from(json!({
                    "id": "claude-1",
                    "credits_after_limit": true
                }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    let res = app
        .clone()
        .oneshot(
            axum::http::Request::put("/v0/management/accounts/credits")
                .header("authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(Body::from(json!({
                    "id": "nonexistent-account",
                    "credits_after_limit": true
                }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    let res = app
        .clone()
        .oneshot(
            axum::http::Request::put("/v0/management/accounts/credits")
                .header("authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(Body::from(json!({
                    "all": true
                }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 100_000).await.unwrap()).unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["all"], true);
    assert!(state.settings.current().is_codex_account_credit_enabled("codex-1"));
    assert!(state.settings.current().is_codex_account_credit_enabled("codex-2"));
    assert!(!state.settings.current().is_codex_account_credit_enabled("claude-1"));
    assert!(!state.settings.current().is_codex_account_credit_enabled("codex-3-new"));

    let res = app
        .clone()
        .oneshot(
            axum::http::Request::put("/v0/management/accounts/credits")
                .header("authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(Body::from(json!({
                    "all": "non-bool"
                }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    let res = app
        .clone()
        .oneshot(
            axum::http::Request::put("/v0/management/accounts/credits")
                .header("authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(Body::from(json!({
                    "all": false
                }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 100_000).await.unwrap()).unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["all"], false);
    assert_eq!(body["ids"], json!([]));
    assert!(body["ids"].as_array().unwrap().is_empty());
    assert!(!state.settings.current().is_codex_account_credit_enabled("codex-1"));
    assert!(!state.settings.current().is_codex_account_credit_enabled("codex-2"));

    std::fs::remove_dir_all(temp_dir).ok();
}

#[tokio::test]
async fn exhausted_codex_account_with_credits_off_is_excluded_from_selection() {
    let requests_received = Arc::new(AtomicUsize::new(0));
    let requests_clone = Arc::clone(&requests_received);

    let listener = bind_fixture_listener().await;
    let port = listener.local_addr().unwrap().port();
    let upstream_task = tokio::spawn(async move {
        let app = Router::new().route(
            common::CODEX_PATH,
            post(move || {
                let count = Arc::clone(&requests_clone);
                async move {
                    count.fetch_add(1, Ordering::Relaxed);
                    (
                        StatusCode::OK,
                        [("Content-Type", "text/event-stream")],
                        common::codex_sse("ok"),
                    )
                }
            }),
        );
        let _ = axum::serve(listener, app).await;
    });

    let temp_dir = unique_temp_dir("codex-exhausted-off");
    let json_1 = create_auth_file_json(
        "codex-1",
        "acc_1",
        "tok_1",
        Some(&format!("http://127.0.0.1:{port}")),
    );
    std::fs::write(temp_dir.join("codex-1.json"), json_1).unwrap();
    let config_path = temp_dir.join("config.yaml");
    std::fs::write(&config_path, "logging-to-file: false\n").unwrap();

    let state = Arc::new(
        AppState::new(&GatewayConfig {
            auth_dir: temp_dir.clone(),
            config_path,
            api_keys: mahoquot_gateway::inbound::ApiKeys::from_env_value("test-key"),
            auth_refresh_enabled: false,
            strategy: Strategy::FillFirst,
            ..GatewayConfig::default()
        })
        .unwrap(),
    );

    let member = state.find_member("codex-1").expect("codex-1 in pool");
    let now = now_unix();
    let usage = AccountUsage {
        primary: QuotaWindow {
            used_percent: Some(100.0),
            reset_at_unix: Some(now + 3600),
            window_minutes: Some(300),
            ..Default::default()
        },
        credits_balance: Some(50.0),
        has_credits: Some(true),
        ..Default::default()
    };
    member.update_usage_from_headers(usage);

    let app = create_app(Arc::clone(&state));

    assert!(!state.settings.current().is_codex_account_credit_enabled("codex-1"));
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::post("/v1/chat/completions")
                .header("authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(Body::from(common::session_request("sess-1")))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(requests_received.load(Ordering::Relaxed), 0);
    assert_ne!(res.status(), StatusCode::OK);

    state
        .settings
        .mutate(|s| {
            s.set_codex_account_credit_use("codex-1".to_string(), true);
        })
        .unwrap();
    assert!(state.settings.current().is_codex_account_credit_enabled("codex-1"));

    let res = app
        .clone()
        .oneshot(
            axum::http::Request::post("/v1/chat/completions")
                .header("authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(Body::from(common::session_request("sess-2")))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(requests_received.load(Ordering::Relaxed), 1);

    upstream_task.abort();
    std::fs::remove_dir_all(temp_dir).ok();
}

#[tokio::test]
async fn authoritative_overage_limit_is_never_bypassed_by_credits_on() {
    let requests_received = Arc::new(AtomicUsize::new(0));
    let requests_clone = Arc::clone(&requests_received);

    let listener = bind_fixture_listener().await;
    let port = listener.local_addr().unwrap().port();
    let upstream_task = tokio::spawn(async move {
        let app = Router::new().route(
            common::CODEX_PATH,
            post(move || {
                let count = Arc::clone(&requests_clone);
                async move {
                    count.fetch_add(1, Ordering::Relaxed);
                    (
                        StatusCode::OK,
                        [("Content-Type", "text/event-stream")],
                        common::codex_sse("ok"),
                    )
                }
            }),
        );
        let _ = axum::serve(listener, app).await;
    });

    let temp_dir = unique_temp_dir("codex-hard-overage");
    let json_1 = create_auth_file_json(
        "codex-hard",
        "acc_hard",
        "tok_hard",
        Some(&format!("http://127.0.0.1:{port}")),
    );
    std::fs::write(temp_dir.join("codex-hard.json"), json_1).unwrap();
    let config_path = temp_dir.join("config.yaml");
    std::fs::write(&config_path, "logging-to-file: false\n").unwrap();

    let state = Arc::new(
        AppState::new(&GatewayConfig {
            auth_dir: temp_dir.clone(),
            config_path,
            api_keys: mahoquot_gateway::inbound::ApiKeys::from_env_value("test-key"),
            auth_refresh_enabled: false,
            strategy: Strategy::FillFirst,
            ..GatewayConfig::default()
        })
        .unwrap(),
    );

    let member = state.find_member("codex-hard").expect("codex-hard in pool");
    let now = now_unix();
    member.update_usage_from_headers(AccountUsage {
        primary: QuotaWindow {
            used_percent: Some(20.0),
            reset_at_unix: Some(now + 3600),
            window_minutes: Some(300),
            ..Default::default()
        },
        credits_balance: Some(0.0),
        has_credits: Some(true),
        overage_limit_reached: Some(true),
        ..Default::default()
    });

    state
        .settings
        .mutate(|s| {
            s.set_codex_account_credit_use("codex-hard".to_string(), true);
        })
        .unwrap();
    assert!(state.settings.current().is_codex_account_credit_enabled("codex-hard"));
    assert!(member.usage_snapshot().is_codex_hard_limit_reached());

    let app = create_app(Arc::clone(&state));

    let res = app
        .clone()
        .oneshot(
            axum::http::Request::post("/v1/chat/completions")
                .header("authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(Body::from(common::session_request("sess-hard")))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(requests_received.load(Ordering::Relaxed), 0);
    assert_ne!(res.status(), StatusCode::OK);

    upstream_task.abort();
    std::fs::remove_dir_all(temp_dir).ok();
}

#[tokio::test]
async fn model_matching_contract_isolates_exhausted_group_buckets() {
    let temp_dir = unique_temp_dir("codex-model-match");
    let json_1 = create_auth_file_json("codex-mm", "acc_mm", "tok_mm", None);
    std::fs::write(temp_dir.join("codex-mm.json"), json_1).unwrap();
    let config_path = temp_dir.join("config.yaml");
    std::fs::write(&config_path, "logging-to-file: false\n").unwrap();

    let state = Arc::new(
        AppState::new(&GatewayConfig {
            auth_dir: temp_dir.clone(),
            config_path,
            api_keys: mahoquot_gateway::inbound::ApiKeys::from_env_value("test-key"),
            auth_refresh_enabled: false,
            strategy: Strategy::FillFirst,
            ..GatewayConfig::default()
        })
        .unwrap(),
    );

    let member = state.find_member("codex-mm").expect("codex-mm in pool");
    let now = now_unix();
    member.update_usage_from_headers(AccountUsage {
        primary: QuotaWindow {
            used_percent: Some(10.0),
            reset_at_unix: Some(now + 3600),
            window_minutes: Some(300),
            ..Default::default()
        },
        groups: vec![QuotaGroup {
            display_name: Some("Spark".to_string()),
            models: Some("bengalfox".to_string()),
            buckets: vec![QuotaBucket {
                bucket_id: Some("bengalfox-spark".to_string()),
                used_percent: Some(100.0),
                reset_at_unix: Some(now + 3600),
                ..Default::default()
            }],
        }],
        credits_balance: Some(5.0),
        has_credits: Some(true),
        ..Default::default()
    });

    assert!(!state.settings.current().is_codex_account_credit_enabled("codex-mm"));
    assert!(member.usage_snapshot().is_codex_included_quota_exhausted(Some("bengalfox"), now));
    assert!(member.usage_snapshot().is_codex_included_quota_exhausted(Some("gpt-5.3-codex-spark"), now));
    assert!(!member.usage_snapshot().is_codex_included_quota_exhausted(Some("gpt-5.6-sol"), now));

    std::fs::remove_dir_all(temp_dir).ok();
}

#[tokio::test]
async fn failover_skips_exhausted_codex_account_when_credits_off() {
    let calls_a = Arc::new(AtomicUsize::new(0));
    let calls_b = Arc::new(AtomicUsize::new(0));

    let calls_a_clone = Arc::clone(&calls_a);
    let listener_a = bind_fixture_listener().await;
    let port_a = listener_a.local_addr().unwrap().port();
    let task_a = tokio::spawn(async move {
        let app = Router::new().route(
            common::CODEX_PATH,
            post(move || {
                let c = Arc::clone(&calls_a_clone);
                async move {
                    c.fetch_add(1, Ordering::Relaxed);
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            }),
        );
        let _ = axum::serve(listener_a, app).await;
    });

    let calls_b_clone = Arc::clone(&calls_b);
    let listener_b = bind_fixture_listener().await;
    let port_b = listener_b.local_addr().unwrap().port();
    let task_b = tokio::spawn(async move {
        let app = Router::new().route(
            common::CODEX_PATH,
            post(move || {
                let c = Arc::clone(&calls_b_clone);
                async move {
                    c.fetch_add(1, Ordering::Relaxed);
                    (
                        StatusCode::OK,
                        [("Content-Type", "text/event-stream")],
                        common::codex_sse("ok-from-b"),
                    )
                }
            }),
        );
        let _ = axum::serve(listener_b, app).await;
    });

    let temp_dir = unique_temp_dir("codex-failover-credits");
    let json_a = create_auth_file_json(
        "codex-a",
        "acc_a",
        "tok_a",
        Some(&format!("http://127.0.0.1:{port_a}")),
    );
    let json_b = create_auth_file_json(
        "codex-b",
        "acc_b",
        "tok_b",
        Some(&format!("http://127.0.0.1:{port_b}")),
    );
    std::fs::write(temp_dir.join("codex-a.json"), json_a).unwrap();
    std::fs::write(temp_dir.join("codex-b.json"), json_b).unwrap();
    let config_path = temp_dir.join("config.yaml");
    std::fs::write(&config_path, "logging-to-file: false\n").unwrap();

    let state = Arc::new(
        AppState::new(&GatewayConfig {
            auth_dir: temp_dir.clone(),
            config_path,
            api_keys: mahoquot_gateway::inbound::ApiKeys::from_env_value("test-key"),
            auth_refresh_enabled: false,
            strategy: Strategy::FillFirst,
            max_failover: 2,
            ..GatewayConfig::default()
        })
        .unwrap(),
    );

    let member_b = state.find_member("codex-b").expect("codex-b in pool");
    let now = now_unix();
    member_b.update_usage_from_headers(AccountUsage {
        primary: QuotaWindow {
            used_percent: Some(100.0),
            reset_at_unix: Some(now + 1800),
            window_minutes: Some(300),
            ..Default::default()
        },
        credits_balance: Some(25.0),
        has_credits: Some(true),
        ..Default::default()
    });

    let app = create_app(Arc::clone(&state));

    let _ = app
        .clone()
        .oneshot(
            axum::http::Request::post("/v1/chat/completions")
                .header("authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(Body::from(common::session_request("sess-fail-1")))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(calls_a.load(Ordering::Relaxed), 1);
    assert_eq!(calls_b.load(Ordering::Relaxed), 0);

    state
        .settings
        .mutate(|s| {
            s.set_codex_account_credit_use("codex-b".to_string(), true);
        })
        .unwrap();

    let res = app
        .clone()
        .oneshot(
            axum::http::Request::post("/v1/chat/completions")
                .header("authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(Body::from(common::session_request("sess-fail-2")))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(calls_a.load(Ordering::Relaxed), 2);
    assert_eq!(calls_b.load(Ordering::Relaxed), 1);

    task_a.abort();
    task_b.abort();
    std::fs::remove_dir_all(temp_dir).ok();
}
