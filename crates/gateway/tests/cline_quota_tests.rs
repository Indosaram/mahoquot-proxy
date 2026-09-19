mod common;

use std::sync::Arc;
use axum::http::StatusCode;
use axum::routing::post;
use axum::Router;
use common::unique_temp_dir;
use mahoquot_gateway::{
    account::{model_quota_group, AccountMember, GenericAccount, ProviderAccount},
    config::GatewayConfig,
    metrics::HealthStats,
    routes::create_app,
    state::AppState,
    usage::{AccountUsage, QuotaBucket, QuotaGroup, UsageStateStore},
};
use mahoquot_types::{Health, PoolMember, Strategy};

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

fn create_cline_account(id: &str, base_url: &str, models: Vec<String>) -> AccountMember {
    let generic = GenericAccount {
        identity_slug: id.to_string(),
        provider: "cline".to_string(),
        label: format!("Cline {id}"),
        email: format!("{id}@cline.example"),
        adapter: "openai".to_string(),
        base_url: base_url.to_string(),
        api_key: "dummy-key".to_string(),
        auth_mode: "bearer".to_string(),
        refresh_token: String::new(),
        expired: String::new(),
        token_url: String::new(),
        client_id: String::new(),
        project_id: String::new(),
        models,
        static_headers: Default::default(),
        disabled: false,
    };
    AccountMember::for_test(ProviderAccount::Generic(generic))
}

#[test]
fn test_cline_model_quota_group_isolation_and_antigravity_policy() {
    // 1. Antigravity policy: gemini vs 3p
    assert_eq!(
        model_quota_group("antigravity", "gemini-3.8-flash-high"),
        Some("gemini")
    );
    assert_eq!(
        model_quota_group("antigravity", "claude-opus-4-6-thinking"),
        Some("3p")
    );
    assert_eq!(
        model_quota_group("antigravity", "gpt-oss-120b-medium"),
        Some("3p")
    );

    // 2. Single-pool providers have no group split
    assert_eq!(model_quota_group("codex", "gpt-5.6-sol"), None);
    assert_eq!(model_quota_group("claude", "claude-sonnet-4-6"), None);

    // 3. Cline isolates per affected model
    assert_eq!(
        model_quota_group("cline", "z-ai/glm-5.3-flash"),
        Some("z-ai/glm-5.3-flash")
    );
    assert_eq!(
        model_quota_group("cline", "claude-3-7-sonnet"),
        Some("claude-3-7-sonnet")
    );

    // 4. Exercise on AccountMember
    let cline_member = create_cline_account(
        "cline-1",
        "http://127.0.0.1:18899",
        vec!["z-ai/glm-5.3-flash".to_string(), "claude-3-7-sonnet".to_string()],
    );

    let now_ms = 1_000_000;
    let until_ms = now_ms + 3600_000;

    assert!(cline_member.group_available("z-ai/glm-5.3-flash", now_ms));
    assert!(cline_member.group_available("claude-3-7-sonnet", now_ms));

    // Bench only z-ai/glm-5.3-flash
    assert!(cline_member.set_group_cooldown("z-ai/glm-5.3-flash", until_ms));

    // Affected model is unavailable until deadline
    assert!(!cline_member.group_available("z-ai/glm-5.3-flash", now_ms));
    assert!(cline_member.group_available("z-ai/glm-5.3-flash", until_ms));

    // Unrelated model on the same Cline account remains available and routable
    assert!(cline_member.group_available("claude-3-7-sonnet", now_ms));
    assert_eq!(cline_member.health(), Health::Available);
}

#[test]
fn test_cline_record_ok_preserves_future_cooldown_against_inflight_success() {
    let cline_member = create_cline_account(
        "cline-ok-test",
        "http://127.0.0.1:18899",
        vec!["model-a".to_string()],
    );

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    // Simulate an active future cooldown from a recent 429
    let future_cooldown_ms = now_ms + 600_000;
    cline_member.set_health(Health::Cooldown {
        until_unix_ms: future_cooldown_ms,
    });

    // In-flight success finishes and records ok
    cline_member.record_ok();

    // The future cooldown MUST be preserved against older in-flight success!
    assert_eq!(
        cline_member.health(),
        Health::Cooldown {
            until_unix_ms: future_cooldown_ms
        },
        "older in-flight success must not clear active future cooldown"
    );

    // Now simulate an expired cooldown in the past
    let past_cooldown_ms = now_ms - 10_000;
    cline_member.set_health(Health::Cooldown {
        until_unix_ms: past_cooldown_ms,
    });

    // When success arrives after the deadline, it safely transitions to Available
    cline_member.record_ok();
    assert_eq!(
        cline_member.health(),
        Health::Available,
        "record_ok should normalize expired cooldown to Available"
    );
}

#[test]
fn test_cline_stats_normalizes_expired_health_and_preserves_future_and_disabled() {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    let temp_dir = unique_temp_dir("qgw-cline-stats-test");

    // Account 1: expired cooldown
    let acct_expired = r#"{
        "provider": "cline",
        "adapter": "openai",
        "base_url": "http://127.0.0.1:18899",
        "api_key": "dummy",
        "models": ["m1"]
    }"#;
    std::fs::write(temp_dir.join("generic-cline-1.json"), acct_expired).unwrap();

    // Account 2: future cooldown
    let acct_future = r#"{
        "provider": "cline",
        "adapter": "openai",
        "base_url": "http://127.0.0.1:18899",
        "api_key": "dummy",
        "models": ["m2"]
    }"#;
    std::fs::write(temp_dir.join("generic-cline-2.json"), acct_future).unwrap();

    // Account 3: disabled
    let acct_disabled = r#"{
        "provider": "cline",
        "adapter": "openai",
        "base_url": "http://127.0.0.1:18899",
        "api_key": "dummy",
        "models": ["m3"],
        "disabled": true
    }"#;
    std::fs::write(temp_dir.join("generic-cline-3.json"), acct_disabled).unwrap();

    let config = GatewayConfig {
        usage_poll_secs: 120,
        port: 0,
        auth_dir: temp_dir.clone(),
        config_path: temp_dir.join("config.yaml"),
        strategy: Strategy::FillFirst,
        max_failover: 3,
        log_level: "info".to_string(),
        api_keys: mahoquot_gateway::inbound::ApiKeys::default(),
        models_env: None,
        refresh_url: mahoquot_providers::refresh::REFRESH_TOKEN_URL.to_string(),
        auth_refresh_enabled: false,
        ..Default::default()
    };

    let state = AppState::new(&config).unwrap();
    let m1 = state.find_member("generic-cline-1").unwrap();
    let m2 = state.find_member("generic-cline-2").unwrap();

    // Set m1 to past cooldown
    m1.set_health(Health::Cooldown {
        until_unix_ms: now_ms - 5000,
    });
    // Set m2 to future cooldown
    m2.set_health(Health::Cooldown {
        until_unix_ms: now_ms + 500_000,
    });

    let stats = state.get_stats();

    let s1 = stats.accounts.iter().find(|a| a.id == "generic-cline-1").unwrap();
    let s2 = stats.accounts.iter().find(|a| a.id == "generic-cline-2").unwrap();
    let s3 = stats.accounts.iter().find(|a| a.id == "generic-cline-3").unwrap();

    // m1: expired cooldown must normalize to Available with reset_at_unix_ms: None
    assert!(
        matches!(s1.health, HealthStats::Available),
        "expired cooldown must report Available, got {:?}",
        s1.health
    );
    assert_eq!(s1.reset_at_unix_ms, None);

    // m2: future cooldown preserved
    assert!(
        matches!(s2.health, HealthStats::Cooldown { until_unix_ms } if until_unix_ms == now_ms + 500_000),
        "future cooldown must be preserved, got {:?}",
        s2.health
    );
    assert_eq!(s2.reset_at_unix_ms, Some(now_ms + 500_000));

    // m3: disabled preserved
    assert!(
        matches!(s3.health, HealthStats::Disabled),
        "disabled status must be preserved, got {:?}",
        s3.health
    );

    std::fs::remove_dir_all(&temp_dir).unwrap();
}

#[test]
fn test_cline_stale_usage_expires_and_drops_non_display_models() {
    let cline_member = create_cline_account(
        "cline-usage-test",
        "http://127.0.0.1:18899",
        vec!["z-ai/glm-5.3-flash".to_string(), "z-ai/glm-5.3".to_string()],
    );

    let now_unix = 1_700_000_000;
    let past_reset = now_unix - 3600;
    let future_reset = now_unix + 7200;

    let mut usage = AccountUsage {
        groups: vec![QuotaGroup {
            display_name: Some("Cline Free Limits".to_string()),
            models: Some("Cline Free Models".to_string()),
            buckets: vec![
                QuotaBucket {
                    bucket_id: Some("z-ai/glm-5.3-flash".to_string()),
                    display_name: Some("z-ai/glm-5.3-flash (Daily limit)".to_string()),
                    window: Some("Daily".to_string()),
                    used_percent: Some(100.0),
                    reset_at_unix: Some(past_reset),
                },
                QuotaBucket {
                    bucket_id: Some("z-ai/glm-5.3".to_string()),
                    display_name: Some("z-ai/glm-5.3 (Daily limit)".to_string()),
                    window: Some("Daily".to_string()),
                    used_percent: Some(100.0),
                    reset_at_unix: Some(future_reset),
                },
            ],
        }],
        observed_at_unix: Some(now_unix - 100),
        ..Default::default()
    };

    cline_member.set_usage(usage.clone());

    // Expire stale limits
    usage.expire_stale_cline_limits(now_unix);

    let group = &usage.groups[0];
    // Non-display models are dropped entirely, even while unexpired
    assert_eq!(
        group.buckets.iter().find(|b| b.bucket_id.as_deref() == Some("z-ai/glm-5.3")),
        None,
        "non-display model buckets must never render"
    );
    let b_display = group
        .buckets
        .iter()
        .find(|b| b.bucket_id.as_deref() == Some("z-ai/glm-5.3-flash"))
        .expect("display model bucket is kept");

    // Expired bucket must become unknown (used_percent: None), NOT fabricated zero
    assert_eq!(
        b_display.used_percent, None,
        "expired Cline usage must be unknown (None), never fabricated 0"
    );
    assert_eq!(b_display.reset_at_unix, None);
}

#[test]
fn test_non_cline_provider_with_cline_label_quota_preserved() {
    let generic_non_cline = GenericAccount {
        identity_slug: "custom-1".to_string(),
        provider: "custom-provider".to_string(),
        label: "Custom Provider".to_string(),
        email: "custom@example.com".to_string(),
        adapter: "openai-chat".to_string(),
        base_url: "http://127.0.0.1:18899".to_string(),
        api_key: "dummy-key".to_string(),
        auth_mode: "bearer".to_string(),
        refresh_token: String::new(),
        expired: String::new(),
        token_url: String::new(),
        client_id: String::new(),
        project_id: String::new(),
        models: vec!["custom-model".to_string()],
        static_headers: Default::default(),
        disabled: false,
    };
    let non_cline_member = AccountMember::for_test(ProviderAccount::Generic(generic_non_cline));

    let now_unix = 1_700_000_000;
    let past_reset = now_unix - 3600;

    let usage = AccountUsage {
        groups: vec![QuotaGroup {
            display_name: Some("Cline Free Limits".to_string()),
            models: Some("Cline Free Models".to_string()),
            buckets: vec![QuotaBucket {
                bucket_id: Some("custom-model".to_string()),
                display_name: Some("custom-model (Daily limit)".to_string()),
                window: Some("Daily".to_string()),
                used_percent: Some(100.0),
                reset_at_unix: Some(past_reset),
            }],
        }],
        observed_at_unix: Some(now_unix - 100),
        ..Default::default()
    };

    non_cline_member.set_usage(usage);

    // Because this member's provider is NOT "cline", its usage MUST NOT be mutated or expired
    let snapshot = non_cline_member.usage_snapshot();
    let b = &snapshot.groups[0].buckets[0];
    assert_eq!(
        b.used_percent,
        Some(100.0),
        "non-Cline provider with same group label must remain intact, never normalized"
    );
    assert_eq!(b.reset_at_unix, Some(past_reset));
}

#[test]
fn test_cline_persisted_old_quota_expires_on_restore() {
    let temp_dir = unique_temp_dir("qgw-cline-restore-test");
    let store_path = temp_dir.join("usage-state.json");
    let store = UsageStateStore::load(store_path.clone());

    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let mut accounts = std::collections::BTreeMap::new();
    accounts.insert(
        "cline-saved".to_string(),
        AccountUsage {
            groups: vec![QuotaGroup {
                display_name: Some("Cline Free Limits".to_string()),
                models: Some("Cline Free Models".to_string()),
                buckets: vec![QuotaBucket {
                    bucket_id: Some("z-ai/glm-5.3-flash".to_string()),
                    display_name: Some("z-ai/glm-5.3-flash (Daily limit)".to_string()),
                    window: Some("Daily".to_string()),
                    used_percent: Some(100.0),
                    // Reset was in the past (e.g. 1 hour ago)
                    reset_at_unix: Some(now_unix - 3600),
                }],
            }],
            observed_at_unix: Some(now_unix - 7200),
            ..Default::default()
        },
    );

    store.save(&accounts, now_unix - 7200);

    // Restore snapshots: store restore retains raw usage without provider metadata
    let restored = store.restore();
    let restored_usage = restored.get("cline-saved").expect("account restored");
    let raw_bucket = &restored_usage.groups[0].buckets[0];
    assert_eq!(
        raw_bucket.used_percent,
        Some(100.0),
        "store restore preserves raw usage without provider metadata"
    );

    // Attached to a confirmed Cline AccountMember, usage_snapshot normalizes stale quota to unknown
    let cline_member = create_cline_account(
        "cline-saved",
        "http://127.0.0.1:18899",
        vec!["z-ai/glm-5.3-flash".to_string()],
    );
    cline_member.set_usage(restored_usage.clone());
    let snapshot = cline_member.usage_snapshot();
    let normalized_bucket = &snapshot.groups[0].buckets[0];
    assert_eq!(
        normalized_bucket.used_percent, None,
        "confirmed Cline account normalizes stale restored snapshot to unknown (None)"
    );

    std::fs::remove_dir_all(&temp_dir).unwrap();
}

#[tokio::test]
async fn test_cline_request_path_model_isolation_and_rotation() {
    let mut servers = Vec::new();
    let mut shutdowns = Vec::new();

    // Cline server 1: returns 429 daily free limit for z-ai/glm-5.3-flash, 200 for other-model
    let listener_1 = bind_fixture_listener().await;
    let port_1 = listener_1.local_addr().unwrap().port();
    let app_1 = Router::new().route(
        "/v1/chat/completions",
        post(|body: axum::body::Bytes| async move {
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let model = json.get("model").and_then(|m| m.as_str()).unwrap_or("");
            if model == "z-ai/glm-5.3-flash" {
                let cap_body = "{\"error\":{\"code\":\"INFERENCE_CAP_ERROR\",\"message\":\"Error 429: Daily free limit reached on model z-ai/glm-5.3-flash. Try again in 8h 48m\"}}";
                (StatusCode::TOO_MANY_REQUESTS, [("content-type", "application/json")], cap_body)
            } else {
                let ok_body = "{\"id\":\"chatcmpl-1\",\"object\":\"chat.completion\",\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"hello from server 1\"}}]}";
                (StatusCode::OK, [("content-type", "application/json")], ok_body)
            }
        }),
    );
    let (shutdown_1, stopped_1) = tokio::sync::oneshot::channel();
    shutdowns.push(shutdown_1);
    servers.push(tokio::spawn(async move {
        axum::serve(listener_1, app_1)
            .with_graceful_shutdown(async {
                stopped_1.await.unwrap();
            })
            .await
            .unwrap();
    }));

    // Cline server 2: returns 200 for z-ai/glm-5.3-flash (failover target)
    let listener_2 = bind_fixture_listener().await;
    let port_2 = listener_2.local_addr().unwrap().port();
    let app_2 = Router::new().route(
        "/v1/chat/completions",
        post(|_: axum::body::Bytes| async move {
            let ok_body = "{\"id\":\"chatcmpl-2\",\"object\":\"chat.completion\",\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"hello from server 2\"}}]}";
            (StatusCode::OK, [("content-type", "application/json")], ok_body)
        }),
    );
    let (shutdown_2, stopped_2) = tokio::sync::oneshot::channel();
    shutdowns.push(shutdown_2);
    servers.push(tokio::spawn(async move {
        axum::serve(listener_2, app_2)
            .with_graceful_shutdown(async {
                stopped_2.await.unwrap();
            })
            .await
            .unwrap();
    }));

    let temp_dir = unique_temp_dir("qgw-cline-req-test");

    // Account 1: Cline account on port_1
    let acct_1 = format!(
        r#"{{
            "type": "generic",
            "provider": "cline",
            "adapter": "openai-chat",
            "base_url": "http://127.0.0.1:{port_1}",
            "api_key": "dummy",
            "models": ["z-ai/glm-5.3-flash", "other-model"]
        }}"#
    );
    std::fs::write(temp_dir.join("generic-cline-a.json"), acct_1).unwrap();

    // Account 2: Cline account on port_2
    let acct_2 = format!(
        r#"{{
            "type": "generic",
            "provider": "cline",
            "adapter": "openai-chat",
            "base_url": "http://127.0.0.1:{port_2}",
            "api_key": "dummy",
            "models": ["z-ai/glm-5.3-flash", "other-model"]
        }}"#
    );
    std::fs::write(temp_dir.join("generic-cline-b.json"), acct_2).unwrap();

    let config = GatewayConfig {
        usage_poll_secs: 120,
        port: 0,
        auth_dir: temp_dir.clone(),
        config_path: temp_dir.join("config.yaml"),
        strategy: Strategy::FillFirst,
        max_failover: 3,
        log_level: "info".to_string(),
        api_keys: mahoquot_gateway::inbound::ApiKeys::default(),
        models_env: None,
        refresh_url: mahoquot_providers::refresh::REFRESH_TOKEN_URL.to_string(),
        auth_refresh_enabled: false,
        ..Default::default()
    };

    let state = Arc::new(AppState::new(&config).unwrap());
    let app = create_app(state.clone());
    let gw_listener = bind_fixture_listener().await;
    let gw_port = gw_listener.local_addr().unwrap().port();
    let (gw_shutdown, gw_stopped) = tokio::sync::oneshot::channel();
    shutdowns.push(gw_shutdown);
    servers.push(tokio::spawn(async move {
        axum::serve(gw_listener, app)
            .with_graceful_shutdown(async {
                gw_stopped.await.unwrap();
            })
            .await
            .unwrap();
    }));

    let client = reqwest::Client::new();
    let gw_url = format!("http://127.0.0.1:{gw_port}/v1/chat/completions");

    // Request 1: z-ai/glm-5.3-flash.
    // Server 1 returns 429 daily free limit.
    // Proxy must failover to Server 2 and return 200.
    let req_glm = serde_json::json!({
        "model": "z-ai/glm-5.3-flash",
        "messages": [{"role": "user", "content": "hello"}],
        "stream": false
    });
    let res = client
        .post(&gw_url)
        .header("Content-Type", "application/json")
        .json(&req_glm)
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "hello from server 2"
    );

    // Account 1 must NOT be benched account-wide; only z-ai/glm-5.3-flash is benched!
    let m1 = state.find_member("generic-cline-a").unwrap();
    assert_eq!(
        m1.health(),
        Health::Available,
        "Account 1 should remain Available, not benched account-wide"
    );

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert!(
        !m1.group_available("z-ai/glm-5.3-flash", now_ms),
        "z-ai/glm-5.3-flash must be benched on Account 1"
    );
    assert!(
        m1.group_available("other-model", now_ms),
        "other-model must remain available on Account 1"
    );

    // Request 2: other-model.
    // With FillFirst strategy, Account 1 is first. Since other-model is NOT benched,
    // it must route directly to Server 1 and return 200!
    let req_other = serde_json::json!({
        "model": "other-model",
        "messages": [{"role": "user", "content": "hello other"}],
        "stream": false
    });
    let res2 = client
        .post(&gw_url)
        .header("Content-Type", "application/json")
        .json(&req_other)
        .send()
        .await
        .unwrap();

    assert_eq!(res2.status(), reqwest::StatusCode::OK);
    let body2: serde_json::Value = res2.json().await.unwrap();
    assert_eq!(
        body2["choices"][0]["message"]["content"],
        "hello from server 1",
        "unrelated model on Account 1 must route to Server 1 successfully"
    );

    for s in shutdowns {
        let _ = s.send(());
    }
    for server in servers {
        let _ = server.await;
    }
    std::fs::remove_dir_all(&temp_dir).unwrap();
}

#[tokio::test]
async fn test_cline_malformed_body_safely_retains_header_fallback() {
    let mut servers = Vec::new();
    let mut shutdowns = Vec::new();

    // Cline server returns 429 with malformed JSON and Retry-After: 45
    let listener = bind_fixture_listener().await;
    let port = listener.local_addr().unwrap().port();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|_: axum::body::Bytes| async move {
            (
                StatusCode::TOO_MANY_REQUESTS,
                [("Retry-After", "45"), ("content-type", "application/json")],
                "invalid json body {",
            )
        }),
    );
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    shutdowns.push(shutdown);
    servers.push(tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                stopped.await.unwrap();
            })
            .await
            .unwrap();
    }));

    let temp_dir = unique_temp_dir("qgw-cline-malformed-test");
    let acct = format!(
        r#"{{
            "type": "generic",
            "provider": "cline",
            "adapter": "openai-chat",
            "base_url": "http://127.0.0.1:{port}",
            "api_key": "dummy",
            "models": ["z-ai/glm-5.3-flash"]
        }}"#
    );
    std::fs::write(temp_dir.join("generic-cline-malformed.json"), acct).unwrap();

    let config = GatewayConfig {
        usage_poll_secs: 120,
        port: 0,
        auth_dir: temp_dir.clone(),
        config_path: temp_dir.join("config.yaml"),
        strategy: Strategy::FillFirst,
        max_failover: 1,
        log_level: "info".to_string(),
        api_keys: mahoquot_gateway::inbound::ApiKeys::default(),
        models_env: None,
        refresh_url: mahoquot_providers::refresh::REFRESH_TOKEN_URL.to_string(),
        auth_refresh_enabled: false,
        ..Default::default()
    };

    let state = Arc::new(AppState::new(&config).unwrap());
    let app = create_app(state.clone());
    let gw_listener = bind_fixture_listener().await;
    let gw_port = gw_listener.local_addr().unwrap().port();
    let (gw_shutdown, gw_stopped) = tokio::sync::oneshot::channel();
    shutdowns.push(gw_shutdown);
    servers.push(tokio::spawn(async move {
        axum::serve(gw_listener, app)
            .with_graceful_shutdown(async {
                gw_stopped.await.unwrap();
            })
            .await
            .unwrap();
    }));

    let client = reqwest::Client::new();
    let gw_url = format!("http://127.0.0.1:{gw_port}/v1/chat/completions");

    let req = serde_json::json!({
        "model": "z-ai/glm-5.3-flash",
        "messages": [{"role": "user", "content": "hello"}],
        "stream": false
    });
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let res = client
        .post(&gw_url)
        .header("Content-Type", "application/json")
        .json(&req)
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);

    let m = state.find_member("generic-cline-malformed").unwrap();
    // Since body was malformed, it safely fell back to header Retry-After: 45s on the group/model
    // Cooldown deadline should be approximately now_ms + 45_000ms
    assert!(!m.group_available("z-ai/glm-5.3-flash", now_ms));
    assert!(m.group_available("z-ai/glm-5.3-flash", now_ms + 46_000));

    for s in shutdowns {
        let _ = s.send(());
    }
    for server in servers {
        let _ = server.await;
    }
    std::fs::remove_dir_all(&temp_dir).unwrap();
}
