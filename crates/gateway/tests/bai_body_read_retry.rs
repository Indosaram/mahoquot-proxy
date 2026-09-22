mod common;
use std::sync::{Arc, Mutex};
use axum::{body::Bytes, extract::State, http::StatusCode, routing::post, Router};
use mahoquot_gateway::{config::GatewayConfig, inbound::ApiKeys, routes::create_app, state::AppState};

#[tokio::test]
async fn bai_body_read_failure_replays_once_without_changing_body() {
    for (provider, failures, message, attempts, expected) in [
        ("b-ai", 1, "read body failed", 2, StatusCode::OK),
        ("b-ai", 3, "read body failed", 2, StatusCode::BAD_REQUEST),
        ("b-ai", 1, "invalid role", 1, StatusCode::BAD_REQUEST),
        ("other", 1, "read body failed", 1, StatusCode::BAD_REQUEST),
    ] {
    let seen = Arc::new(Mutex::new(Vec::<Bytes>::new()));
    let mock = Router::new().fallback(post(move |State(seen): State<Arc<Mutex<Vec<Bytes>>>>, body: Bytes| async move {
        let mut requests = seen.lock().unwrap();
        requests.push(body);
        if requests.len() <= failures {
            (StatusCode::BAD_REQUEST, [("content-type", "application/json")], serde_json::json!({"error":{"message":format!("The request is invalid: {message}. Please check the request body, required fields, and request format."),"type":"gateway_error","param":"","code":"400001"}}).to_string())
        } else {
            (StatusCode::OK, [("content-type", "application/json")], r#"{"id":"test","object":"chat.completion","model":"glm-5.3-flash","choices":[{"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}]}"#.to_string())
        }
    })).with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:18897").await.unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let dir = common::unique_temp_dir("bai-body-read-retry");
    std::fs::write(dir.join("generic-b-ai-test.json"), serde_json::json!({
        "type":"generic", "provider":provider, "adapter":"openai-chat",
        "base_url":"http://127.0.0.1:18897", "upstream_override":"http://127.0.0.1:18897",
        "api_key":"mock", "models":["glm-5.3-flash"]
    }).to_string()).unwrap();
    let config = GatewayConfig {
        auth_dir: dir.clone(), config_path: dir.join("config.yaml"),
        api_keys: ApiKeys::from_env_value("test"), auth_refresh_enabled:false,
        ..GatewayConfig::default()
    };
    let app = create_app(Arc::new(AppState::new(&config).unwrap()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:18898").await.unwrap();
    let gateway = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let response = reqwest::Client::new().post("http://127.0.0.1:18898/v1/chat/completions")
        .bearer_auth("test").json(&serde_json::json!({"model":"glm-5.3-flash","messages":[{"role":"user","content":"hello"}],"stream":false}))
        .timeout(std::time::Duration::from_secs(10)).send().await.unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    gateway.abort(); server.abort();
    assert!(gateway.await.unwrap_err().is_cancelled());
    assert!(server.await.unwrap_err().is_cancelled());
    std::fs::remove_dir_all(dir).unwrap();
    assert_eq!(status, expected, "{body}");
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), attempts);
    if attempts == 2 {
        assert_eq!(requests[0], requests[1]);
    }
    }
}
