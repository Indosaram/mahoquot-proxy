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
