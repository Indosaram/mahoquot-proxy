use std::sync::Arc;

use axum::http::StatusCode;
use futures::{SinkExt, StreamExt};
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use mahoquot_types::Strategy;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::handshake::client::Request as WsRequest;
use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

mod common;

fn gateway_config(auth_dir: &std::path::Path, port: u16) -> GatewayConfig {
    GatewayConfig {
        port,
        auth_dir: auth_dir.to_path_buf(),
        strategy: Strategy::StrictRoundRobin,
        max_failover: 3,
        log_level: "info".to_string(),
        api_keys: mahoquot_gateway::inbound::ApiKeys::default(),
        models_env: None,
        refresh_url: mahoquot_providers::refresh::REFRESH_TOKEN_URL.to_string(),
        auth_refresh_enabled: false,
        usage_poll_secs: 120,
        config_path: auth_dir.join("config.yaml"),
        catalog_cache_path: None,
        history_queue_capacity: 1024,
        history_batch_size: 64,
        max_concurrent_inference: 1024,
        captcha_config_url: None,
        captcha_solver_bin: None,
    }
}

#[tokio::test]
async fn test_aistudio_worker_lifecycle_catalog_and_correlation() {
    let auth_dir = common::unique_temp_dir("aistudio-worker-test");
    std::fs::write(
        auth_dir.join("config.yaml"),
        "routing:\n  strategy: fill-first\napi-keys:\n  - test-key\n",
    )
    .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let cfg = gateway_config(&auth_dir, port);
    let state = Arc::new(AppState::new(&cfg).unwrap());
    let app = create_app(state);

    let server_task = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .await
            .unwrap();
    });

    let client = reqwest::Client::new();
    let base_url = format!("http://127.0.0.1:{port}");

    let res = client
        .get(format!("{base_url}/v1/models"))
        .header("authorization", "Bearer test-key")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let models_body: Value = res.json().await.unwrap();
    let data = models_body["data"].as_array().unwrap();
    assert!(!data.iter().any(|m| m["id"] == "aistudio-remote-pro"));

    let res = client
        .post(format!("{base_url}/v1/chat/completions"))
        .header("authorization", "Bearer test-key")
        .json(&json!({
            "model": "aistudio-remote-pro",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    let err_body: Value = res.json().await.unwrap();
    assert!(err_body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("No connected AI Studio worker"));

    let tcp = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    let ws_req = WsRequest::builder()
        .uri(format!("ws://127.0.0.1:{port}/v1/ws"))
        .header("host", format!("127.0.0.1:{port}"))
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("x-aistudio-worker-id", "aistudio-worker-integration-1")
        .header("authorization", "Bearer test-key")
        .body(())
        .unwrap();

    let (mut ws_stream, _) = tokio_tungstenite::client_async(ws_req, tcp).await.unwrap();

    let catalog_msg = json!({
        "id": "cat-msg-1",
        "type": "catalog",
        "payload": {
            "models": ["aistudio-remote-pro", "aistudio-remote-flash"]
        }
    });
    ws_stream
        .send(TungsteniteMessage::Text(catalog_msg.to_string().into()))
        .await
        .unwrap();

    let res = client
        .get(format!("{base_url}/v1/models"))
        .header("authorization", "Bearer test-key")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let models_body: Value = res.json().await.unwrap();
    let data = models_body["data"].as_array().unwrap();
    assert!(data.iter().any(|m| m["id"] == "aistudio-remote-pro"));
    assert!(data.iter().any(|m| m["id"] == "aistudio-remote-flash"));

    let req_a = client
        .post(format!("{base_url}/v1/chat/completions"))
        .header("authorization", "Bearer test-key")
        .json(&json!({
            "model": "aistudio-remote-pro",
            "messages": [{"role": "user", "content": "question-A"}]
        }));
    let req_b = client
        .post(format!("{base_url}/v1/chat/completions"))
        .header("authorization", "Bearer test-key")
        .json(&json!({
            "model": "aistudio-remote-pro",
            "messages": [{"role": "user", "content": "question-B"}]
        }));

    let handle_a = tokio::spawn(async move { req_a.send().await.unwrap() });
    let handle_b = tokio::spawn(async move { req_b.send().await.unwrap() });

    let frame_1 = ws_stream.next().await.unwrap().unwrap();
    let frame_2 = ws_stream.next().await.unwrap().unwrap();

    let val_1: Value = serde_json::from_str(&frame_1.into_text().unwrap()).unwrap();
    let val_2: Value = serde_json::from_str(&frame_2.into_text().unwrap()).unwrap();

    assert_eq!(val_1["type"], "http_request");
    assert_eq!(val_2["type"], "http_request");
    let id_1 = val_1["id"].as_str().unwrap().to_string();
    let id_2 = val_2["id"].as_str().unwrap().to_string();
    assert_ne!(id_1, id_2);

    let body_2 = json!({
        "candidates": [{
            "content": {
                "parts": [{"text": format!("reply-for-{id_2}")}]
            }
        }]
    });
    ws_stream
        .send(TungsteniteMessage::Text(
            json!({
                "id": id_2,
                "type": "http_response",
                "payload": {
                    "status": 200,
                    "headers": {"content-type": "application/json"},
                    "body": body_2.to_string()
                }
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    let body_1 = json!({
        "candidates": [{
            "content": {
                "parts": [{"text": format!("reply-for-{id_1}")}]
            }
        }]
    });
    ws_stream
        .send(TungsteniteMessage::Text(
            json!({
                "id": id_1,
                "type": "http_response",
                "payload": {
                    "status": 200,
                    "headers": {"content-type": "application/json"},
                    "body": body_1.to_string()
                }
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    let res_a = handle_a.await.unwrap();
    let res_b = handle_b.await.unwrap();
    assert_eq!(res_a.status(), StatusCode::OK);
    assert_eq!(res_b.status(), StatusCode::OK);

    let json_a: Value = res_a.json().await.unwrap();
    let json_b: Value = res_b.json().await.unwrap();
    let text_a = json_a["choices"][0]["message"]["content"].as_str().unwrap();
    let text_b = json_b["choices"][0]["message"]["content"].as_str().unwrap();

    assert!(text_a.contains(&id_1) || text_a.contains(&id_2));
    assert!(text_b.contains(&id_1) || text_b.contains(&id_2));
    assert_ne!(text_a, text_b);

    ws_stream
        .send(TungsteniteMessage::Text(
            json!({
                "id": "unmatched-frame-id-999",
                "type": "http_response",
                "payload": {"status": 200, "body": "ignore me"}
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    ws_stream.close(None).await.unwrap();
    drop(ws_stream);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let res = client
            .get(format!("{base_url}/v1/models"))
            .header("authorization", "Bearer test-key")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let models_body: Value = res.json().await.unwrap();
        let data = models_body["data"].as_array().unwrap();
        if !data.iter().any(|m| m["id"] == "aistudio-remote-pro") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "disconnected worker's model is still listed after 5s"
        );
        tokio::task::yield_now().await;
    }

    let res = client
        .post(format!("{base_url}/v1/chat/completions"))
        .header("authorization", "Bearer test-key")
        .json(&json!({
            "model": "aistudio-remote-pro",
            "messages": [{"role": "user", "content": "after disconnect"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);

    server_task.abort();
}
