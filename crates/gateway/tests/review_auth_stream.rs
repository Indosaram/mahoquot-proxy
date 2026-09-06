mod common;

use std::sync::Arc;
use axum::{body::Body, http::{Request, StatusCode}, response::Response, routing::post, Router};
use bytes::Bytes;
use http_body_util::BodyExt;
use mahoquot_gateway::{config::GatewayConfig, routes::create_app, state::AppState};
use tower::ServiceExt;

struct Cleanup {
    server: tokio::task::JoinHandle<()>,
    dir: std::path::PathBuf,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        self.server.abort();
        std::fs::remove_dir_all(&self.dir).unwrap();
    }
}

async fn exercise_stream(drop_early: bool) {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(2);
    let rx = Arc::new(tokio::sync::Mutex::new(Some(rx)));
    let upstream = Router::new().route(common::CODEX_PATH, post(move || {
        let rx = rx.clone();
        async move {
            let rx = rx.lock().await.take().unwrap();
            Response::builder().header("content-type", "text/event-stream")
                .body(Body::from_stream(futures::stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|item| (item, rx))
                }))).unwrap()
        }
    }));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap(); });
    let dir = common::unique_temp_dir("review-auth-stream");
    let url = format!("http://127.0.0.1:{port}");
    let mut credential: serde_json::Value = serde_json::from_str(&common::create_auth_file_json("stream", "stream", "mock", Some(&url))).unwrap();
    credential["usage_override"] = serde_json::json!(url);
    std::fs::write(dir.join("codex-stream.json"), credential.to_string()).unwrap();
    let config_path = dir.join("config.yaml");
    std::fs::write(&config_path, "logging-to-file: false\n").unwrap();
    let cleanup = Cleanup {server, dir: dir.clone()};
    let state = Arc::new(AppState::new(&GatewayConfig {auth_dir:dir, config_path, auth_refresh_enabled:false, ..GatewayConfig::default()}).unwrap());
    tx.send(Ok(Bytes::from_static(b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n"))).await.unwrap();
    let response = create_app(state.clone()).oneshot(Request::post(common::CODEX_PATH)
        .header("content-type", "application/json")
        .body(Body::from(r#"{"model":"gpt-5.6-sol","stream":true,"input":"hi"}"#)).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), body.frame()).await.unwrap().unwrap().unwrap();
    assert!(!first.into_data().unwrap().is_empty());
    assert_eq!(state.monitor.in_flight(), 1, "stream remains in flight after first frame");
    if drop_early {
        drop(body);
    } else {
        drop(tx);
        tokio::time::timeout(std::time::Duration::from_secs(5), body.collect()).await.unwrap().unwrap();
    }
    assert_eq!(state.monitor.in_flight(), 0, "EOF/drop releases guard synchronously");
    cleanup.server.abort();
}

#[tokio::test]
async fn streaming_guard_lasts_until_eof() { exercise_stream(false).await; }

#[tokio::test]
async fn streaming_guard_releases_on_client_drop() { exercise_stream(true).await; }
