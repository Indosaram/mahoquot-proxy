use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use mahoquot_gateway::{
    config::GatewayConfig, management::settings::Settings, routes::create_app, state::AppState,
};
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tower::ServiceExt;

const KEY: &str = "config-review-management-secret";

struct Fixture {
    dir: PathBuf,
    state: Arc<AppState>,
    app: Router,
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("review-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("auth")).unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            format!(
            "logging-to-file: false\nproxy-url: direct\nremote-management:\n  secret-key: {KEY}\n"
        ),
        )
        .unwrap();
        let state = Arc::new(
            AppState::new(&GatewayConfig {
                auth_dir: dir.join("auth"),
                config_path: dir.join("config.yaml"),
                auth_refresh_enabled: false,
                ..Default::default()
            })
            .unwrap(),
        );
        Self {
            app: create_app(state.clone()),
            state,
            dir,
        }
    }

    async fn send(&self, method: &str, uri: &str, body: String) -> (StatusCode, String) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("x-management-key", KEY)
            .header("content-type", "application/json")
            .extension(ConnectInfo(
                "127.0.0.1:18840".parse::<std::net::SocketAddr>().unwrap(),
            ))
            .body(Body::from(body))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            let response = self.app.clone().oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (status, String::from_utf8(bytes.to_vec()).unwrap())
        })
        .await
        .expect("bounded management response")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).expect("owned fixture cleanup");
    }
}

async fn listener() -> tokio::net::TcpListener {
    for port in 18840..=18899 {
        match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => return listener,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => panic!("bind fixture: {error}"),
        }
    }
    panic!("no fixture port available")
}

#[tokio::test]
async fn api_call_rejects_external_credential_paths() {
    let fixture = Fixture::new();
    let outside = fixture.dir.join("outside.json");
    std::fs::write(&outside, r#"{"access_token":"external-secret"}"#).unwrap();
    std::fs::write(
        fixture.dir.join("auth/valid.json"),
        r#"{"access_token":"owned-secret"}"#,
    )
    .unwrap();
    let listener = listener().await;
    let url = format!("http://{}/capture", listener.local_addr().unwrap());
    let mut invalid = vec![
        outside.to_string_lossy().into_owned(),
        "../outside.json".into(),
        "missing.json".into(),
    ];
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, fixture.dir.join("auth/escape.json")).unwrap();
        invalid.push("escape.json".into());
    }
    for endpoint in [
        "/v0/management/api-call",
        "/v8/management/requests/api-call",
    ] {
        for index in &invalid {
            let (status, _) = fixture
                .send(
                    "POST",
                    endpoint,
                    json!({
                        "method":"POST", "url":url, "auth_index":index, "data":"$TOKEN$"
                    })
                    .to_string(),
                )
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{endpoint}: {index}");
        }
    }
    let listener = listener.into_std().unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let app = Router::new().route(
        "/capture",
        axum::routing::post(move |body: String| {
            let tx = tx.clone();
            async move {
                tx.send(body).unwrap();
                "received"
            }
        }),
    );
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                stopped.await.unwrap();
            })
            .await
            .unwrap();
    });
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    "valid.json".hash(&mut hash);
    for index in ["valid.json".to_string(), format!("{:016x}", hash.finish())] {
        let (status, _) = fixture
            .send(
                "POST",
                "/v8/management/requests/api-call",
                json!({
                    "method":"POST", "url":url, "auth_index":index, "data":"$TOKEN$"
                })
                .to_string(),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap(),
            "owned-secret"
        );
    }
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn v8_config_withholds_management_secret_and_roundtrips() {
    let fixture = Fixture::new();
    for path in [
        "/v8/management/config",
        "/v8/management/config/management",
        "/v8/management/config.yaml",
    ] {
        let (status, body) = fixture.send("GET", path, String::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.contains(KEY));
        if path != "/v8/management/config/management" {
            let (status, body) = fixture.send("PUT", path, body).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(
                fixture
                    .state
                    .settings
                    .current()
                    .remote_management
                    .secret_key,
                KEY
            );
            assert_eq!(
                Settings::load(&fixture.dir.join("config.yaml"))
                    .unwrap()
                    .remote_management
                    .secret_key,
                KEY
            );
        }
    }
    let (status, body) = fixture
        .send(
            "GET",
            "/v8/management/config/management/secret-key",
            String::new(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!body.contains(KEY));
}

#[tokio::test]
async fn v8_edit_preserves_profile_on_reload() {
    let fixture = Fixture::new();
    let input = json!({"compat":{"flag":true},
        "models":[{"id":"fixture","contextWindow":200000}],"remove":["retired"]});
    let request = Request::put("/v0/management/omo-profile")
        .header("x-management-key", KEY)
        .header("content-type", "application/json")
        .header("if-match", "\"r0\"")
        .extension(ConnectInfo(
            "127.0.0.1:18840".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::from(input.to_string()))
        .unwrap();
    let response =
        tokio::time::timeout(Duration::from_secs(5), fixture.app.clone().oneshot(request))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let profile = serde_json::to_value(
        fixture
            .state
            .settings
            .current()
            .omo_profile
            .as_ref()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(profile["revision"], 1);
    let (status, body) = fixture
        .send(
            "PUT",
            "/v8/management/config/observability/logs/debug",
            "true".into(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let reloaded = Settings::load(&fixture.dir.join("config.yaml")).unwrap();
    assert_eq!(
        serde_json::to_value(reloaded.omo_profile.unwrap()).unwrap(),
        profile
    );
    let (status, body) = fixture
        .send("GET", "/v8/management/config/omo-profile", String::new())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), profile);
    let before = std::fs::read(fixture.dir.join("config.yaml")).unwrap();
    let (status, _) = fixture
        .send(
            "PUT",
            "/v8/management/config/omo-profile",
            json!({"revision":"invalid"}).to_string(),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        std::fs::read(fixture.dir.join("config.yaml")).unwrap(),
        before
    );
}

#[tokio::test]
async fn v8_write_publishes_proxy_and_registry() {
    let fixture = Fixture::new();
    let a = listener().await;
    let b = listener().await;
    let a_url = format!("http://{}", a.local_addr().unwrap());
    let b_url = format!("http://{}", b.local_addr().unwrap());
    fixture
        .state
        .settings
        .mutate(|s| s.proxy_url = a_url)
        .unwrap();
    let (status, body) = fixture
        .send(
            "PUT",
            "/v8/management/config/requests/proxy-url",
            json!(b_url).to_string(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut stream, _) = b.accept().await.unwrap();
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            bytes.push(stream.read_u8().await.unwrap());
            assert!(bytes.len() < 8192);
        }
        assert!(String::from_utf8(bytes)
            .unwrap()
            .starts_with("GET http://127.0.0.1:18899/probe "));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nB")
            .await
            .unwrap();
    });
    let response = fixture
        .state
        .default_client()
        .get("http://127.0.0.1:18899/probe")
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "B");
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        a.into_std().unwrap().accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let target = fixture
        .state
        .pool
        .load()
        .registry
        .models()
        .keys()
        .next()
        .unwrap()
        .clone();
    let alias = mahoquot_registry::ModelId::new("review-config-alias").unwrap();
    let (status, body) = fixture
        .send(
            "PUT",
            "/v8/management/config/oauth/model-alias",
            json!({
                "openai":[{"name":target.as_str(),"alias":alias.as_str()}]
            })
            .to_string(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        fixture
            .state
            .pool
            .load()
            .registry
            .resolve_alias(&alias)
            .unwrap(),
        target
    );
}
