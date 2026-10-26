use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use mahoquot_gateway::{
    config::GatewayConfig, routes::create_app, state::AppState, tui::ManagementClient,
};

struct TempRoot(PathBuf);

impl Drop for TempRoot {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("remove owned fixture root");
    }
}

#[tokio::test]
async fn tui_client_reads_gateway_log_records() {
    let dir = TempRoot(
        std::env::temp_dir().join(format!("review-discovery-tui-{}", uuid::Uuid::new_v4())),
    );
    let auth_dir = dir.0.join("auth");
    std::fs::create_dir_all(&auth_dir).unwrap();
    let config_path = dir.0.join("config.yaml");
    std::fs::write(
        &config_path,
        "logging-to-file: false\nremote-management:\n  secret-key: discovery-tui-fixture\n",
    )
    .unwrap();
    let config = GatewayConfig {
        auth_dir,
        config_path,
        auth_refresh_enabled: false,
        ..GatewayConfig::default()
    };
    let state = Arc::new(AppState::new(&config).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:18887")
        .await
        .unwrap();
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let app = create_app(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = stopped.await;
        })
        .await
        .unwrap();
    });
    let client = ManagementClient::new(
        "http://127.0.0.1:18887".into(),
        "discovery-tui-fixture".into(),
    );
    let empty = client.get_logs().await;
    state.log_tail.push("fixture café".into());
    let record = serde_json::json!({
        "kind": "request", "timestamp": 7, "provider": "fixture",
        "model": "local-model", "status": 200, "success": true
    });
    state.log_tail.push(record.to_string());
    let logs = client.get_logs().await;
    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert!(empty.unwrap().is_empty());
    let logs = logs.unwrap();
    assert_eq!(logs.len(), 2);
    assert_eq!(logs[0], "fixture café");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&logs[1]).unwrap(),
        record
    );
}
