//! Cline daily-budget tracking: served tokens accumulate per account and the
//! quota bucket surfaces the running estimate with the estimated 24h reset.
//! The estimate is display-only — it never withholds an account from routing,
//! because only the upstream cap 429 knows the real allowance.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use axum::http::StatusCode;
use axum::routing::post;
use axum::Router;
use common::unique_temp_dir;
use mahoquot_gateway::{config::GatewayConfig, routes::create_app, state::AppState};
use mahoquot_types::Strategy;

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

fn usage_body(total_tokens: u64) -> String {
    format!(
        r#"{{"id":"chatcmpl-1","object":"chat.completion","choices":[{{"index":0,"message":{{"role":"assistant","content":"ok"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":{total_tokens},"completion_tokens":0,"total_tokens":{total_tokens}}}}}"#
    )
}

fn cline_account(port: u16, models: &str) -> String {
    format!(
        r#"{{
            "type": "generic",
            "provider": "cline",
            "adapter": "openai-chat",
            "base_url": "http://127.0.0.1:{port}",
            "api_key": "dummy",
            "models": {models}
        }}"#
    )
}

#[tokio::test]
async fn cline_daily_budget_reports_usage_without_capping_the_account() {
    let mut servers = Vec::new();
    let mut shutdowns = Vec::new();
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    // Server A: serves 8M tokens per request — two requests put the account
    // past the 15.2M daily budget estimate. Upstream keeps answering 200, so
    // the gateway must keep routing to it.
    let listener_a = bind_fixture_listener().await;
    let port_a = listener_a.local_addr().unwrap().port();
    let hits_a = Arc::new(AtomicUsize::new(0));
    let app_a = {
        let hits = hits_a.clone();
        Router::new().route(
            "/v1/chat/completions",
            post(move |_: axum::body::Bytes| {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        [("content-type", "application/json")],
                        usage_body(8_000_000),
                    )
                }
            }),
        )
    };
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    shutdowns.push(shutdown);
    servers.push(tokio::spawn(async move {
        axum::serve(listener_a, app_a)
            .with_graceful_shutdown(async {
                stopped.await.unwrap();
            })
            .await
            .unwrap();
    }));

    // Server B: healthy failover target with negligible usage.
    let listener_b = bind_fixture_listener().await;
    let port_b = listener_b.local_addr().unwrap().port();
    let hits_b = Arc::new(AtomicUsize::new(0));
    let app_b = {
        let hits = hits_b.clone();
        Router::new().route(
            "/v1/chat/completions",
            post(move |_: axum::body::Bytes| {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        [("content-type", "application/json")],
                        usage_body(1_000),
                    )
                }
            }),
        )
    };
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    shutdowns.push(shutdown);
    servers.push(tokio::spawn(async move {
        axum::serve(listener_b, app_b)
            .with_graceful_shutdown(async {
                stopped.await.unwrap();
            })
            .await
            .unwrap();
    }));

    let temp_dir = unique_temp_dir("qgw-cline-usage-test");
    std::fs::write(
        temp_dir.join("generic-cline-a.json"),
        cline_account(port_a, "[\"z-ai/glm-5.3-flash\"]"),
    )
    .unwrap();
    std::fs::write(
        temp_dir.join("generic-cline-b.json"),
        cline_account(port_b, "[\"z-ai/glm-5.3-flash\"]"),
    )
    .unwrap();

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
    let req = serde_json::json!({
        "model": "z-ai/glm-5.3-flash",
        "messages": [{"role": "user", "content": "hello"}],
        "stream": false
    });

    // Request 1: account A serves 8M tokens (52.6% of the budget estimate).
    let res = client
        .post(&gw_url)
        .header("Content-Type", "application/json")
        .json(&req)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // Request 2: A serves another 8M — 16M is past the whole budget estimate.
    // The record is finalized synchronously, so usage is visible immediately.
    let res = client
        .post(&gw_url)
        .header("Content-Type", "application/json")
        .json(&req)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // The usage window opened when the first record landed — somewhere between
    // test start (`now_unix`) and this response returning. Pin the estimated
    // reset to that range: a fixed tolerance around `now_unix` would fail on a
    // loaded machine whenever fixture setup took longer than the slack.
    let requests_done_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let member_a = state.find_member("generic-cline-a").expect("member a");
    let model = "z-ai/glm-5.3-flash";
    assert!(
        member_a.group_available(model, now_unix),
        "an over-budget estimate must not bench the account: only upstream caps"
    );
    let usage = member_a.usage_snapshot();
    let group = usage
        .groups
        .iter()
        .find(|g| g.display_name.as_deref() == Some("Cline Free Limits"))
        .expect("cline free-limits group");
    let bucket = group
        .buckets
        .iter()
        .find(|b| b.bucket_id.as_deref() == Some(model))
        .expect("model bucket");
    let used_percent = bucket.used_percent.expect("live usage is reported");
    assert!(
        used_percent > 99.0 && used_percent < 100.0,
        "an over-budget estimate reports just under 100%: 100% is reserved for \
         an upstream-confirmed cap, got {used_percent}"
    );
    let reset_at = bucket.reset_at_unix.expect("estimated reset is reported");
    assert!(
        reset_at >= now_unix + 86_400 && reset_at <= requests_done_unix + 86_400,
        "reset must estimate window start + 24h within [test start, requests done] \
         + 24h, got {reset_at} vs now {now_unix}..{requests_done_unix}"
    );

    // Request 3: A is still the FillFirst head and still routable, so it keeps
    // serving. The estimate reports exhaustion; it does not enforce it.
    let res = client
        .post(&gw_url)
        .header("Content-Type", "application/json")
        .json(&req)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        hits_a.load(Ordering::SeqCst),
        3,
        "A keeps serving past its budget estimate"
    );
    assert_eq!(
        hits_b.load(Ordering::SeqCst),
        0,
        "B is never needed while upstream still accepts A"
    );

    for shutdown in shutdowns {
        let _ = shutdown.send(());
    }
    for server in servers {
        let _ = server.await;
    }
    std::fs::remove_dir_all(&temp_dir).unwrap();
}

/// The display models (glm and deepseek) keep independent "(Daily limit)"
/// quota buckets backed by separate token trackers. A served request on any
/// other pooled Cline model never creates a bucket, and its tokens move
/// neither display model's estimate.
#[tokio::test]
async fn cline_quota_bucket_surfaces_only_the_display_model() {
    let mut servers = Vec::new();
    let mut shutdowns = Vec::new();

    let listener = bind_fixture_listener().await;
    let port = listener.local_addr().unwrap().port();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|body: axum::body::Bytes| async move {
            let model = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| {
                    value
                        .get("model")
                        .and_then(|model| model.as_str())
                        .map(ToString::to_string)
                })
                .unwrap_or_default();
            let tokens = match model.as_str() {
                "z-ai/glm-5.3-flash" => 6_000_000,
                "z-ai/glm-4.7" => 7_000_000,
                _ => 2_000_000,
            };
            (
                StatusCode::OK,
                [("content-type", "application/json")],
                usage_body(tokens),
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

    let temp_dir = unique_temp_dir("qgw-cline-quota-display-test");
    std::fs::write(
        temp_dir.join("generic-cline-a.json"),
        cline_account(
            port,
            "[\"z-ai/glm-5.3-flash\",\"z-ai/glm-4.7\",\"cline-free/deepseek-v4.1-flash\"]",
        ),
    )
    .unwrap();

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
    for model in [
        "z-ai/glm-5.3-flash",
        "z-ai/glm-4.7",
        "cline-free/deepseek-v4.1-flash",
    ] {
        let res = client
            .post(&gw_url)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "hello"}],
                "stream": false
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "model {model} must serve");
    }

    let member = state.find_member("generic-cline-a").expect("member a");
    let usage = member.usage_snapshot();
    let groups = usage
        .groups
        .iter()
        .filter(|g| g.display_name.as_deref() == Some("Cline Free Limits"))
        .count();
    assert_eq!(groups, 1, "exactly one cline free-limits group");
    let group = usage
        .groups
        .iter()
        .find(|g| g.display_name.as_deref() == Some("Cline Free Limits"))
        .unwrap();
    let ids: Vec<_> = group
        .buckets
        .iter()
        .filter_map(|b| b.bucket_id.as_deref())
        .collect();
    assert_eq!(
        ids,
        vec!["z-ai/glm-5.3-flash", "cline-free/deepseek-v4.1-flash"],
        "glm and deepseek render buckets; every other model stays hidden"
    );
    let bucket = &group.buckets[0];
    assert_eq!(bucket.display_name.as_deref(), Some("z-ai/glm-5.3-flash (Daily limit)"));
    let glm_used = bucket.used_percent.expect("live usage is reported");
    let deepseek = &group.buckets[1];
    assert_eq!(
        deepseek.display_name.as_deref(),
        Some("cline-free/deepseek-v4.1-flash (Daily limit)")
    );
    let ds_used = deepseek.used_percent.expect("deepseek usage is reported");
    let glm_expect = 6_000_000.0 * 100.0 / 15_200_000.0;
    let ds_expect = 2_000_000.0 * 100.0 / 15_200_000.0;
    assert!(
        (glm_used - glm_expect).abs() < 1e-9,
        "glm lane counts only glm-served tokens: got {glm_used}, want {glm_expect}"
    );
    assert!(
        (ds_used - ds_expect).abs() < 1e-9,
        "deepseek lane counts only deepseek-served tokens: got {ds_used}, want {ds_expect}"
    );
    assert!(
        glm_used > ds_used,
        "lanes are independent: {glm_used} must exceed {ds_used}"
    );

    for shutdown in shutdowns {
        let _ = shutdown.send(());
    }
    for server in servers {
        let _ = server.await;
    }
    std::fs::remove_dir_all(&temp_dir).unwrap();
}
