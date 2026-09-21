mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::Router;
use common::{create_auth_file_json, unique_temp_dir};
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::inbound::ApiKeys;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use mahoquot_types::Strategy;

#[tokio::test]
async fn test_fill_first_routing_follows_accounts_view_order_sst() {
    let call_count_a = Arc::new(AtomicUsize::new(0));
    let call_count_b = Arc::new(AtomicUsize::new(0));

    let count_a_clone = call_count_a.clone();
    let listener_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_a = listener_a.local_addr().unwrap().port();
    let app_a = Router::new().route(
        common::CODEX_PATH,
        post(move |_headers: HeaderMap| {
            let count = count_a_clone.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    [("Content-Type", "text/event-stream")],
                    common::codex_sse("from_a"),
                )
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener_a, app_a).await.unwrap();
    });

    let count_b_clone = call_count_b.clone();
    let listener_b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_b = listener_b.local_addr().unwrap().port();
    let app_b = Router::new().route(
        common::CODEX_PATH,
        post(move |_headers: HeaderMap| {
            let count = count_b_clone.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    [("Content-Type", "text/event-stream")],
                    common::codex_sse("from_b"),
                )
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener_b, app_b).await.unwrap();
    });

    let temp_dir = unique_temp_dir("qgw-test-t29-sst");

    let json_a = create_auth_file_json(
        "acc_a",
        "acc_id_a",
        "tok_a",
        Some(&format!("http://127.0.0.1:{port_a}")),
    );
    let json_b = create_auth_file_json(
        "acc_b",
        "acc_id_b",
        "tok_b",
        Some(&format!("http://127.0.0.1:{port_b}")),
    );

    let file_a = "codex-acc_a-plus.json";
    let file_b = "codex-acc_b-plus.json";

    std::fs::write(temp_dir.join(file_a), json_a).unwrap();
    std::fs::write(temp_dir.join(file_b), json_b).unwrap();

    let initial_order = serde_json::json!([file_a, file_b]);
    std::fs::write(
        temp_dir.join(".mahoquot-account-order.json"),
        serde_json::to_vec(&initial_order).unwrap(),
    )
    .unwrap();

    let config = GatewayConfig {
        usage_poll_secs: 120,
        port: 0,
        auth_dir: temp_dir.clone(),
        strategy: Strategy::FillFirst,
        max_failover: 1,
        log_level: "info".to_string(),
        api_keys: ApiKeys::default(),
        config_path: temp_dir.join("config.yaml"),
        ..Default::default()
    };

    let state = Arc::new(AppState::new(&config).unwrap());
    let app = create_app(state.clone());
    let gw_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_port = gw_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(gw_listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();
    let chat_url = format!("http://127.0.0.1:{gw_port}/v1/chat/completions");
    let management_order_url = format!("http://127.0.0.1:{gw_port}/v0/management/auth-files/order");

    let res1 = client
        .post(&chat_url)
        .header("Content-Type", "application/json")
        .body(common::OPENAI_REQUEST)
        .send()
        .await
        .unwrap();
    assert_eq!(res1.status(), reqwest::StatusCode::OK);
    assert_eq!(call_count_a.load(Ordering::SeqCst), 1);
    assert_eq!(call_count_b.load(Ordering::SeqCst), 0);

    let reorder_res = client
        .put(&management_order_url)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "names": [file_b, file_a]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(reorder_res.status(), reqwest::StatusCode::OK);

    let live_members = state.pool.load().members.clone();
    assert_eq!(live_members[0].id, "acc_b");
    assert_eq!(live_members[1].id, "acc_a");

    let res2 = client
        .post(&chat_url)
        .header("Content-Type", "application/json")
        .body(common::OPENAI_REQUEST)
        .send()
        .await
        .unwrap();
    assert_eq!(res2.status(), reqwest::StatusCode::OK);
    assert_eq!(call_count_a.load(Ordering::SeqCst), 1);
    assert_eq!(call_count_b.load(Ordering::SeqCst), 1);

    std::fs::remove_dir_all(&temp_dir).ok();
}

const OPENAI_COMPLETIONS_RESPONSE: &str = r#"{"id":"chatcmpl-test","object":"chat.completion","created":1,"model":"shared-model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;

fn write_generic_auth_file(
    dir: &std::path::Path,
    file_name: &str,
    value: &serde_json::Value,
) -> String {
    std::fs::write(dir.join(file_name), serde_json::to_string(value).unwrap()).unwrap();
    file_name.to_string()
}

#[tokio::test]
async fn test_fill_first_cross_provider_account_order_sst() {
    let call_count_cline = Arc::new(AtomicUsize::new(0));
    let call_count_antigravity = Arc::new(AtomicUsize::new(0));

    let count_cline_clone = call_count_cline.clone();
    let listener_cline = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_cline = listener_cline.local_addr().unwrap().port();
    let app_cline = Router::new()
        .route(
            "/chat/completions",
            post({
                let count = count_cline_clone.clone();
                move || {
                    let count = count.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::OK,
                            [("content-type", "application/json")],
                            OPENAI_COMPLETIONS_RESPONSE,
                        )
                    }
                }
            }),
        )
        .fallback(post(move || {
            let count = count_cline_clone.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    [("content-type", "application/json")],
                    OPENAI_COMPLETIONS_RESPONSE,
                )
            }
        }));
    tokio::spawn(async move {
        axum::serve(listener_cline, app_cline).await.unwrap();
    });

    let count_ag_clone = call_count_antigravity.clone();
    let listener_ag = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_ag = listener_ag.local_addr().unwrap().port();
    let app_ag = Router::new()
        .route(
            "/chat/completions",
            post({
                let count = count_ag_clone.clone();
                move || {
                    let count = count.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::OK,
                            [("content-type", "application/json")],
                            OPENAI_COMPLETIONS_RESPONSE,
                        )
                    }
                }
            }),
        )
        .fallback(post(move || {
            let count = count_ag_clone.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    [("content-type", "application/json")],
                    OPENAI_COMPLETIONS_RESPONSE,
                )
            }
        }));
    tokio::spawn(async move {
        axum::serve(listener_ag, app_ag).await.unwrap();
    });

    let temp_dir = unique_temp_dir("qgw-test-t29-cross-provider");
    let file_cline = write_generic_auth_file(
        &temp_dir,
        "generic-cline.json",
        &serde_json::json!({
            "type": "generic",
            "provider": "cline-provider",
            "label": "Cline",
            "adapter": "openai-chat",
            "base_url": format!("http://127.0.0.1:{port_cline}"),
            "api_url": format!("http://127.0.0.1:{port_cline}"),
            "api_key": "fixture-cline",
            "models": ["shared-model", "gpt-4o", "codex"],
            "model_list": ["shared-model", "gpt-4o", "codex"]
        }),
    );
    let file_antigravity = write_generic_auth_file(
        &temp_dir,
        "generic-antigravity.json",
        &serde_json::json!({
            "type": "generic",
            "provider": "antigravity-provider",
            "label": "Antigravity",
            "adapter": "openai-chat",
            "base_url": format!("http://127.0.0.1:{port_ag}"),
            "api_url": format!("http://127.0.0.1:{port_ag}"),
            "api_key": "fixture-ag",
            "models": ["shared-model", "gpt-4o", "codex"],
            "model_list": ["shared-model", "gpt-4o", "codex"]
        }),
    );

    let config = GatewayConfig {
        usage_poll_secs: 120,
        port: 0,
        auth_dir: temp_dir.clone(),
        strategy: Strategy::FillFirst,
        max_failover: 1,
        log_level: "info".to_string(),
        api_keys: ApiKeys::default(),
        config_path: temp_dir.join("config.yaml"),
        auth_refresh_enabled: false,
        ..Default::default()
    };

    let state = Arc::new(AppState::new(&config).unwrap());
    let app = create_app(state.clone());
    let gw_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_port = gw_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(gw_listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();
    let chat_url = format!("http://127.0.0.1:{gw_port}/v1/chat/completions");
    let management_order_url = format!("http://127.0.0.1:{gw_port}/v0/management/auth-files/order");

    let reorder_res = client
        .put(&management_order_url)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "names": [file_cline, file_antigravity]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(reorder_res.status(), reqwest::StatusCode::OK);

    let shared_model_req = serde_json::json!({
        "model": "shared-model",
        "messages": [{"role": "user", "content": "hi"}]
    })
    .to_string();

    let res1 = client
        .post(&chat_url)
        .header("Content-Type", "application/json")
        .body(shared_model_req.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(res1.status(), reqwest::StatusCode::OK);
    assert_eq!(call_count_cline.load(Ordering::SeqCst), 1);
    assert_eq!(call_count_antigravity.load(Ordering::SeqCst), 0);

    let reorder_res2 = client
        .put(&management_order_url)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "names": [file_antigravity, file_cline]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(reorder_res2.status(), reqwest::StatusCode::OK);

    let res2 = client
        .post(&chat_url)
        .header("Content-Type", "application/json")
        .body(shared_model_req)
        .send()
        .await
        .unwrap();
    assert_eq!(res2.status(), reqwest::StatusCode::OK);
    assert_eq!(call_count_cline.load(Ordering::SeqCst), 1);
    assert_eq!(call_count_antigravity.load(Ordering::SeqCst), 1);

    std::fs::remove_dir_all(&temp_dir).ok();
}

#[tokio::test]
async fn test_runtime_strategy_switch_sst() {
    let call_count_a = Arc::new(AtomicUsize::new(0));
    let call_count_b = Arc::new(AtomicUsize::new(0));

    let count_a_clone = call_count_a.clone();
    let listener_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_a = listener_a.local_addr().unwrap().port();
    let app_a = Router::new()
        .route(
            "/chat/completions",
            post({
                let count = count_a_clone.clone();
                move || {
                    let count = count.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::OK,
                            [("content-type", "application/json")],
                            OPENAI_COMPLETIONS_RESPONSE,
                        )
                    }
                }
            }),
        )
        .fallback(post(move || {
            let count = count_a_clone.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    [("content-type", "application/json")],
                    OPENAI_COMPLETIONS_RESPONSE,
                )
            }
        }));
    tokio::spawn(async move {
        axum::serve(listener_a, app_a).await.unwrap();
    });

    let count_b_clone = call_count_b.clone();
    let listener_b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_b = listener_b.local_addr().unwrap().port();
    let app_b = Router::new()
        .route(
            "/chat/completions",
            post({
                let count = count_b_clone.clone();
                move || {
                    let count = count.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::OK,
                            [("content-type", "application/json")],
                            OPENAI_COMPLETIONS_RESPONSE,
                        )
                    }
                }
            }),
        )
        .fallback(post(move || {
            let count = count_b_clone.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    [("content-type", "application/json")],
                    OPENAI_COMPLETIONS_RESPONSE,
                )
            }
        }));
    tokio::spawn(async move {
        axum::serve(listener_b, app_b).await.unwrap();
    });

    let temp_dir = unique_temp_dir("qgw-test-t29-switch");
    let file_a = write_generic_auth_file(
        &temp_dir,
        "generic-alpha.json",
        &serde_json::json!({
            "type": "generic",
            "provider": "alpha-provider",
            "label": "Alpha",
            "adapter": "openai-chat",
            "base_url": format!("http://127.0.0.1:{port_a}"),
            "api_url": format!("http://127.0.0.1:{port_a}"),
            "api_key": "secret",
            "models": ["shared-model", "gpt-4o", "codex"],
            "model_list": ["shared-model", "gpt-4o", "codex"]
        }),
    );
    let file_b = write_generic_auth_file(
        &temp_dir,
        "generic-beta.json",
        &serde_json::json!({
            "type": "generic",
            "provider": "beta-provider",
            "label": "Beta",
            "adapter": "openai-chat",
            "base_url": format!("http://127.0.0.1:{port_b}"),
            "api_url": format!("http://127.0.0.1:{port_b}"),
            "api_key": "secret",
            "models": ["shared-model", "gpt-4o", "codex"],
            "model_list": ["shared-model", "gpt-4o", "codex"]
        }),
    );

    let config = GatewayConfig {
        usage_poll_secs: 120,
        port: 0,
        auth_dir: temp_dir.clone(),
        strategy: Strategy::StrictRoundRobin,
        max_failover: 1,
        log_level: "info".to_string(),
        api_keys: ApiKeys::default(),
        config_path: temp_dir.join("config.yaml"),
        auth_refresh_enabled: false,
        ..Default::default()
    };

    let state = Arc::new(AppState::new(&config).unwrap());
    let app = create_app(state.clone());
    let gw_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_port = gw_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(gw_listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();
    let chat_url = format!("http://127.0.0.1:{gw_port}/v1/chat/completions");
    let management_order_url = format!("http://127.0.0.1:{gw_port}/v0/management/auth-files/order");
    let management_strategy_url =
        format!("http://127.0.0.1:{gw_port}/v0/management/routing/strategy");

    let order_res = client
        .put(&management_order_url)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "names": [file_a, file_b]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(order_res.status(), reqwest::StatusCode::OK);

    let shared_model_req = serde_json::json!({
        "model": "shared-model",
        "messages": [{"role": "user", "content": "hi"}]
    })
    .to_string();

    let res_rr1 = client
        .post(&chat_url)
        .header("Content-Type", "application/json")
        .body(shared_model_req.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(res_rr1.status(), reqwest::StatusCode::OK);

    let res_rr2 = client
        .post(&chat_url)
        .header("Content-Type", "application/json")
        .body(shared_model_req.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(res_rr2.status(), reqwest::StatusCode::OK);
    assert_eq!(call_count_a.load(Ordering::SeqCst), 1);
    assert_eq!(call_count_b.load(Ordering::SeqCst), 1);

    let switch_res = client
        .put(&management_strategy_url)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "value": "fill-first"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(switch_res.status(), reqwest::StatusCode::OK);

    let res_ff1 = client
        .post(&chat_url)
        .header("Content-Type", "application/json")
        .body(shared_model_req.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(res_ff1.status(), reqwest::StatusCode::OK);

    let res_ff2 = client
        .post(&chat_url)
        .header("Content-Type", "application/json")
        .body(shared_model_req.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(res_ff2.status(), reqwest::StatusCode::OK);

    let res_ff3 = client
        .post(&chat_url)
        .header("Content-Type", "application/json")
        .body(shared_model_req.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(res_ff3.status(), reqwest::StatusCode::OK);

    assert_eq!(call_count_a.load(Ordering::SeqCst), 4);
    assert_eq!(call_count_b.load(Ordering::SeqCst), 1);

    std::fs::remove_dir_all(&temp_dir).ok();
}

