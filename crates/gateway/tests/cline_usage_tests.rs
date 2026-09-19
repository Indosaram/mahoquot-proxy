//! Cline daily-budget tracking: served tokens accumulate per account, the
//! quota bucket surfaces live usage with the estimated 24h reset, and the
//! account is benched proactively at the threshold so the upstream cap 429
//! never reaches a client.

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
async fn cline_daily_budget_benches_exhausted_account_proactively() {
    let mut servers = Vec::new();
    let mut shutdowns = Vec::new();
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    // Server A: serves 8M tokens per request — two requests cross the 95%
    // proactive-bench threshold of the 15.2M daily budget.
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

    // Request 1: account A serves 8M tokens (52.6% of budget) — no bench yet.
    let res = client
        .post(&gw_url)
        .header("Content-Type", "application/json")
        .json(&req)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // Request 2: A serves another 8M — 16M crosses the threshold. The record
    // is finalized synchronously, so the bench is visible immediately.
    let res = client
        .post(&gw_url)
        .header("Content-Type", "application/json")
        .json(&req)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let member_a = state.find_member("generic-cline-a").expect("member a");
    let model = "z-ai/glm-5.3-flash";
    assert!(
        !member_a.group_available(model, now_unix),
        "A must be benched for the model after crossing the budget threshold"
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
        (used_percent - 100.0).abs() < 0.01,
        "16M of 15.2M must clamp to 100%, got {used_percent}"
    );
    let reset_at = bucket.reset_at_unix.expect("estimated reset is reported");
    assert!(
        reset_at > now_unix + 86_399 && reset_at <= now_unix + 86_401,
        "reset must estimate window start + 24h, got {reset_at} vs now {now_unix}"
    );

    // Request 3: the benched account must be skipped without any upstream
    // 429 — B serves the request directly.
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
        2,
        "A serves exactly the two pre-threshold requests"
    );
    assert_eq!(hits_b.load(Ordering::SeqCst), 1, "B serves the third");

    for shutdown in shutdowns {
        let _ = shutdown.send(());
    }
    for server in servers {
        let _ = server.await;
    }
    std::fs::remove_dir_all(&temp_dir).unwrap();
}
