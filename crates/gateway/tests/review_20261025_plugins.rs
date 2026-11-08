use mahoquot_gateway::plugins::store::PluginStore;
use serde_json::json;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct TempRoot(PathBuf);
impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("review-plugins-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for TempRoot {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn deletion_preserves_adjacent_ids_and_reports_removal_failure() {
    let root = TempRoot::new();
    let store = PluginStore::new(root.0.clone(), None);
    for name in [
        "foo.so",
        "foo-v1.dylib",
        "foo.config.json",
        "foobar.so",
        "foobar.config.json",
        "foo-extra.dll",
        "foo.notes",
    ] {
        std::fs::write(root.0.join(name), name).unwrap();
    }
    assert!(store.delete_plugin("foo").unwrap());
    for name in ["foo.so", "foo-v1.dylib", "foo.config.json"] {
        assert!(!root.0.join(name).exists());
    }
    for name in [
        "foobar.so",
        "foobar.config.json",
        "foo-extra.dll",
        "foo.notes",
    ] {
        assert_eq!(std::fs::read(root.0.join(name)).unwrap(), name.as_bytes());
    }
    std::fs::create_dir(root.0.join("foo.config.json")).unwrap();
    assert_eq!(store.delete_plugin("foo").unwrap_err().code, "fs_error");
}

#[tokio::test]
async fn requested_version_must_match_before_artifact_download() {
    let root = TempRoot::new();
    let store = PluginStore::new(root.0.clone(), None);
    let mut listener = None;
    for port in 18840..=18899 {
        match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            Ok(bound) => {
                listener = Some(bound);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {}
            Err(error) => panic!("bind fixture: {error}"),
        }
    }
    let listener = listener.expect("fixture port available");
    let address = listener.local_addr().unwrap();
    let downloads = Arc::new(AtomicUsize::new(0));
    let counter = downloads.clone();
    let registry = json!({"plugins": [{"id": "fixture", "version": "2", "install": {"artifacts": [{"url": format!("http://{address}/artifact")}]}}]});
    let app = axum::Router::new()
        .route(
            "/registry",
            axum::routing::get(move || {
                let registry = registry.clone();
                async move { axum::Json(registry) }
            }),
        )
        .route(
            "/artifact",
            axum::routing::get(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                async { "version-two" }
            }),
        );
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    let url = format!("http://{address}/registry");
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        store.install_from_registry("fixture", "1", &url, Some("direct")),
    )
    .await
    .unwrap();
    assert_eq!(result.unwrap_err().code, "version_unavailable");
    assert_eq!(downloads.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read_dir(&root.0).unwrap().count(), 0);
    let installed = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        store.install_from_registry("fixture", "2", &url, Some("direct")),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(installed.version, "2");
    assert_eq!(std::fs::read(installed.path).unwrap(), b"version-two");
    assert_eq!(downloads.load(Ordering::SeqCst), 1);
    stop.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn management_config_and_enable_surface_persistence_failures() {
    use axum::{
        body::Body,
        extract::ConnectInfo,
        http::{Request, StatusCode},
    };
    use mahoquot_gateway::{config::GatewayConfig, routes::create_app, state::AppState};
    use tower::ServiceExt;
    let root = TempRoot::new();
    let plugins = root.0.join("plugins");
    std::fs::create_dir(&plugins).unwrap();
    std::fs::create_dir(root.0.join("auth")).unwrap();
    let config = root.0.join("config.yaml");
    std::fs::write(&config, format!("logging-to-file: false\nproxy-url: direct\nremote-management:\n  secret-key: plugins-fixture-key\nplugins:\n  dir: {}\n", plugins.display())).unwrap();
    let state = Arc::new(
        AppState::new(&GatewayConfig {
            config_path: config,
            auth_dir: root.0.join("auth"),
            auth_refresh_enabled: false,
            ..Default::default()
        })
        .unwrap(),
    );
    std::fs::write(plugins.join("fixture.so"), b"not loaded").unwrap();
    let config_path = plugins.join("fixture.config.json");
    std::fs::create_dir(&config_path).unwrap();
    let app = create_app(state);
    for (method, route, body) in [
        ("PUT", "config", r#"{"config":{"x":1}}"#),
        ("PATCH", "enabled", r#"{"enabled":false}"#),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(format!("/v0/management/plugins/fixture/{route}"))
            .header("x-management-key", "plugins-fixture-key")
            .header("content-type", "application/json")
            .extension(ConnectInfo(
                "127.0.0.1:18840".parse::<std::net::SocketAddr>().unwrap(),
            ))
            .body(Body::from(body))
            .unwrap();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            app.clone().oneshot(request),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(config_path.is_dir());
    }
}
