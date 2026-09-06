mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use axum::routing::post;
use axum::Router;
use common::{create_auth_file_json, unique_temp_dir};
use mahoquot_gateway::{config::GatewayConfig, routes::create_app, state::AppState};
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

const NOW_MS: i64 = 1_800_000_000_123;

fn assert_cooldown(headers: &[(&str, &str)], seconds: i64) {
    let mut map = reqwest::header::HeaderMap::new();
    for (name, value) in headers {
        map.insert(
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    assert_eq!(
        mahoquot_gateway::relay::cooldown_deadline_from_headers(&map, NOW_MS),
        NOW_MS + seconds * 1000,
        "headers: {headers:?}"
    );
}

#[test]
fn test_cooldown_relative_reset_after_seconds_header_honored() {
    for seconds in [42, 600] {
        assert_cooldown(
            &[("anthropic-ratelimit-unified-reset-after-seconds", &seconds.to_string())],
            seconds,
        );
    }
}

#[test]
fn test_cooldown_absolute_reset_and_mixed_headers() {
    assert_cooldown(&[("x-ratelimit-reset-at", "1800000060")], 60);
    assert_cooldown(&[("x-ratelimit-reset-at", "1799999999")], 300);
    assert_cooldown(
        &[("x-ratelimit-reset-at", "1800000060"), ("x-ratelimit-reset-after-seconds", "42")],
        42,
    );
    assert_cooldown(
        &[("retry-after", "90"), ("x-ratelimit-reset-after-seconds", "42")],
        90,
    );
}

#[test]
fn test_cooldown_invalid_headers_fallback_and_overflow_clamp() {
    assert_cooldown(&[], 300);
    for value in ["0", "-1", "invalid", "9223372036854775808"] {
        assert_cooldown(&[("x-ratelimit-reset-after-seconds", value)], 300);
    }
    for name in ["retry-after", "x-ratelimit-reset-at", "x-ratelimit-reset-after-seconds"] {
        assert_cooldown(&[(name, "9223372036854775807")], 86_400);
    }
    assert_eq!(
        mahoquot_gateway::relay::cooldown_deadline_from_headers(&reqwest::header::HeaderMap::new(), i64::MAX - 1),
        i64::MAX
    );
}

#[tokio::test]
async fn test_t3_failover() {
    let mut servers = Vec::new();
    let mut shutdowns = Vec::new();
    // Given: Account A override always 429, Account B 200 SSE
    let listener_a = bind_fixture_listener().await;
    let port_a = listener_a.local_addr().unwrap().port();
    let app_a = Router::new().route(
        common::CODEX_PATH,
        post(|| async {
            (
                StatusCode::TOO_MANY_REQUESTS,
                [("Retry-After", "300")],
                "{\"error\":\"rate limited\"}",
            )
        }),
    );
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    shutdowns.push(shutdown);
    servers.push(tokio::spawn(async move {
        axum::serve(listener_a, app_a)
            .with_graceful_shutdown(async { stopped.await.unwrap(); })
            .await
            .unwrap();
    }));

    let listener_b = bind_fixture_listener().await;
    let port_b = listener_b.local_addr().unwrap().port();
    let app_b = Router::new().route(
        common::CODEX_PATH,
        post(|| async {
            (
                StatusCode::OK,
                [("Content-Type", "text/event-stream")],
                common::codex_sse("hello"),
            )
        }),
    );
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    shutdowns.push(shutdown);
    servers.push(tokio::spawn(async move {
        axum::serve(listener_b, app_b)
            .with_graceful_shutdown(async { stopped.await.unwrap(); })
            .await
            .unwrap();
    }));

    let temp_dir = unique_temp_dir("qgw-test-t3");
    let json_a = create_auth_file_json(
        "a",
        "acc_a",
        "token_a",
        Some(&format!("http://127.0.0.1:{port_a}")),
    );
    std::fs::write(temp_dir.join("codex-a-plus.json"), json_a).unwrap();

    let json_b = create_auth_file_json(
        "b",
        "acc_b",
        "token_b",
        Some(&format!("http://127.0.0.1:{port_b}")),
    );
    std::fs::write(temp_dir.join("codex-b-plus.json"), json_b).unwrap();

    let config = GatewayConfig {
        usage_poll_secs: 120,
        port: 0,
        auth_dir: temp_dir.clone(),
        config_path: temp_dir.join("config.yaml"),
        strategy: Strategy::FillFirst, // FillFirst will always try 'a' first
        max_failover: 3,
        log_level: "info".to_string(),
        api_keys: mahoquot_gateway::inbound::ApiKeys::default(),
        models_env: None,
        refresh_url: mahoquot_providers::refresh::REFRESH_TOKEN_URL.to_string(),
        auth_refresh_enabled: true,
        ..Default::default()
    };

    let state = Arc::new(AppState::new(&config).unwrap());
    let app = create_app(state.clone());
    let gw_listener = bind_fixture_listener().await;
    let gw_port = gw_listener.local_addr().unwrap().port();
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    shutdowns.push(shutdown);
    servers.push(tokio::spawn(async move {
        axum::serve(gw_listener, app)
            .with_graceful_shutdown(async { stopped.await.unwrap(); })
            .await
            .unwrap();
    }));

    let client = reqwest::Client::new();
    let gw_url = format!("http://127.0.0.1:{gw_port}/v1/chat/completions");

    // When: client makes single request
    let res = client
        .post(&gw_url)
        .header("Content-Type", "application/json")
        .body(common::OPENAI_REQUEST)
        .send()
        .await
        .unwrap();

    // Then: single client response 200 SSE
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    assert_eq!(
        res.headers().get("content-type").unwrap().to_str().unwrap(),
        "text/event-stream"
    );
    let body = res.text().await.unwrap();
    assert!(body.contains("data: [DONE]"));

    // Stats checks
    let stats = state.get_stats();
    assert!(
        stats.failed_over >= 1,
        "failed_over must be >= 1, got {}",
        stats.failed_over
    );
    assert_eq!(
        stats.exposed_errors, 0,
        "exposed_errors must be 0, got {}",
        stats.exposed_errors
    );

    // Account A state is Cooldown
    let acct_a = state.find_member("a").expect("member a exists");
    assert!(
        matches!(acct_a.health(), Health::Cooldown { .. }),
        "acct a must be in Cooldown, was {:?}",
        acct_a.health()
    );

    drop(client);
    for shutdown in shutdowns {
        shutdown.send(()).unwrap();
    }
    for server in servers {
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await.unwrap().unwrap();
    }
    drop(state);
    std::fs::remove_dir_all(&temp_dir).unwrap();
}

#[tokio::test]
async fn test_t3b_final_failure_records_the_attempted_account() {
    let mut servers = Vec::new();
    let mut shutdowns = Vec::new();
    // Given: the only available account's upstream always answers 429
    let listener = bind_fixture_listener().await;
    let port = listener.local_addr().unwrap().port();
    let app = Router::new().route(
        common::CODEX_PATH,
        post(|| async {
            (
                StatusCode::TOO_MANY_REQUESTS,
                [("Retry-After", "300")],
                "{\"error\":\"rate limited\"}",
            )
        }),
    );
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    shutdowns.push(shutdown);
    servers.push(tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async { stopped.await.unwrap(); })
            .await
            .unwrap();
    }));

    let temp_dir = unique_temp_dir("qgw-test-t3b");
    let json = create_auth_file_json(
        "a",
        "acc_a",
        "token_a",
        Some(&format!("http://127.0.0.1:{port}")),
    );
    std::fs::write(temp_dir.join("codex-a-plus.json"), json).unwrap();

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
        auth_refresh_enabled: true,
        ..Default::default()
    };

    let state = Arc::new(AppState::new(&config).unwrap());
    let app = create_app(state.clone());
    let gw_listener = bind_fixture_listener().await;
    let gw_port = gw_listener.local_addr().unwrap().port();
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    shutdowns.push(shutdown);
    servers.push(tokio::spawn(async move {
        axum::serve(gw_listener, app)
            .with_graceful_shutdown(async { stopped.await.unwrap(); })
            .await
            .unwrap();
    }));

    let client = reqwest::Client::new();
    let gw_url = format!("http://127.0.0.1:{gw_port}/v1/chat/completions");

    // When: the single attempt fails with 429 and no failover target remains
    let res = client
        .post(&gw_url)
        .header("Content-Type", "application/json")
        .body(common::OPENAI_REQUEST)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);

    res.bytes().await.unwrap();

    // Then: the recorded usage event names the attempted account, not "unknown"
    state.history.flush().unwrap();
    let rows = state
        .history
        .store()
        .unwrap()
        .export(&mahoquot_gateway::request_history::HistoryQuery::default())
        .unwrap();
    assert_eq!(rows.len(), 1, "exactly one usage event is recorded");
    assert_eq!(rows[0].status_code, 429);
    assert_eq!(
        rows[0].account_identifier, "a",
        "failed request must keep the attempted account id"
    );
    assert_eq!(rows[0].provider, "codex");

    drop(client);
    for shutdown in shutdowns {
        shutdown.send(()).unwrap();
    }
    for server in servers {
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await.unwrap().unwrap();
    }
    drop(state);
    std::fs::remove_dir_all(&temp_dir).unwrap();
}

#[tokio::test]
async fn test_t3c_final_failure_attribution_names_the_last_attempted_account() {
    let mut servers = Vec::new();
    let mut shutdowns = Vec::new();
    // Given: account A answers HTTP 500, account B closes connections
    let listener_a = bind_fixture_listener().await;
    let port_a = listener_a.local_addr().unwrap().port();
    let app_a = Router::new().route(
        common::CODEX_PATH,
        post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "{\"error\":\"boom\"}") }),
    );
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    shutdowns.push(shutdown);
    servers.push(tokio::spawn(async move {
        axum::serve(listener_a, app_a)
            .with_graceful_shutdown(async { stopped.await.unwrap(); })
            .await
            .unwrap();
    }));

    // Hold the port and close accepted connections to force a transport failure.
    let dead = bind_fixture_listener().await;
    let dead_port = dead.local_addr().unwrap().port();
    let transport_server = tokio::spawn(async move {
        loop {
            let (connection, _) = dead.accept().await.unwrap();
            drop(connection);
        }
    });

    let temp_dir = unique_temp_dir("qgw-test-t3c");
    for (id, upstream) in [
        ("a", format!("http://127.0.0.1:{port_a}")),
        ("b", format!("http://127.0.0.1:{dead_port}")),
    ] {
        let json = create_auth_file_json(
            id,
            &format!("acc_{id}"),
            &format!("token_{id}"),
            Some(&upstream),
        );
        std::fs::write(temp_dir.join(format!("codex-{id}-plus.json")), json).unwrap();
    }

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
        auth_refresh_enabled: true,
        ..Default::default()
    };

    let state = Arc::new(AppState::new(&config).unwrap());
    let app = create_app(state.clone());
    let gw_listener = bind_fixture_listener().await;
    let gw_port = gw_listener.local_addr().unwrap().port();
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    shutdowns.push(shutdown);
    servers.push(tokio::spawn(async move {
        axum::serve(gw_listener, app)
            .with_graceful_shutdown(async { stopped.await.unwrap(); })
            .await
            .unwrap();
    }));

    let client = reqwest::Client::new();
    let gw_url = format!("http://127.0.0.1:{gw_port}/v1/chat/completions");

    // When: A fails with HTTP 500, then B fails during transport
    let res = client
        .post(&gw_url)
        .header("Content-Type", "application/json")
        .body(common::OPENAI_REQUEST)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);

    res.bytes().await.unwrap();

    // Then: the usage event names B — the last ATTEMPTED account — not A,
    // whose HTTP failure merely produced the final response body.
    state.history.flush().unwrap();
    let rows = state
        .history
        .store()
        .unwrap()
        .export(&mahoquot_gateway::request_history::HistoryQuery::default())
        .unwrap();
    assert_eq!(rows.len(), 1, "exactly one usage event is recorded");
    assert_eq!(
        rows[0].account_identifier, "b",
        "attribution must follow the last attempted account, not the last HTTP failure"
    );
    assert_eq!(rows[0].provider, "codex");

    transport_server.abort();
    assert!(transport_server.await.unwrap_err().is_cancelled());
    drop(client);
    for shutdown in shutdowns {
        shutdown.send(()).unwrap();
    }
    for server in servers {
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await.unwrap().unwrap();
    }
    drop(state);
    std::fs::remove_dir_all(&temp_dir).unwrap();
}

#[tokio::test]
async fn server_error_failover_keeps_health_and_moves_to_the_next_account() {
    let mut servers = Vec::new();
    let mut shutdowns = Vec::new();
    // Given: Account A (FillFirst order) always answers 500, Account B 200 SSE
    let listener_a = bind_fixture_listener().await;
    let port_a = listener_a.local_addr().unwrap().port();
    let app_a = Router::new().route(
        common::CODEX_PATH,
        post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "{\"error\":\"boom\"}") }),
    );
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    shutdowns.push(shutdown);
    servers.push(tokio::spawn(async move {
        axum::serve(listener_a, app_a)
            .with_graceful_shutdown(async { stopped.await.unwrap(); })
            .await
            .unwrap();
    }));

    let listener_b = bind_fixture_listener().await;
    let port_b = listener_b.local_addr().unwrap().port();
    let app_b = Router::new().route(
        common::CODEX_PATH,
        post(|| async {
            (
                StatusCode::OK,
                [("Content-Type", "text/event-stream")],
                common::codex_sse("hello"),
            )
        }),
    );
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    shutdowns.push(shutdown);
    servers.push(tokio::spawn(async move {
        axum::serve(listener_b, app_b)
            .with_graceful_shutdown(async { stopped.await.unwrap(); })
            .await
            .unwrap();
    }));

    let temp_dir = unique_temp_dir("qgw-test-t3-server-error");
    let json_a = create_auth_file_json(
        "a",
        "acc_a",
        "token_a",
        Some(&format!("http://127.0.0.1:{port_a}")),
    );
    std::fs::write(temp_dir.join("codex-a-plus.json"), json_a).unwrap();

    let json_b = create_auth_file_json(
        "b",
        "acc_b",
        "token_b",
        Some(&format!("http://127.0.0.1:{port_b}")),
    );
    std::fs::write(temp_dir.join("codex-b-plus.json"), json_b).unwrap();

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
        auth_refresh_enabled: true,
        ..Default::default()
    };

    let state = Arc::new(AppState::new(&config).unwrap());
    let app = create_app(state.clone());
    let gw_listener = bind_fixture_listener().await;
    let gw_port = gw_listener.local_addr().unwrap().port();
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    shutdowns.push(shutdown);
    servers.push(tokio::spawn(async move {
        axum::serve(gw_listener, app)
            .with_graceful_shutdown(async { stopped.await.unwrap(); })
            .await
            .unwrap();
    }));

    let client = reqwest::Client::new();
    let gw_url = format!("http://127.0.0.1:{gw_port}/v1/chat/completions");

    // When: one request bound to the affinity session (A is first in FillFirst
    // order, so without exclusion the bound account would be retried forever)
    let res = client
        .post(&gw_url)
        .header("Content-Type", "application/json")
        .header("x-session-id", "conv-server-error")
        .body(common::OPENAI_REQUEST)
        .send()
        .await
        .unwrap();

    // Then: the client still gets a 200 SSE from account B
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    let body = res.text().await.unwrap();
    assert!(body.contains("data: [DONE]"));

    // And: a transient upstream 5xx never benches account A
    let acct_a = state.find_member("a").expect("member a exists");
    assert_eq!(
        acct_a.health(),
        Health::Available,
        "5xx must leave health unchanged, was {:?}",
        acct_a.health()
    );

    drop(client);
    for shutdown in shutdowns {
        shutdown.send(()).unwrap();
    }
    for server in servers {
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await.unwrap().unwrap();
    }
    drop(state);
    std::fs::remove_dir_all(&temp_dir).unwrap();
}
