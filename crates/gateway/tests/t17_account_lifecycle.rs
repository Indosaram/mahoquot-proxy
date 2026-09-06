mod common;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use mahoquot_types::{Health, PoolMember};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::inbound::ApiKeys;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use tower::ServiceExt;

fn config(auth_dir: std::path::PathBuf) -> GatewayConfig {
    GatewayConfig {
        auth_dir: auth_dir.clone(),
        api_keys: ApiKeys::new(vec!["lifecycle-key".to_string()]),
        config_path: auth_dir.join("config.yaml"),
        auth_refresh_enabled: false,
        ..GatewayConfig::default()
    }
}

fn codex_credential() -> serde_json::Value {
    serde_json::json!({
        "type": "codex",
        "identity_slug": "toggle-me",
        "access_token": "access",
        "refresh_token": "refresh",
        "account_id": "account",
        "email": "toggle@example.test",
        "expired": "2030-01-01T00:00:00Z",
        "id_token": "id",
        "last_refresh": "2026-01-01T00:00:00Z",
        "disabled": false
    })
}

async fn json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("json response")
}

async fn stats(app: &axum::Router) -> serde_json::Value {
    json(
        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/stats")
                    .header(header::AUTHORIZATION, "Bearer lifecycle-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await
}

#[tokio::test]
async fn disabled_credentials_preserve_counters_but_cannot_route_or_advertise_models() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let recorded = calls.clone();
    let upstream = axum::Router::new().route(common::CODEX_PATH, axum::routing::post(move || {
        recorded.fetch_add(1, Ordering::SeqCst);
        async { ([(header::CONTENT_TYPE, "text/event-stream")], common::codex_sse("fixture")) }
    }));
    let task = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let auth_dir = std::env::temp_dir().join(format!("quotio-lifecycle-{}", std::process::id()));
    std::fs::create_dir_all(&auth_dir).expect("auth dir");
    let mut credential = codex_credential();
    credential["upstream_override"] = serde_json::json!(base);
    credential["usage_override"] = serde_json::json!(base);
    std::fs::write(
        auth_dir.join("codex-toggle.json"),
        serde_json::to_vec_pretty(&credential).unwrap(),
    )
    .expect("credential");
    let state = Arc::new(
        AppState::new(&config(auth_dir.clone())).expect("state"),
    );
    let app = create_app(state.clone());
    state.find_member("toggle-me").unwrap().ok_count.store(9, Ordering::Relaxed);
    assert_eq!(stats(&app).await["accounts"].as_array().unwrap().len(), 1);

    let disable = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/v0/management/auth-files/status")
                .header(header::AUTHORIZATION, "Bearer lifecycle-key")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"name":"codex-toggle.json","disabled":true}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(disable.status(), StatusCode::OK);
    assert_eq!(stats(&app).await["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(state.find_member("toggle-me").unwrap().health(), Health::Disabled);
    assert_eq!(state.find_member("toggle-me").unwrap().ok_count.load(Ordering::Relaxed), 9);
    assert!(state.scheduler.snapshot().order.is_empty());
    let models = app.clone().oneshot(Request::builder().uri("/v1/models")
        .header(header::AUTHORIZATION, "Bearer lifecycle-key").body(Body::empty()).unwrap()).await.unwrap();
    assert!(!json(models).await["data"].as_array().unwrap().iter().any(|m| m["id"] == "gpt-5.6-sol"));
    let chat = || Request::builder().method("POST").uri("/v1/chat/completions")
        .header(header::AUTHORIZATION, "Bearer lifecycle-key").header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"model":"gpt-5.6-sol","stream":true,"messages":[{"role":"user","content":"fixture"}]}"#)).unwrap();
    let rejected = app.clone().oneshot(chat()).await.unwrap();
    assert!(matches!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE | StatusCode::BAD_REQUEST));
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let listed = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v0/management/auth-files")
                .header(header::AUTHORIZATION, "Bearer lifecycle-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(json(listed).await["files"][0]["disabled"], true);

    let enable = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/v0/management/auth-files/status")
                .header(header::AUTHORIZATION, "Bearer lifecycle-key")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"name":"codex-toggle.json","disabled":false}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(enable.status(), StatusCode::OK);
    assert_eq!(stats(&app).await["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(state.find_member("toggle-me").unwrap().health(), Health::Available);
    assert_eq!(state.find_member("toggle-me").unwrap().ok_count.load(Ordering::Relaxed), 9);
    let models = app.clone().oneshot(Request::builder().uri("/v1/models")
        .header(header::AUTHORIZATION, "Bearer lifecycle-key").body(Body::empty()).unwrap()).await.unwrap();
    assert!(json(models).await["data"].as_array().unwrap().iter().any(|m| m["id"] == "gpt-5.6-sol"));
    let routed = app.clone().oneshot(chat()).await.unwrap();
    assert_eq!(routed.status(), StatusCode::OK);
    routed.into_body().collect().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    std::fs::remove_dir_all(auth_dir).ok();
}

#[tokio::test]
async fn generic_openai_key_provider_joins_pool() {
    let auth_dir = std::env::temp_dir().join(format!("quotio-generic-{}", std::process::id()));
    std::fs::create_dir_all(&auth_dir).expect("auth dir");
    let app = create_app(Arc::new(
        AppState::new(&config(auth_dir.clone())).expect("state"),
    ));
    let create = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v0/management/auth-files")
                .header(header::AUTHORIZATION, "Bearer lifecycle-key")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "name": "deepseek-primary.json",
                        "content": {
                            "type": "generic",
                            "provider": "deepseek",
                            "label": "DeepSeek primary",
                            "adapter": "openai-chat",
                            "base_url": "https://api.deepseek.com",
                            "api_key": "secret",
                            "models": ["deepseek-chat", "deepseek-reasoner"],
                            "disabled": false
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create.status(), StatusCode::OK);
    let account_stats = stats(&app).await;
    let accounts = account_stats["accounts"].as_array().unwrap();
    assert_eq!(
        accounts.len(),
        1,
        "generic credential must join pool: {account_stats}"
    );
    assert_eq!(accounts[0]["provider"], "deepseek");
    std::fs::remove_dir_all(auth_dir).ok();
}

#[tokio::test]
async fn vertex_import_exchanges_service_account_and_joins_google_pool() {
    let token_app = axum::Router::new().route(
        "/token",
        axum::routing::post(|body: String| async move {
            assert!(
                body.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer")
            );
            assert!(body.contains("assertion="));
            axum::Json(serde_json::json!({"access_token":"vertex-access","expires_in":3600}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let token_uri = format!("http://{}/token", listener.local_addr().unwrap());
    let token_task = tokio::spawn(async move { axum::serve(listener, token_app).await.unwrap() });
    let auth_dir = std::env::temp_dir().join(format!("quotio-vertex-{}", std::process::id()));
    std::fs::remove_dir_all(&auth_dir).ok();
    std::fs::create_dir_all(&auth_dir).unwrap();
    let app = create_app(Arc::new(AppState::new(&config(auth_dir.clone())).unwrap()));
    let private_key = include_str!("fixtures/test-rsa-private.pem");
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v0/management/vertex/import")
                .header(header::AUTHORIZATION, "Bearer lifecycle-key")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"file":serde_json::json!({
            "type":"service_account","project_id":"project-1","private_key":private_key,
            "client_email":"service@project-1.iam.gserviceaccount.com","token_uri":token_uri
        }).to_string()})
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = json(response).await;
    assert_eq!(status, StatusCode::OK, "response: {body}");
    let accounts = stats(&app).await;
    assert_eq!(accounts["accounts"][0]["provider"], "google-vertex");
    token_task.abort();
    assert!(token_task.await.unwrap_err().is_cancelled());
    std::fs::remove_dir_all(auth_dir).ok();
}
