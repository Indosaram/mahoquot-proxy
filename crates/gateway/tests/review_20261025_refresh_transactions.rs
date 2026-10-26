mod common;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    routing::{get, post},
    Json, Router,
};
use mahoquot_gateway::{config::GatewayConfig, routes::create_app, state::AppState};
use mahoquot_registry::store::{LocalStore, StorageBackend, StorageManager};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tower::ServiceExt;

async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("event deadline")
}

struct Root(std::path::PathBuf);
impl Drop for Root {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            if std::thread::panicking() {
                eprintln!("fixture cleanup failed for {}: {error}", self.0.display());
            } else {
                panic!("fixture cleanup failed for {}: {error}", self.0.display());
            }
        }
    }
}

struct OwnedTask<T: Send + 'static>(Option<tokio::task::JoinHandle<T>>);
impl<T: Send + 'static> OwnedTask<T> {
    fn spawn(future: impl std::future::Future<Output = T> + Send + 'static) -> Self {
        Self(Some(tokio::spawn(future)))
    }
    async fn join(mut self) -> T {
        let result = bounded(self.0.as_mut().unwrap()).await;
        self.0.take();
        result.unwrap()
    }
}
impl<T: Send + 'static> Drop for OwnedTask<T> {
    fn drop(&mut self) {
        if let Some(mut task) = self.0.take() {
            task.abort();
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    tokio::time::timeout(Duration::from_secs(10), &mut task).await
                })
            });
            let error = match result {
                Ok(Ok(_)) => None,
                Ok(Err(error)) if error.is_cancelled() => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(error) => Some(error.to_string()),
            };
            if let Some(error) = error {
                if std::thread::panicking() {
                    eprintln!("fixture task cleanup failed: {error}");
                } else {
                    panic!("fixture task cleanup failed: {error}");
                }
            }
        }
    }
}

// The token request is the subscription signal: management commits while the
// provider is gated, then rotation is released. No scheduling delay is assumed.
async fn race(delete: bool, remote: bool, provider_kind: &str, replace_grant: bool) {
    let kimi = provider_kind == "kimi";
    let meta = provider_kind == "meta";
    let vertex = provider_kind == "vertex";
    let root =
        Root(std::env::temp_dir().join(format!("refresh-transaction-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir_all(&root.0).unwrap();
    let listener = (18840..=18899)
        .find_map(|port| std::net::TcpListener::bind(("127.0.0.1", port)).ok())
        .unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    let (entered, mut events) = tokio::sync::mpsc::unbounded_channel();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let release = gate.clone();
    let provider = Router::new().route("/api/oauth/token", post(move || {
        let entered = entered.clone(); let gate = gate.clone();
        async move {
            entered.send(()).unwrap();
            bounded(gate.acquire()).await.unwrap().forget();
            Json(if vertex {
                json!({"access_token":"A2","expires_in":3600})
            } else {
                json!({"access_token":"A2","api_key":"A2","refresh_token":"R2","expires_in":3600})
            })
        }
    })).route("/v1/models", get(|| async { Json(json!({"data":[{"id":"new-model"}]})) }));
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = OwnedTask::spawn(async move {
        axum::serve(listener, provider)
            .with_graceful_shutdown(async {
                stopped.await.unwrap();
            })
            .await
            .unwrap();
    });
    let backend = Arc::new(LocalStore::new(root.0.join("remote")).unwrap());
    let store = Arc::new(StorageManager::new(backend.clone(), root.0.join("spool")).unwrap());
    let mut credential = json!({"type":"generic","provider":if kimi {"kimi"} else {"fixture"},
        "identity_slug":"transaction","email":"fixture@example.invalid","adapter":"openai-chat",
        "auth_mode":"oauth","base_url":base,"upstream_override":base,
        "token_url":format!("{base}/api/oauth/token"),"api_key":"A1","refresh_token":"R1",
        "models":["old-model"],"disabled":false,"label":"original","expired":"2020-01-01T00:00:00Z"});
    if meta {
        credential["type"] = json!("meta");
        credential["access_token"] = json!("A1");
        credential["dca_token"] = json!("dca:fixture");
        credential["auth_kind"] = json!("oauth");
        credential["upstream_override"] = json!(format!("{base}/v1"));
        credential["mint_url"] = json!(format!("{base}/api/oauth/token"));
    }
    if vertex {
        credential = json!({"type":"vertex","api_key":"A1","disabled":false,"label":"original",
            "expired":"2020-01-01T00:00:00Z", "upstream_override":base,
            "service_account":{"client_email":"fixture@example.invalid","project_id":"fixture",
                "token_uri":format!("{base}/api/oauth/token"),
                "private_key":include_str!("fixtures/test-rsa-private.pem")}});
    }
    let filename = if vertex {
        "vertex-transaction.json"
    } else {
        "generic-transaction.json"
    };
    let key = format!("auths/{filename}");
    store
        .put(&key, &serde_json::to_vec(&credential).unwrap())
        .unwrap();
    let config = GatewayConfig {
        auth_dir: store.auth_dir(),
        config_path: root.0.join("config.yaml"),
        auth_refresh_enabled: false,
        ..Default::default()
    };
    common::configure_management_secret(&config.config_path, "transaction-fixture");
    let state = Arc::new(AppState::new(&config).unwrap());
    if remote {
        state.settings.set_storage_manager(store.clone());
    }
    // Initial pool loading uses the generic filename fallback, not Vertex's
    // reload helper, and therefore retains the vertex- prefix for absent slugs.
    let member = state
        .find_member(if vertex {
            "vertex-transaction"
        } else {
            "transaction"
        })
        .unwrap();
    let refreshing = state.clone();
    let task =
        OwnedTask::spawn(async move { refreshing.refresh_member(&member, Some("A1")).await });
    bounded(events.recv()).await.unwrap();
    let app = create_app(state.clone());
    let (method, uri, body) = if delete {
        (
            Method::DELETE,
            "/v0/management/auth-files?name=generic-transaction.json",
            Value::Null,
        )
    } else {
        (
            Method::PATCH,
            "/v0/management/auth-files/status",
            json!({"name":filename,"disabled":true}),
        )
    };
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("X-Management-Key", "transaction-fixture")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = bounded(app.clone().oneshot(common::with_loopback_peer(request)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    if !delete {
        let fields = if replace_grant {
            json!({"name":"generic-transaction.json","refresh_token":"replacement","dca_token":"dca:replacement"})
        } else {
            json!({"name":filename,"label":"edited"})
        };
        let request = Request::builder()
            .method(Method::PATCH)
            .uri("/v0/management/auth-files/fields")
            .header("X-Management-Key", "transaction-fixture")
            .header("content-type", "application/json")
            .body(Body::from(fields.to_string()))
            .unwrap();
        assert_eq!(
            bounded(app.oneshot(common::with_loopback_peer(request)))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }
    release.add_permits(1);
    let result = task.join().await;
    if delete {
        assert!(result.is_err());
        assert!(!store.auth_dir().join("generic-transaction.json").exists());
        if remote {
            assert!(backend
                .get("auths/generic-transaction.json")
                .unwrap()
                .is_none());
        }
    } else if replace_grant {
        assert!(result.is_err());
        let saved: Value = serde_json::from_slice(
            &std::fs::read(store.auth_dir().join("generic-transaction.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved["refresh_token"], "replacement");
        assert_eq!(saved["api_key"], "A1");
    } else {
        assert!(result.unwrap());
        let saved: Value =
            serde_json::from_slice(&std::fs::read(store.auth_dir().join(filename)).unwrap())
                .unwrap();
        assert_eq!(saved["disabled"], true);
        assert_eq!(saved["label"], "edited");
        if vertex {
            assert!(saved.get("refresh_token").is_none());
        } else {
            assert_eq!(saved["refresh_token"], if meta { "R1" } else { "R2" });
        }
        assert_eq!(saved[if vertex { "access_token" } else { "api_key" }], "A2");
        if vertex {
            assert!(saved.get("api_key").is_none());
        }
        if kimi {
            assert_eq!(saved["models"], json!(["new-model"]));
        }
        if remote {
            let reconstructed = StorageManager::new(backend.clone(), root.0.join("fresh")).unwrap();
            reconstructed.sync_all_to_spool().unwrap();
            let durable: Value = serde_json::from_slice(
                &std::fs::read(reconstructed.auth_dir().join(filename)).unwrap(),
            )
            .unwrap();
            assert_eq!(durable, saved);
        }
    }
    stop.send(()).unwrap();
    server.join().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_survives_rotation_and_catalog_commit() {
    for remote in [false, true] {
        for provider in ["generic", "kimi", "meta", "vertex"] {
            race(false, remote, provider, false).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleted_credentials_are_not_recreated_by_rotation() {
    for remote in [false, true] {
        for provider in ["generic", "kimi", "meta"] {
            race(true, remote, provider, false).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replaced_grants_reject_inflight_rotation() {
    for remote in [false, true] {
        for provider in ["generic", "kimi", "meta"] {
            race(false, remote, provider, true).await;
        }
    }
}
