use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
    routing::post,
    Router,
};
use futures::{SinkExt, StreamExt};
use mahoquot_gateway::{
    config::GatewayConfig,
    management::settings::{RemoteManagement, Settings},
    routes::create_app,
    state::AppState,
};
use serde_json::json;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tower::ServiceExt;

struct Temp(std::path::PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("remove owned test root");
    }
}
fn setup() -> (Temp, Arc<AppState>) {
    assert!(
        std::env::var("MANAGEMENT_PASSWORD")
            .unwrap_or_default()
            .is_empty(),
        "run isolated from management password overrides"
    );
    let temp = Temp(std::env::temp_dir().join(format!("worker-auth-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir_all(&temp.0).unwrap();
    let config = GatewayConfig {
        auth_dir: temp.0.clone(),
        config_path: temp.0.join("config.yaml"),
        auth_refresh_enabled: false,
        ..Default::default()
    };
    Settings {
        api_keys: vec!["master".into()],
        remote_management: RemoteManagement { secret_key: "management".into(), allow_remote: false, ..Default::default() },
        auth_dir: temp.0.to_string_lossy().into_owned(),
        scoped_api_keys: vec![serde_json::from_value(json!({"id":"scoped","name":"scoped","key_identifier":mahoquot_gateway::request_history::stable_key_identifier("scoped"),"key_prefix":"scoped","allowed_providers":["codex"]})).unwrap()],
        credit_codex_account_ids: vec!["existing".into()],
        ..Default::default()
    }.persist(&config.config_path).unwrap();
    let state = Arc::new(AppState::new(&config).unwrap());
    (temp, state)
}
async fn listener() -> tokio::net::TcpListener {
    for port in 18840..=18899 {
        match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            Ok(l) => return l,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(e) => panic!("{e}"),
        }
    }
    panic!("fixture ports exhausted")
}
async fn call(
    app: &Router,
    path: &str,
    method: &str,
    key: Option<&str>,
    peer: &str,
    body: String,
) -> StatusCode {
    let mut req = Request::builder()
        .uri(path)
        .method(method)
        .header("content-type", "application/json");
    if let Some(key) = key {
        req = req.header("authorization", format!("Bearer {key}"));
    }
    let mut req = req.body(Body::from(body)).unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(peer.parse::<std::net::SocketAddr>().unwrap()));
    tokio::time::timeout(Duration::from_secs(5), app.clone().oneshot(req))
        .await
        .unwrap()
        .unwrap()
        .status()
}

#[tokio::test]
async fn review_management_alias_and_meta_are_gated_before_side_effects() {
    let (temp, state) = setup();
    let app = create_app(state.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let socket = listener().await;
    let base = format!("http://{}", socket.local_addr().unwrap());
    let fixture = Router::new().route("/device", post(move || {
        let count = count.clone();
        async move { count.fetch_add(1, Ordering::SeqCst); axum::Json(json!({"device_code":"fixture","user_code":"fixture","verification_uri":"http://127.0.0.1/unused","expires_in":600,"interval":1})) }
    })).route("/token", post({ let calls = calls.clone(); move || { let calls = calls.clone(); async move { calls.fetch_add(1, Ordering::SeqCst); (StatusCode::BAD_REQUEST, axum::Json(json!({"error":"authorization_pending"}))) } } }));
    let server = tokio::spawn(async move { axum::serve(socket, fixture).await.unwrap() });
    let path = format!(
        "/v0/management/meta-auth-url?device_url={}",
        urlencoding::encode(&format!("{base}/device"))
    );
    let complete = json!({"device_code":"fixture","token_url":format!("{base}/token")}).to_string();
    let original = std::fs::read(temp.0.join("config.yaml")).unwrap();
    let inventory = std::fs::read_dir(&temp.0).unwrap().count();
    for (index, key) in [None, Some("master"), Some("scoped")]
        .into_iter()
        .enumerate()
    {
        let peer = format!("127.0.0.1:{}", 18840 + index);
        *state.api_keys.management_attempts().lock().unwrap() = Default::default();
        for (route, method, body) in [
            (path.as_str(), "GET", String::new()),
            (
                "/v0/management/meta-auth-complete",
                "POST",
                complete.clone(),
            ),
            ("/api/codex-auth/accounts/credits", "GET", String::new()),
            (
                "/api/codex-auth/accounts/credits",
                "PUT",
                json!({"all":false}).to_string(),
            ),
        ] {
            let status = call(&app, route, method, key, &peer, body).await;
            assert!(status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            state.settings.current().credit_codex_account_ids,
            vec!["existing"]
        );
        assert_eq!(std::fs::read(temp.0.join("config.yaml")).unwrap(), original);
        assert_eq!(std::fs::read_dir(&temp.0).unwrap().count(), inventory);
    }
    assert_eq!(
        call(
            &app,
            &path,
            "GET",
            Some("management"),
            "192.0.2.1:18840",
            String::new()
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    *state.api_keys.management_attempts().lock().unwrap() = Default::default();
    assert_eq!(
        call(
            &app,
            &path,
            "GET",
            Some("management"),
            "127.0.0.1:18840",
            String::new()
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &app,
            "/v0/management/meta-auth-complete",
            "POST",
            Some("management"),
            "127.0.0.1:18840",
            complete
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        call(
            &app,
            "/api/codex-auth/accounts/credits",
            "PUT",
            Some("management"),
            "127.0.0.1:18840",
            json!({"all":false}).to_string()
        )
        .await,
        StatusCode::OK
    );
    assert!(state.settings.current().credit_codex_account_ids.is_empty());
    assert_ne!(std::fs::read(temp.0.join("config.yaml")).unwrap(), original);
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn review_scoped_websocket_cannot_register_worker() {
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Error, Message};
    let (_temp, state) = setup();
    let socket = listener().await;
    let addr = socket.local_addr().unwrap();
    let app = create_app(state.clone());
    let server = tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    for key in ["scoped", "master"] {
        let mut req = format!("ws://{addr}/v1/ws").into_client_request().unwrap();
        req.headers_mut()
            .insert("authorization", format!("Bearer {key}").parse().unwrap());
        req.headers_mut().insert(
            "x-aistudio-worker-id",
            "aistudio-worker-review".parse().unwrap(),
        );
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            tokio_tungstenite::client_async(req, tcp),
        )
        .await
        .unwrap();
        if key == "scoped" {
            assert!(matches!(result, Err(Error::Http(r)) if r.status() == StatusCode::FORBIDDEN));
            assert!(state.aistudio.registry().connected().is_empty());
        } else {
            let (mut ws, _) = result.unwrap();
            ws.send(Message::Text(
                json!({"id":"catalog","type":"catalog","payload":{"models":["aistudio-review"],"ack":true}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                let frame = ws.next().await.unwrap().unwrap().into_text().unwrap();
                let ack: serde_json::Value = serde_json::from_str(&frame).unwrap();
                assert_eq!(ack["type"], "catalog_ack");
                assert_eq!(ack["id"], "catalog");
            })
            .await
            .unwrap();
            assert_eq!(
                state.aistudio.registry().catalog("aistudio-worker-review"),
                Some(vec!["aistudio-review".into()])
            );
            ws.close(None).await.unwrap();
        }
    }
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}
