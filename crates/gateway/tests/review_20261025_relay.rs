use axum::{body::Body, http::Request};
use futures::{SinkExt, StreamExt};
use mahoquot_gateway::{
    config::GatewayConfig, management::settings::Settings, routes::create_app, state::AppState,
};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tower::ServiceExt;

struct Cleanup {
    root: std::path::PathBuf,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        self.server.abort();
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

#[tokio::test]
async fn worker_usage_charges_live_and_durable_budget_once() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let root = std::env::temp_dir().join(format!("relay-worker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let identifier = mahoquot_gateway::request_history::stable_key_identifier("scoped");
        let config = GatewayConfig { auth_dir: root.clone(), config_path: root.join("config.yaml"), auth_refresh_enabled: false, ..Default::default() };
        Settings {
            api_keys: vec!["master".into()],
            auth_dir: root.to_string_lossy().into_owned(),
            scoped_api_keys: vec![serde_json::from_value(json!({"id":"limited","name":"limited","key_identifier":identifier,"key_prefix":"scoped","token_limit":1,"token_used":0,"allowed_providers":["aistudio"]})).unwrap()],
            ..Default::default()
        }.persist(&config.config_path).unwrap();
        let state = Arc::new(AppState::new(&config).unwrap());
        let app = create_app(state.clone());
        let mut socket = None;
        for port in 18840..=18899 {
            match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
                Ok(listener) => { socket = Some(listener); break; }
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {},
                Err(e) => panic!("{e}"),
            }
        }
        let socket = socket.expect("available fixture port");
        let address = socket.local_addr().unwrap();
        let serving = app.clone();
        let mut cleanup = Cleanup { root, server: tokio::spawn(async move { axum::serve(socket, serving).await.unwrap(); }) };
        let tcp = tokio::net::TcpStream::connect(address).await.unwrap();
        let request = tokio_tungstenite::tungstenite::handshake::client::Request::builder()
            .uri(format!("ws://{address}/v1/ws"))
            .header("host", address.to_string()).header("connection", "Upgrade")
            .header("upgrade", "websocket").header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .header("authorization", "Bearer master").header("x-aistudio-worker-id", "aistudio-worker-fixture")
            .body(()).unwrap();
        let (mut ws, _) = tokio_tungstenite::client_async(request, tcp).await.unwrap();
        ws.send(tokio_tungstenite::tungstenite::Message::Text(json!({"id":"catalog","type":"catalog","payload":{"models":["aistudio-fixture"],"ack":true}}).to_string().into())).await.unwrap();
        let ack: serde_json::Value = serde_json::from_str(&ws.next().await.unwrap().unwrap().into_text().unwrap()).unwrap();
        assert_eq!(ack["type"], "catalog_ack");
        let inference = || Request::post("/v1/chat/completions").header("authorization", "Bearer scoped").header("content-type", "application/json")
            .body(Body::from(json!({"model":"aistudio-fixture","messages":[{"role":"user","content":"hi"}]}).to_string())).unwrap();
        let call = tokio::spawn(app.clone().oneshot(inference()));
        let request: serde_json::Value = serde_json::from_str(&ws.next().await.unwrap().unwrap().into_text().unwrap()).unwrap();
        ws.send(tokio_tungstenite::tungstenite::Message::Text(json!({"id":request["id"],"type":"http_response","payload":{"status":503,"body":"fixture unavailable"}}).to_string().into())).await.unwrap();
        assert_eq!(call.await.unwrap().unwrap().status(), 503);
        assert_eq!(state.scoped_keys.get(&identifier).unwrap().token_used(), 0);
        let call = tokio::spawn(app.clone().oneshot(inference()));
        let request: serde_json::Value = serde_json::from_str(&ws.next().await.unwrap().unwrap().into_text().unwrap()).unwrap();
        ws.send(tokio_tungstenite::tungstenite::Message::Text(json!({"id":request["id"],"type":"http_response","payload":{"status":200,"headers":{"content-type":"application/json"},"body":json!({"candidates":[{"content":{"parts":[{"text":"hello"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3,"totalTokenCount":10}}).to_string()}}).to_string().into())).await.unwrap();
        let response = call.await.unwrap().unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(state.scoped_keys.get(&identifier).unwrap().token_used(), 10);
        assert_eq!(state.settings.current().scoped_api_keys[0].token_used, 10);
        let reloaded = Settings::load(&config.config_path).unwrap();
        assert_eq!(reloaded.scoped_api_keys[0].token_used, 10);
        assert_eq!(app.oneshot(inference()).await.unwrap().status(), 429);
        let history = state.clone();
        let totals = tokio::task::spawn_blocking(move || {
            history.history.flush().unwrap();
            history.history.store().unwrap().query(&Default::default()).unwrap().totals
        }).await.unwrap();
        assert_eq!(totals.requests, 2);
        assert_eq!(totals.successful_requests, 1);
        assert_eq!(totals.failed_requests, 1);
        assert_eq!(totals.total_tokens, 10);
        ws.close(None).await.unwrap();
        cleanup.server.abort();
        assert!((&mut cleanup.server).await.unwrap_err().is_cancelled());
    }).await.unwrap();
}
