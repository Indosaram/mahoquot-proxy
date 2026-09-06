use std::sync::{Arc, Mutex};
use axum::{body::Body, http::{Request, StatusCode}};
use mahoquot_gateway::{config::GatewayConfig, management::settings::{Settings, ScopedApiKey}, routes::create_app, state::AppState};
use tower::ServiceExt;

mod common;

#[tokio::test(flavor = "current_thread")]
async fn management_mutation_keeps_single_thread_executor_available() {
    for (method, path, body) in [
        ("PATCH", "/v0/management/scoped-keys/key", r#"{"name":"renamed"}"#),
        ("DELETE", "/v0/management/scoped-keys/key", ""),
        ("PUT", "/v0/management/request-retry", r#"{"value":3}"#),
        ("PUT", "/v0/management/api-keys", r#"["master","second"]"#),
        ("PUT", "/v0/management/request-log", r#"{"value":true}"#),
        ("PUT", "/v0/management/logs-max-total-size-mb", r#"{"value":5}"#),
        ("PUT", "/v0/management/api-key-bindings", r#"{"api_key":"master","account":"fixture"}"#),
    ] {
        let dir = std::env::temp_dir().join(format!("review-persistence-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut credential: serde_json::Value = serde_json::from_str(&common::create_auth_file_json("fixture", "fixture", "mock", Some("http://127.0.0.1:18879"))).unwrap();
        credential["usage_override"] = serde_json::json!("http://127.0.0.1:18879");
        std::fs::write(dir.join("codex-fixture.json"), credential.to_string()).unwrap();
        let config = GatewayConfig { auth_dir: dir.clone(), config_path: dir.join("config.yaml"), ..GatewayConfig::default() };
        Settings {
            auth_dir: dir.to_string_lossy().into_owned(),
            api_keys: vec!["master".into()],
            scoped_api_keys: vec![ScopedApiKey { id: "key".into(), name: "key".into(), key_identifier: mahoquot_gateway::request_history::stable_key_identifier("secret"), key_prefix: "secret".into(), raw_key: Some("secret".into()), allowed_providers: vec![], allowed_accounts: vec![], allowed_models: vec![], token_limit: 1000, token_used: 0, is_active: true, created_at_ms: 0, expires_at_ms: None }],
            ..Settings::default()
        }.persist(&config.config_path).unwrap();
        let state = Arc::new(AppState::new(&config).unwrap());
        let executor = std::thread::current().id();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let entered_tx = Mutex::new(Some(entered_tx));
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        state.settings.add_observer(Arc::new(move |_| {
            let offloaded = std::thread::current().id() != executor;
            entered_tx.lock().unwrap().take().unwrap().send(offloaded).unwrap();
            // A broken synchronous handler must fail an assertion, not deadlock the runtime.
            if offloaded {
                release_rx.lock().unwrap().recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            }
        }));
        let app = create_app(state.clone());
        let request = Request::builder().method(method).uri(path).header("Authorization", "Bearer master").header("Content-Type", "application/json").body(Body::from(body)).unwrap();
        let mut write = tokio::spawn(app.clone().oneshot(request));
        let offloaded = tokio::select! {
            entered = tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx) => entered.unwrap().unwrap(),
            response = &mut write => panic!("{method} {path} returned {} before persistence", response.unwrap().unwrap().status()),
        };
        let probe = app.oneshot(Request::builder().uri("/v0/management/scoped-keys").header("Authorization", "Bearer master").body(Body::empty()).unwrap()).await.unwrap();
        if offloaded { release_tx.send(()).unwrap(); }
        let response = write.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(probe.status(), StatusCode::OK);
        drop(state);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(offloaded, "{method} {path} persisted on the single Tokio executor thread");
    }
}
