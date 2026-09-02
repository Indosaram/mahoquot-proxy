mod common;

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::routing::post;
use axum::Router;
use common::{codex_sse, create_auth_file_json, unique_temp_dir, CODEX_PATH, OPENAI_REQUEST};
use http_body_util::BodyExt;
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::inbound::ApiKeys;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use mahoquot_gateway::usage::{AccountUsage, QuotaWindow};
use mahoquot_types::Strategy;
use tower::ServiceExt;

static NEXT_PORT: AtomicU16 = AtomicU16::new(18840);
const KEY: &str = "scheduler-test-key";

async fn bind_test_listener() -> tokio::net::TcpListener {
    for _ in 18840..=18899 {
        let port = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
        let port = 18840 + (port - 18840) % 60;
        if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            return listener;
        }
    }
    panic!("no free scheduler test port in 18840-18899");
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn usage(remaining_percent: u8, reset_at_unix: i64) -> AccountUsage {
    AccountUsage {
        primary: QuotaWindow {
            used_percent: Some(100.0 - f64::from(remaining_percent)),
            reset_at_unix: Some(reset_at_unix),
            ..QuotaWindow::default()
        },
        observed_at_unix: Some(now_unix()),
        ..AccountUsage::default()
    }
}

async fn spawn_upstream(text: &'static str) -> (String, tokio::task::JoinHandle<()>) {
    let listener = bind_test_listener().await;
    let address = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        CODEX_PATH,
        post(move || async move {
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream")],
                codex_sse(text),
            )
        }),
    );
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (address, task)
}

fn write_account(auth_dir: &std::path::Path, id: &str, upstream: &str) {
    let raw = create_auth_file_json(
        id,
        &format!("account-{id}"),
        &format!("token-{id}"),
        Some(upstream),
    );
    std::fs::write(auth_dir.join(format!("codex-{id}.json")), raw).unwrap();
}

fn config(auth_dir: std::path::PathBuf, strategy: Strategy) -> GatewayConfig {
    GatewayConfig {
        auth_dir: auth_dir.clone(),
        config_path: auth_dir.join("config.yaml"),
        api_keys: ApiKeys::new(vec![KEY.to_string()]),
        strategy,
        max_failover: 3,
        auth_refresh_enabled: false,
        ..GatewayConfig::default()
    }
}

async fn response_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("json response")
}

async fn management(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> axum::response::Response {
    let mut builder = Request::builder()
        .method(method)
        .uri(format!("/v0/management{path}"))
        .header(header::AUTHORIZATION, format!("Bearer {KEY}"));
    let body = match body {
        Some(value) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    app.clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap()
}

async fn relay(app: &axum::Router) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(OPENAI_REQUEST))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}

#[tokio::test]
async fn scheduler_rotates_two_account_pool_by_reset_time() {
    let (upstream_a, task_a) = spawn_upstream("from-a").await;
    let (upstream_b, task_b) = spawn_upstream("from-b").await;
    let auth_dir = unique_temp_dir("mahoquot-scheduler-rotation");
    write_account(&auth_dir, "a", &upstream_a);
    write_account(&auth_dir, "b", &upstream_b);
    let state = Arc::new(AppState::new(&config(auth_dir.clone(), Strategy::FillFirst)).unwrap());
    let app = create_app(Arc::clone(&state));

    let now = now_unix();
    state
        .find_member("a")
        .unwrap()
        .set_usage(usage(60, now + 600));
    state
        .find_member("b")
        .unwrap()
        .set_usage(usage(60, now + 3_600));
    assert_eq!(
        management(
            &app,
            "PUT",
            "/scheduler/settings",
            Some(serde_json::json!({"enabled": true}))
        )
        .await
        .status(),
        StatusCode::OK
    );

    let status = response_json(management(&app, "GET", "/scheduler/status", None).await).await;
    let order = response_json(management(&app, "GET", "/scheduler/order", None).await).await;
    assert_eq!(status["selected"], "a");
    assert_eq!(order["order"], serde_json::json!(["a", "b"]));
    assert!(relay(&app).await.contains("from-a"));

    state
        .find_member("a")
        .unwrap()
        .set_usage(usage(60, now + 7_200));
    state
        .find_member("b")
        .unwrap()
        .set_usage(usage(60, now + 300));
    assert_eq!(
        management(
            &app,
            "PUT",
            "/scheduler/settings",
            Some(serde_json::json!({"enabled": true}))
        )
        .await
        .status(),
        StatusCode::OK
    );
    let status = response_json(management(&app, "GET", "/scheduler/status", None).await).await;
    assert_eq!(status["selected"], "b");
    assert!(relay(&app).await.contains("from-b"));

    task_a.abort();
    task_b.abort();
    std::fs::remove_dir_all(auth_dir).ok();
}

#[tokio::test]
async fn corrupt_scheduler_state_fails_open_without_auth_mutation() {
    let (upstream_a, task_a) = spawn_upstream("from-a").await;
    let (upstream_b, task_b) = spawn_upstream("from-b").await;
    let auth_dir = unique_temp_dir("mahoquot-scheduler-corrupt");
    write_account(&auth_dir, "a", &upstream_a);
    write_account(&auth_dir, "b", &upstream_b);
    std::fs::write(
        auth_dir.join("scheduler-settings.json"),
        r#"{"enabled":true}"#,
    )
    .unwrap();
    std::fs::write(auth_dir.join("scheduler-state.json"), b"{not-json").unwrap();
    let auth_a_before = std::fs::read(auth_dir.join("codex-a.json")).unwrap();
    let auth_b_before = std::fs::read(auth_dir.join("codex-b.json")).unwrap();

    let state =
        Arc::new(AppState::new(&config(auth_dir.clone(), Strategy::StrictRoundRobin)).unwrap());
    let app = create_app(state);
    let first = relay(&app).await;
    let second = relay(&app).await;
    assert!(first.contains("from-a") || first.contains("from-b"));
    assert!(second.contains("from-a") || second.contains("from-b"));
    assert_ne!(first.contains("from-a"), second.contains("from-a"));
    let status = response_json(management(&app, "GET", "/scheduler/status", None).await).await;
    assert_eq!(status["fail_open"], true);
    assert_eq!(
        std::fs::read(auth_dir.join("codex-a.json")).unwrap(),
        auth_a_before
    );
    assert_eq!(
        std::fs::read(auth_dir.join("codex-b.json")).unwrap(),
        auth_b_before
    );

    task_a.abort();
    task_b.abort();
    std::fs::remove_dir_all(auth_dir).ok();
}

#[tokio::test]
async fn all_exhausted_restores_base_strategy() {
    let (upstream_a, task_a) = spawn_upstream("from-a").await;
    let (upstream_b, task_b) = spawn_upstream("from-b").await;
    let auth_dir = unique_temp_dir("mahoquot-scheduler-exhausted");
    write_account(&auth_dir, "a", &upstream_a);
    write_account(&auth_dir, "b", &upstream_b);
    let state =
        Arc::new(AppState::new(&config(auth_dir.clone(), Strategy::StrictRoundRobin)).unwrap());
    let app = create_app(Arc::clone(&state));
    let now = now_unix();
    state
        .find_member("a")
        .unwrap()
        .set_usage(usage(0, now + 600));
    state
        .find_member("b")
        .unwrap()
        .set_usage(usage(0, now + 900));
    management(
        &app,
        "PUT",
        "/scheduler/settings",
        Some(serde_json::json!({"enabled": true})),
    )
    .await;

    let status = response_json(management(&app, "GET", "/scheduler/status", None).await).await;
    assert_eq!(status["selected"], serde_json::Value::Null);
    assert_eq!(status["fail_open"], true);
    let first = relay(&app).await;
    let second = relay(&app).await;
    assert_ne!(first.contains("from-a"), second.contains("from-a"));

    task_a.abort();
    task_b.abort();
    std::fs::remove_dir_all(auth_dir).ok();
}

#[tokio::test]
async fn scheduler_parking_only_filters_future_selections_and_in_flight_arc_completes() {
    let started = Arc::new(tokio::sync::Notify::new());
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let release = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
    let listener_a = bind_test_listener().await;
    let upstream_a = format!("http://{}", listener_a.local_addr().unwrap());
    let app_a = Router::new().route(
        CODEX_PATH,
        post({
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            move || {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                async move {
                    started.notify_one();
                    if let Some(rx) = release.lock().await.take() {
                        let _ = rx.await;
                    }
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        codex_sse("in-flight-a"),
                    )
                }
            }
        }),
    );
    let task_a = tokio::spawn(async move { axum::serve(listener_a, app_a).await.unwrap() });
    let (upstream_b, task_b) = spawn_upstream("future-b").await;

    let auth_dir = unique_temp_dir("mahoquot-scheduler-in-flight");
    write_account(&auth_dir, "a", &upstream_a);
    write_account(&auth_dir, "b", &upstream_b);
    let state = Arc::new(AppState::new(&config(auth_dir.clone(), Strategy::FillFirst)).unwrap());
    let app = create_app(Arc::clone(&state));
    let now = now_unix();
    state
        .find_member("a")
        .unwrap()
        .set_usage(usage(60, now + 300));
    state
        .find_member("b")
        .unwrap()
        .set_usage(usage(60, now + 3_600));
    management(
        &app,
        "PUT",
        "/scheduler/settings",
        Some(serde_json::json!({"enabled": true})),
    )
    .await;

    let in_flight_app = app.clone();
    let in_flight = tokio::spawn(async move { relay(&in_flight_app).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
        .await
        .expect("account a request started");

    state
        .find_member("a")
        .unwrap()
        .set_usage(usage(60, now + 7_200));
    state
        .find_member("b")
        .unwrap()
        .set_usage(usage(60, now + 120));
    management(
        &app,
        "PUT",
        "/scheduler/settings",
        Some(serde_json::json!({"enabled": true})),
    )
    .await;
    assert!(relay(&app).await.contains("future-b"));

    release_tx.send(()).unwrap();
    let completed = tokio::time::timeout(std::time::Duration::from_secs(2), in_flight)
        .await
        .expect("in-flight request completes")
        .unwrap();
    assert!(completed.contains("in-flight-a"));

    task_a.abort();
    task_b.abort();
    std::fs::remove_dir_all(auth_dir).ok();
}
