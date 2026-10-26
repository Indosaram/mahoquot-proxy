use axum::{
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use mahoquot_gateway::{config::GatewayConfig, quota::refresh_account_usage, state::AppState};
use serde_json::{json, Value};
use std::{
    future::Future,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("fixture deadline")
}

struct TempRoot(PathBuf);
impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("refresh-review-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn write(&self, name: &str, value: Value) {
        std::fs::write(self.0.join(name), serde_json::to_vec(&value).unwrap()).unwrap();
    }
    fn state(&self, token_url: &str) -> Arc<AppState> {
        Arc::new(
            AppState::new(&GatewayConfig {
                auth_dir: self.0.clone(),
                config_path: self.0.join("config.yaml"),
                refresh_url: token_url.into(),
                auth_refresh_enabled: true,
                ..Default::default()
            })
            .unwrap(),
        )
    }
}
impl Drop for TempRoot {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("owned fixture cleanup");
    }
}

async fn listener() -> tokio::net::TcpListener {
    for port in 18840..=18899 {
        match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => return listener,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {}
            Err(error) => panic!("bind: {error}"),
        }
    }
    panic!("no reserved fixture port")
}

struct Server(tokio::task::JoinHandle<()>);
impl Server {
    fn start(listener: tokio::net::TcpListener, router: Router) -> Self {
        Self(tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        }))
    }
    async fn close(mut self) {
        self.0.abort();
        assert!(bounded(&mut self.0).await.unwrap_err().is_cancelled());
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test]
async fn xai_invalid_proxy_never_dials_billing() {
    let socket = listener().await;
    let base = format!("http://{}", socket.local_addr().unwrap());
    let dir = TempRoot::new();
    dir.write(
        "generic-xai.json",
        json!({"type":"generic","provider":"xai",
        "identity_slug":"xai-review","adapter":"openai-chat","base_url":base,
        "usage_override":base,"api_key":"secret","models":["grok"],
        "expired":"2099-01-01T00:00:00Z","proxy_url":"ftp://invalid-proxy"}),
    );
    let state = dir.state(&format!("{base}/token"));
    let member = state.find_member("xai-review").unwrap();
    bounded(async {
        tokio::select! {
            biased;
            accepted = socket.accept() => panic!("unexpected direct connection: {accepted:?}"),
            result = refresh_account_usage(&state, &member) => {
                assert!(result.unwrap_err().to_string().contains("proxy client construction failed"));
            }
        }
    }).await;
}

#[tokio::test]
async fn rescan_during_rotation_reloads_the_committed_grant() {
    let socket = listener().await;
    let base = format!("http://{}", socket.local_addr().unwrap());
    let (arrival, mut arrived) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let gate = release.clone();
    let server = Server::start(
        socket,
        Router::new().route(
            "/token",
            post(move |body: String| {
                let arrival = arrival.clone();
                let gate = gate.clone();
                async move {
                    arrival.send(body).unwrap();
                    bounded(gate.acquire()).await.unwrap().forget();
                    Json(json!({"access_token":"fresh","refresh_token":"R2","expires_in":3600}))
                }
            }),
        ),
    );
    let dir = TempRoot::new();
    dir.write(
        "codex-review.json",
        json!({"type":"plus","identity_slug":"rotation",
        "account_id":"fixture","email":"fixture@example.invalid","access_token":"stale",
        "refresh_token":"R1","id_token":"id","last_refresh":"",
        "expired":"2020-01-01T00:00:00Z","upstream_override":base}),
    );
    let state = dir.state(&format!("{base}/token"));
    let old = state.find_member("rotation").unwrap();
    let task_state = state.clone();
    let task_old = old.clone();
    let refreshing = tokio::spawn(async move { task_state.refresh_member(&task_old, None).await });
    assert!(bounded(arrived.recv())
        .await
        .unwrap()
        .contains("refresh_token=R1"));
    state.rescan_pool().unwrap();
    let next = state.find_member("rotation").unwrap();
    assert!(Arc::ptr_eq(&old.refresh_lock, &next.refresh_lock));
    release.add_permits(1);
    assert!(bounded(refreshing).await.unwrap().unwrap());
    state.rescan_pool().unwrap();
    assert!(Arc::ptr_eq(
        &old.refresh_lock,
        &state.find_member("rotation").unwrap().refresh_lock
    ));
    bounded(async {
        tokio::select! {
            biased;
            duplicate = arrived.recv() => panic!("duplicate exchange: {duplicate:?}"),
            result = state.refresh_member(&next, Some("stale")) => assert!(!result.unwrap()),
        }
    })
    .await;
    assert_eq!(next.access_token(), "fresh");
    assert_eq!(next.refresh_token(), "R2");
    state.rescan_pool().unwrap();
    assert!(Arc::ptr_eq(
        &old.refresh_lock,
        &state.find_member("rotation").unwrap().refresh_lock
    ));
    let disk: Value = serde_json::from_slice(&std::fs::read(&next.file_path).unwrap()).unwrap();
    assert_eq!(disk["refresh_token"], "R2");
    server.close().await;
}

#[tokio::test]
async fn kimi_commits_rotation_before_catalog_and_uses_rotated_grant_next() {
    use mahoquot_registry::store::{LocalStore, StorageBackend, StorageManager};
    let socket = listener().await;
    let base = format!("http://{}", socket.local_addr().unwrap());
    let grants = Arc::new(Mutex::new(Vec::new()));
    let seen = grants.clone();
    let (catalog, mut catalogs) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let gate = release.clone();
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let server = Server::start(socket, Router::new()
        .route("/api/oauth/token", post(move |body: String| {
            let seen = seen.clone();
            async move {
                let mut grants = seen.lock().unwrap();
                let expected = if grants.is_empty() { "refresh_token=R1" } else { "refresh_token=R2" };
                assert!(body.contains(expected), "single-use grant replay: {body}");
                grants.push(body);
                let n = grants.len() + 1;
                Json(json!({"access_token":format!("A{n}"),"refresh_token":format!("R{n}"),"expires_in":3600}))
            }
        }))
        .route("/v1/models", get(move || {
            let catalog = catalog.clone(); let gate = gate.clone(); let count = count.clone();
            async move {
                let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                catalog.send(()).unwrap();
                bounded(gate.acquire()).await.unwrap().forget();
                (if n == 0 { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK },
                    Json(json!({"data":[{"id":"new-model"}]})))
            }
        })));
    let dir = TempRoot::new();
    dir.write("generic-kimi.json", json!({"type":"generic","provider":"kimi",
        "identity_slug":"kimi-review","adapter":"openai-chat","base_url":base,
        "upstream_override":base,"token_url":format!("{base}/api/oauth/token"),
        "api_key":"A1","refresh_token":"R1","models":["old-model"],"expired":"2020-01-01T00:00:00Z"}));
    let state = dir.state(&format!("{base}/api/oauth/token"));
    let member = state.find_member("kimi-review").unwrap();
    let remote = Arc::new(LocalStore::new(dir.0.join("remote")).unwrap());
    let storage = Arc::new(StorageManager::new(remote.clone(), dir.0.join("spool")).unwrap());
    storage
        .put(
            "auths/generic-kimi.json",
            &std::fs::read(&member.file_path).unwrap(),
        )
        .unwrap();
    state.settings.set_storage_manager(storage);
    for (old, new) in [("A1", "R2"), ("A2", "R3")] {
        let task_state = state.clone();
        let task_member = member.clone();
        let path = member.file_path.clone();
        let task =
            tokio::spawn(async move { task_state.refresh_member(&task_member, Some(old)).await });
        bounded(catalogs.recv()).await.unwrap();
        let disk: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(
            disk["refresh_token"], new,
            "rotation must precede catalog I/O"
        );
        assert_eq!(disk["models"], json!(["old-model"]));
        let durable: Value =
            serde_json::from_slice(&remote.get("auths/generic-kimi.json").unwrap().unwrap())
                .unwrap();
        assert_eq!(durable["refresh_token"], new);
        release.add_permits(1);
        assert!(bounded(task).await.unwrap().unwrap());
        if new == "R2" {
            assert_eq!(state_for_models(&member), vec!["old-model"]);
        }
    }
    assert_eq!(member.refresh_token(), "R3");
    assert_eq!(state_for_models(&member), vec!["new-model"]);
    assert_eq!(grants.lock().unwrap().len(), 2);
    let reconstructed = StorageManager::new(remote, dir.0.join("reconstructed")).unwrap();
    reconstructed.sync_all_to_spool().unwrap();
    let restored: Value = serde_json::from_slice(
        &std::fs::read(reconstructed.auth_dir().join("generic-kimi.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(restored["refresh_token"], "R3");
    assert_eq!(restored["models"], json!(["new-model"]));
    server.close().await;
}

fn state_for_models(member: &mahoquot_gateway::account::AccountMember) -> Vec<String> {
    member.generic_models().unwrap().1
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

#[tokio::test]
async fn proxy_failure_logs_and_errors_do_not_disclose_credentials() {
    let dir = TempRoot::new();
    dir.write(
        "generic-review.json",
        json!({"type":"generic","provider":"fixture",
        "identity_slug":"proxy-review","adapter":"openai-chat","base_url":"http://127.0.0.1:18840",
        "api_key":"fixture","proxy_url":"ftp://SENTINEL_USER:SENTINEL_PASSWORD@127.0.0.1:18840"}),
    );
    let state = dir.state("http://127.0.0.1:18840/token");
    let member = state.find_member("proxy-review").unwrap();
    let buffer = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(buffer.clone())
        .finish();
    let error = tracing::subscriber::with_default(subscriber, || {
        state.try_client_for_member(&member).unwrap_err()
    });
    let logs = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
    assert!(!logs.is_empty(), "exercise the diagnostic sink");
    for output in [logs, error.to_string(), format!("{error:?}")] {
        assert!(!output.contains("SENTINEL_USER"));
        assert!(!output.contains("SENTINEL_PASSWORD"));
    }
}

#[tokio::test]
async fn zcode_unbound_logs_cannot_supply_any_members_quota() {
    if let Ok(path) = std::env::var("REFRESH_REVIEW_CHILD_ROOT") {
        bounded(async {
            let socket = listener().await;
            let base = format!("http://{}", socket.local_addr().unwrap());
            let server = Server::start(
                socket,
                Router::new().fallback(|| async { StatusCode::SERVICE_UNAVAILABLE }),
            );
            let dir = TempRoot(PathBuf::from(path).join("auth"));
            std::fs::create_dir_all(&dir.0).unwrap();
            for id in ["one", "two"] {
                dir.write(
                    &format!("zcode-{id}.json"),
                    json!({"type":"zcode","identity_slug":id,
                    "access_token":format!("key.{id}"),"refresh_token":"","email":"",
                    "expired":"2099-01-01T00:00:00Z","usage_override":base}),
                );
            }
            let state = dir.state(&format!("{base}/token"));
            for id in ["one", "two"] {
                let member = state.find_member(id).unwrap();
                let snapshot_time = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64;
                let before = member.usage_snapshot_at(snapshot_time);
                let error = bounded(refresh_account_usage(&state, &member))
                    .await
                    .expect_err("unbound logs cannot supply account quota");
                let failed = member.usage_snapshot_at(snapshot_time);
                assert_eq!(failed.refreshed_at_unix, before.refreshed_at_unix);
                assert_eq!(failed.refresh_status.as_deref(), Some("error"));
                assert_eq!(failed.last_refresh_error, Some(error.to_string()));
                let mut expected = before;
                expected.refresh_status = Some("error".to_string());
                expected.last_refresh_error = Some(error.to_string());
                assert_eq!(
                    serde_json::to_value(failed).unwrap(),
                    serde_json::to_value(expected).unwrap()
                );
            }
            server.close().await;
        })
        .await;
        return;
    }
    let dir = TempRoot::new();
    let logs = dir.0.join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(logs.join("2026-10-25.log"), r#"{"balances":[{"show_name":"GLM-5.3","used_units":3,"total_units":10,"period_end":1999999999}]}"#).unwrap();
    use tokio::io::AsyncReadExt;

    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "zcode_unbound_logs_cannot_supply_any_members_quota",
            "--nocapture",
        ])
        .env("REFRESH_REVIEW_CHILD_ROOT", &dir.0)
        .env("ZCODE_LOGS_DIR", logs)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().expect("child stdout pipe");
    let mut stderr = child.stderr.take().expect("child stderr pipe");
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(
            child.wait(),
            stdout.read_to_end(&mut stdout_bytes),
            stderr.read_to_end(&mut stderr_bytes),
        )
    })
    .await;
    let (status, stdout_result, stderr_result) = match result {
        Ok(result) => result,
        Err(_) => {
            let kill_result = child.start_kill();
            let cleanup = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(
                    child.wait(),
                    stdout.read_to_end(&mut stdout_bytes),
                    stderr.read_to_end(&mut stderr_bytes),
                )
            })
            .await;
            panic!(
                "isolated fixture timed out; kill: {kill_result:?}; wait/drain: {cleanup:?}\n{}\n{}",
                String::from_utf8_lossy(&stdout_bytes),
                String::from_utf8_lossy(&stderr_bytes)
            );
        }
    };
    stdout_result.expect("drain child stdout");
    stderr_result.expect("drain child stderr");
    assert!(
        status.expect("wait for isolated fixture").success(),
        "{}\n{}",
        String::from_utf8_lossy(&stdout_bytes),
        String::from_utf8_lossy(&stderr_bytes)
    );
}
