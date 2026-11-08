mod common;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use mahoquot_gateway::{config::GatewayConfig, routes::create_app, state::AppState};
use mahoquot_registry::store::{StorageBackend, StorageManager, StoreError};
use mahoquot_types::{Health, PoolMember};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

type SharedObjects = Arc<Mutex<BTreeMap<String, Vec<u8>>>>;

#[derive(Default)]
struct Backend {
    values: Mutex<BTreeMap<String, Vec<u8>>>,
    fail: AtomicBool,
}

impl StorageBackend for Backend {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.values.lock().unwrap().get(key).cloned())
    }
    fn put(&self, key: &str, value: &[u8]) -> Result<(), StoreError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(StoreError::Unavailable("fixture".into()));
        }
        self.values.lock().unwrap().insert(key.into(), value.into());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), StoreError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(StoreError::Unavailable("fixture".into()));
        }
        self.values.lock().unwrap().remove(key);
        Ok(())
    }
    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        Ok(self
            .values
            .lock()
            .unwrap()
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect())
    }
}

struct Fixture {
    root: std::path::PathBuf,
    state: Arc<AppState>,
    app: Router,
    backend: Arc<Backend>,
    store: Arc<StorageManager>,
}

impl Fixture {
    fn new(remote: bool) -> Self {
        let root =
            std::env::temp_dir().join(format!("credentials-review-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = Arc::new(Backend::default());
        let store = Arc::new(StorageManager::new(backend.clone(), root.join("spool")).unwrap());
        std::fs::create_dir_all(store.auth_dir()).unwrap();
        let config = GatewayConfig {
            auth_dir: store.auth_dir(),
            config_path: root.join("config.yaml"),
            auth_refresh_enabled: false,
            ..Default::default()
        };
        common::configure_management_secret(&config.config_path, "credentials-fixture");
        let state = Arc::new(AppState::new(&config).unwrap());
        if remote {
            state.settings.set_storage_manager(store.clone());
        }
        let app = create_app(state.clone());
        Self {
            root,
            state,
            app,
            backend,
            store,
        }
    }

    fn saved(&self, name: &str) -> Value {
        let bytes = self.backend.get(&format!("auths/{name}")).unwrap().unwrap();
        assert_eq!(
            std::fs::read(self.store.auth_dir().join(name)).unwrap(),
            bytes
        );
        serde_json::from_slice(&bytes).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

fn credential(id: &str) -> Value {
    json!({"type":"codex", "identity_slug":id, "account_id":id,
        "email":format!("{id}@example.invalid"), "access_token":"fixture", "refresh_token":"fixture",
        "expired":"2099-01-01T00:00:00Z", "id_token":"fixture", "last_refresh":"", "disabled":false,
        "upstream_override":"http://127.0.0.1:18899"})
}

fn file_snapshot(root: &std::path::Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(file_snapshot(&path));
        } else {
            files.insert(path.clone(), std::fs::read(path).unwrap());
        }
    }
    files
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn portable_filenames_reject_before_any_credential_publication() {
    for remote in [false, true] {
        for (version, resource) in [("v0", "auth-files"), ("v8", "credentials")] {
            let f = Fixture::new(remote);
            let base = format!("/{version}/management/{resource}");
            let outside = f.root.join("outside.json");
            std::fs::write(
                &outside,
                serde_json::to_vec(&credential("outside")).unwrap(),
            )
            .unwrap();
            let before = file_snapshot(&f.root);
            let remote_before = f.backend.values.lock().unwrap().clone();
            let absolute = outside.to_string_lossy().into_owned();
            let absent_absolute = f
                .root
                .join("new-outside.json")
                .to_string_lossy()
                .into_owned();
            for name in [
                "",
                ".",
                "..",
                "../outside.json",
                "..\\outside.json",
                "nested/new.json",
                "nested\\new.json",
                "./new.json",
                "C:\\outside\\new.json",
                "C:new.json",
                "C:/outside/new.json",
                "\\outside\\new.json",
                "\\\\server\\share\\new.json",
                "\\\\?\\C:\\outside\\new.json",
                "\\\\.\\C:\\outside\\new.json",
                "new.json:stream",
                "bad\0.json",
                absolute.as_str(),
                absent_absolute.as_str(),
            ] {
                let query: String = name.bytes().map(|b| format!("%{b:02X}")).collect();
                for (method, path, body) in [
                    (
                        Method::POST,
                        base.clone(),
                        json!({"name":name,"content":credential("new")}),
                    ),
                    (Method::DELETE, format!("{base}?name={query}"), Value::Null),
                    (
                        Method::POST,
                        format!("/v0/management/auth-files/delete?name={query}"),
                        Value::Null,
                    ),
                    (
                        Method::PATCH,
                        format!("{base}/status"),
                        json!({"name":name,"disabled":true}),
                    ),
                    (
                        Method::PATCH,
                        format!("{base}/fields"),
                        json!({"name":name,"label":"changed"}),
                    ),
                    (
                        Method::POST,
                        "/v0/management/auth-files/import".into(),
                        json!({"files":[
                            {"name":"codex-pending.json","content":credential("pending")},
                            {"name":name,"content":credential("new")}
                        ]}),
                    ),
                    (
                        Method::POST,
                        "/v0/management/auth-files/bulk".into(),
                        json!({"action":"disable","names":[name]}),
                    ),
                    (
                        Method::PUT,
                        "/v0/management/auth-files/order".into(),
                        json!({"names":[name]}),
                    ),
                ] {
                    assert_eq!(
                        request(&f.app, method, &path, body).await.0,
                        StatusCode::BAD_REQUEST,
                        "{path}: {name:?}, remote={remote}"
                    );
                    assert_eq!(
                        file_snapshot(&f.root),
                        before,
                        "local/outside publication: {name:?}"
                    );
                    assert_eq!(
                        *f.backend.values.lock().unwrap(),
                        remote_before,
                        "remote publication: {name:?}"
                    );
                    assert!(f.state.pool.load().members.is_empty());
                }
            }
            for reserved in [
                ".mahoquot-account-order.json",
                "account-order.json",
                "telemetry.json",
            ] {
                assert_eq!(
                    request(
                        &f.app,
                        Method::PATCH,
                        &format!("{base}/status"),
                        json!({"name":reserved,"disabled":true})
                    )
                    .await
                    .0,
                    StatusCode::BAD_REQUEST
                );
            }
            // Interior dots are not traversal; preserve normal JSON filenames.
            let name = "codex-valid..name.JSON";
            for (method, path, body) in [
                (
                    Method::POST,
                    base.clone(),
                    json!({"name":name,"content":credential("valid")}),
                ),
                (
                    Method::PATCH,
                    format!("{base}/status"),
                    json!({"name":name,"disabled":true}),
                ),
                (
                    Method::PATCH,
                    format!("{base}/fields"),
                    json!({"name":name,"label":"updated"}),
                ),
            ] {
                assert_eq!(request(&f.app, method, &path, body).await.0, StatusCode::OK);
            }
            let saved: Value =
                serde_json::from_slice(&std::fs::read(f.store.auth_dir().join(name)).unwrap())
                    .unwrap();
            assert_eq!(saved["disabled"], true);
            assert_eq!(saved["label"], "updated");
            if remote {
                assert_eq!(f.saved(name), saved);
            }
            assert_eq!(
                request(
                    &f.app,
                    Method::DELETE,
                    &format!("{base}?name={name}"),
                    Value::Null
                )
                .await
                .0,
                StatusCode::OK
            );
            assert_eq!(file_snapshot(&f.root), before);
            assert_eq!(*f.backend.values.lock().unwrap(), remote_before);
        }
    }
}

async fn request(app: &Router, method: Method, path: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("X-Management-Key", "credentials-fixture")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        app.clone().oneshot(common::with_loopback_peer(req)),
    )
    .await
    .unwrap()
    .unwrap();
    let status = response.status();
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        response.into_body().collect(),
    )
    .await
    .unwrap()
    .unwrap()
    .to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credential_mutations_survive_store_sync_and_delete_aliases() {
    for version in ["v0", "v8"] {
        let f = Fixture::new(true);
        let resource = if version == "v8" {
            "credentials"
        } else {
            "auth-files"
        };
        let base = format!("/{version}/management/{resource}");
        assert_eq!(
            request(
                &f.app,
                Method::POST,
                &base,
                json!({"name":"codex-one.json","content":credential("one")})
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            request(
                &f.app,
                Method::PATCH,
                &format!("{base}/status"),
                json!({"name":"codex-one.json","disabled":true})
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(f.saved("codex-one.json")["disabled"], true);
        assert_eq!(
            request(
                &f.app,
                Method::PATCH,
                &format!("{base}/fields"),
                json!({"name":"codex-one.json","label":"updated"})
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(f.saved("codex-one.json")["label"], "updated");
        f.store.sync_all_to_spool().unwrap();
        f.state.rescan_pool().unwrap();
        assert_eq!(
            f.state.find_member("one").unwrap().health(),
            Health::Disabled
        );
        assert_eq!(
            request(
                &f.app,
                Method::DELETE,
                &format!("{base}?name=codex-one.json"),
                Value::Null
            )
            .await
            .0,
            StatusCode::OK
        );
        f.store.sync_all_to_spool().unwrap();
        f.state.rescan_pool().unwrap();
        assert!(f.backend.get("auths/codex-one.json").unwrap().is_none());
        assert!(!f.store.auth_dir().join("codex-one.json").exists());
        assert!(f.state.find_member("one").is_none());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_bulk_and_provider_keys_use_authoritative_store() {
    let f = Fixture::new(true);
    assert_eq!(
        request(
            &f.app,
            Method::POST,
            "/v0/management/auth-files/import",
            json!({"files":[{"name":"codex-one.json","content":credential("one")}]})
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        request(
            &f.app,
            Method::POST,
            "/v0/management/auth-files/bulk",
            json!({"action":"disable","names":["codex-one.json"]})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(f.saved("codex-one.json")["disabled"], true);
    let provider = json!({"id":"fixture", "name":"Fixture", "base_url":"http://127.0.0.1:18899/v1", "adapter":"openai-chat", "models":["fixture-model"], "api_key":"fixture"});
    let (status, body) = request(
        &f.app,
        Method::POST,
        "/v0/management/providers",
        provider.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let name = body["provider"]["accounts"][0]["credential_name"]
        .as_str()
        .unwrap();
    let key = body["provider"]["keys"][0]["id"].as_str().unwrap();
    f.saved(name);
    let mut updated = provider;
    updated["name"] = json!("Updated");
    assert_eq!(
        request(
            &f.app,
            Method::PUT,
            "/v0/management/providers/fixture",
            updated
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(f.saved(name)["management_provider_name"], "Updated");
    let key_path = format!("/v0/management/providers/fixture/keys/{key}");
    assert_eq!(
        request(&f.app, Method::PATCH, &key_path, json!({"enabled":false}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(f.saved(name)["disabled"], true);
    assert_eq!(
        request(
            &f.app,
            Method::POST,
            "/v0/management/providers/fixture/keys",
            json!({"api_key":"second"})
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        request(&f.app, Method::DELETE, &key_path, Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert!(f.backend.get(&format!("auths/{name}")).unwrap().is_none());
    assert_eq!(
        request(
            &f.app,
            Method::DELETE,
            "/v0/management/providers/fixture",
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    f.store.sync_all_to_spool().unwrap();
    assert_eq!(
        f.backend.list("auths/").unwrap(),
        vec!["auths/codex-one.json"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_store_does_not_publish_mutations_or_imports() {
    let f = Fixture::new(true);
    assert_eq!(
        request(
            &f.app,
            Method::POST,
            "/v0/management/auth-files",
            json!({"name":"codex-one.json","content":credential("one")})
        )
        .await
        .0,
        StatusCode::OK
    );
    let before = f.saved("codex-one.json");
    f.backend.fail.store(true, Ordering::SeqCst);
    for (method, path, body) in [
        (
            Method::PATCH,
            "/auth-files/status",
            json!({"name":"codex-one.json","disabled":true}),
        ),
        (
            Method::PATCH,
            "/auth-files/fields",
            json!({"name":"codex-one.json","label":"lost"}),
        ),
        (
            Method::POST,
            "/auth-files/bulk",
            json!({"action":"disable","names":["codex-one.json"]}),
        ),
        (
            Method::DELETE,
            "/auth-files?name=codex-one.json",
            Value::Null,
        ),
        (
            Method::POST,
            "/auth-files/import",
            json!({"files":[{"name":"codex-two.json","content":credential("two")}]}),
        ),
    ] {
        assert_eq!(
            request(&f.app, method, &format!("/v0/management{path}"), body)
                .await
                .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(f.saved("codex-one.json"), before);
        assert_ne!(
            f.state.find_member("one").unwrap().health(),
            Health::Disabled
        );
        assert!(f.state.find_member("two").is_none());
        assert!(!f.store.auth_dir().join("codex-two.json").exists());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_pending_names_reject_without_publication() {
    for remote in [false, true] {
        let f = Fixture::new(remote);
        let (status, _) = request(
            &f.app,
            Method::POST,
            "/v0/management/auth-files/import",
            json!({"files":[
                {"name":"codex-same.json","content":credential("one")},
                {"name":"codex-same.json","content":credential("two")}
            ]}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(!f.store.auth_dir().join("codex-same.json").exists());
        assert!(f.backend.list("auths/").unwrap().is_empty());
        assert!(f.state.pool.load().members.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn divergent_store_rejects_patch_without_changing_local_or_pool() {
    let f = Fixture::new(true);
    assert_eq!(
        request(
            &f.app,
            Method::POST,
            "/v0/management/auth-files",
            json!({"name":"codex-one.json","content":credential("one")})
        )
        .await
        .0,
        StatusCode::OK
    );
    let local = std::fs::read(f.store.auth_dir().join("codex-one.json")).unwrap();
    let mut remote = credential("one");
    remote["label"] = json!("remote change");
    f.backend
        .put(
            "auths/codex-one.json",
            &serde_json::to_vec(&remote).unwrap(),
        )
        .unwrap();
    assert_eq!(
        request(
            &f.app,
            Method::PATCH,
            "/v0/management/auth-files/status",
            json!({"name":"codex-one.json","disabled":true})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        std::fs::read(f.store.auth_dir().join("codex-one.json")).unwrap(),
        local
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&f.backend.get("auths/codex-one.json").unwrap().unwrap())
            .unwrap(),
        remote
    );
    assert_ne!(
        f.state.find_member("one").unwrap().health(),
        Health::Disabled
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_imports_cannot_clobber_the_winner() {
    for remote in [false, true] {
        let f = Fixture::new(remote);
        let start = Arc::new(tokio::sync::Barrier::new(3));
        let mut tasks = Vec::new();
        for id in ["one", "two"] {
            let app = f.app.clone();
            let start = start.clone();
            tasks.push(tokio::spawn(async move {
                start.wait().await;
                (
                    id,
                    request(
                        &app,
                        Method::POST,
                        "/v0/management/auth-files/import",
                        json!({"files":[{"name":"codex-same.json","content":credential(id)}]}),
                    )
                    .await
                    .0,
                )
            }));
        }
        start.wait().await;
        let mut winner = None;
        let mut conflicts = 0;
        for task in tasks {
            let (id, status) = tokio::time::timeout(std::time::Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
            match status {
                StatusCode::CREATED => {
                    assert!(winner.replace(id).is_none());
                }
                StatusCode::CONFLICT => conflicts += 1,
                other => panic!("unexpected status {other}"),
            }
        }
        assert_eq!(conflicts, 1);
        let bytes = std::fs::read(f.store.auth_dir().join("codex-same.json")).unwrap();
        let saved: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(saved["identity_slug"], winner.unwrap());
        assert_eq!(f.state.pool.load().members.len(), 1);
        if remote {
            assert_eq!(f.saved("codex-same.json"), saved);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credential_handlers_persist_through_real_object_store_http() {
    use axum::{
        extract::{Path, State},
        routing::get,
    };
    use mahoquot_registry::store::ObjectStore;
    let f = Fixture::new(false);
    let objects = Arc::new(Mutex::new(BTreeMap::<String, Vec<u8>>::new()));
    let fixture = Router::new()
        .route(
            "/bucket/{*key}",
            get(
                |State(objects): State<SharedObjects>, Path(key): Path<String>| async move {
                    match objects.lock().unwrap().get(&key).cloned() {
                        Some(bytes) => (StatusCode::OK, bytes),
                        None => (StatusCode::NOT_FOUND, Vec::new()),
                    }
                },
            )
            .put(
                |State(objects): State<SharedObjects>,
                 Path(key): Path<String>,
                 bytes: bytes::Bytes| async move {
                    objects.lock().unwrap().insert(key, bytes.to_vec());
                    StatusCode::OK
                },
            )
            .delete(
                |State(objects): State<SharedObjects>, Path(key): Path<String>| async move {
                    objects.lock().unwrap().remove(&key);
                    StatusCode::NO_CONTENT
                },
            ),
        )
        .with_state(objects.clone());
    let listener = (18840..=18899)
        .find_map(|port| std::net::TcpListener::bind(("127.0.0.1", port)).ok())
        .unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, fixture)
            .with_graceful_shutdown(async {
                stop_rx.await.unwrap();
            })
            .await
            .unwrap();
    });
    let spool = f.store.spool_root().to_path_buf();
    let store = tokio::task::spawn_blocking(move || {
        Arc::new(
            StorageManager::new(
                Arc::new(ObjectStore::new(endpoint, "bucket", "fixture", "fixture", "").unwrap()),
                spool,
            )
            .unwrap(),
        )
    })
    .await
    .unwrap();
    f.state.settings.set_storage_manager(store);
    let created = request(
        &f.app,
        Method::POST,
        "/v0/management/auth-files",
        json!({"name":"codex-wire.json","content":credential("wire")}),
    )
    .await
    .0;
    let patched = request(
        &f.app,
        Method::PATCH,
        "/v0/management/auth-files/status",
        json!({"name":"codex-wire.json","disabled":true}),
    )
    .await
    .0;
    let persisted = objects
        .lock()
        .unwrap()
        .get("auths/codex-wire.json")
        .cloned();
    let deleted = request(
        &f.app,
        Method::DELETE,
        "/v0/management/auth-files?name=codex-wire.json",
        Value::Null,
    )
    .await
    .0;
    stop_tx.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!([created, patched, deleted], [StatusCode::OK; 3]);
    assert_eq!(
        serde_json::from_slice::<Value>(&persisted.unwrap()).unwrap()["disabled"],
        true
    );
    assert!(objects.lock().unwrap().is_empty());
    assert!(!f.store.auth_dir().join("codex-wire.json").exists());
}
