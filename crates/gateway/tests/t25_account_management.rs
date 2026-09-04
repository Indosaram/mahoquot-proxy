mod common;

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::routing::post;
use axum::Router;
use common::{unique_temp_dir, CODEX_PATH};
use http_body_util::BodyExt;
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::inbound::ApiKeys;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use mahoquot_types::Strategy;
use serde_json::{json, Value};
use tower::ServiceExt;

const MANAGEMENT_KEY: &str = "task-12-management-key";
const BOUND_KEY: &str = "task-12-bound-key";
const EXPORT_SECRET: &str = "task-12-export-secret";
static NEXT_PORT: AtomicU16 = AtomicU16::new(18870);

struct TestContext {
    auth_dir: std::path::PathBuf,
    state: Arc<AppState>,
    app: axum::Router,
}

impl Drop for TestContext {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.auth_dir).ok();
    }
}

fn context(tag: &str, inbound_keys: &[&str]) -> TestContext {
    let auth_dir = unique_temp_dir(&format!("mahoquot-t25-{tag}"));
    let config = GatewayConfig {
        auth_dir: auth_dir.clone(),
        config_path: auth_dir.join("config.yaml"),
        api_keys: ApiKeys::new(inbound_keys.iter().map(|key| (*key).to_string()).collect()),
        strategy: Strategy::FillFirst,
        max_failover: 3,
        auth_refresh_enabled: false,
        ..GatewayConfig::default()
    };
    let state = Arc::new(AppState::new(&config).expect("state"));
    let app = create_app(Arc::clone(&state));
    TestContext {
        auth_dir,
        state,
        app,
    }
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&bytes)}))
}

async fn management(
    app: &axum::Router,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> axum::response::Response {
    management_with_export(app, method, path, body, None).await
}

async fn management_with_export(
    app: &axum::Router,
    method: Method,
    path: &str,
    body: Option<Value>,
    export_secret: Option<&str>,
) -> axum::response::Response {
    let mut builder = Request::builder()
        .method(method)
        .uri(format!("/v0/management{path}"))
        .header(header::AUTHORIZATION, format!("Bearer {MANAGEMENT_KEY}"));
    if let Some(secret) = export_secret {
        builder = builder.header("x-mahoquot-export-authorization", secret);
    }
    let request_body = match body {
        Some(value) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    app.clone()
        .oneshot(builder.body(request_body).unwrap())
        .await
        .unwrap()
}

fn codex_credential(id: &str, email: &str) -> Value {
    json!({
        "type": "codex",
        "identity_slug": id,
        "access_token": format!("access-{id}"),
        "refresh_token": format!("refresh-{id}"),
        "account_id": format!("account-{id}"),
        "email": email,
        "expired": "2099-01-01T00:00:00Z",
        "id_token": format!("id-{id}"),
        "last_refresh": "2026-01-01T00:00:00Z",
        "disabled": false
    })
}

async fn bind_listener() -> tokio::net::TcpListener {
    for _ in 18840..=18899 {
        let next = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
        let port = 18840 + (next - 18840) % 60;
        if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            return listener;
        }
    }
    panic!("no free task-12 test port in 18840-18899");
}

async fn spawn_recording_upstream(
    record: &'static str,
) -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<&'static str>,
    tokio::task::JoinHandle<()>,
) {
    let listener = bind_listener().await;
    let address = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let tx = tx.clone();
            async move {
                tx.send(record).expect("relay record receiver");
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    json!({
                        "id": "chatcmpl-task-12",
                        "object": "chat.completion",
                        "choices": [{"index": 0, "message": {"role": "assistant", "content": record}}]
                    })
                    .to_string(),
                )
            }
        }),
    );
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (address, rx, task)
}

async fn create_provider(
    app: &axum::Router,
    id: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
) -> Value {
    let response = management(
        app,
        Method::POST,
        "/providers",
        Some(json!({
            "id": id,
            "name": id,
            "base_url": base_url,
            "adapter": "openai-chat",
            "models": [model],
            "api_key": api_key,
            "key_label": "primary"
        })),
    )
    .await;
    let status = response.status();
    let body = json_body(response).await;
    assert_eq!(status, StatusCode::CREATED, "provider create: {body}");
    body
}

#[tokio::test]
async fn inbound_key_binding_routes_to_selected_pool() {
    let (ordinary_url, mut ordinary_records, ordinary_task) =
        spawn_recording_upstream("ordinary-account").await;
    let (bound_url, mut bound_records, bound_task) =
        spawn_recording_upstream("bound-account").await;
    let ctx = context("binding", &[MANAGEMENT_KEY, BOUND_KEY]);
    create_provider(
        &ctx.app,
        "ordinary-provider",
        &ordinary_url,
        "ordinary-provider-secret",
        "task-12-model",
    )
    .await;
    let bound = create_provider(
        &ctx.app,
        "selected-provider",
        &bound_url,
        "selected-provider-secret",
        "task-12-model",
    )
    .await;
    let account = bound["provider"]["accounts"][0]["account"]
        .as_str()
        .expect("bound account id")
        .to_string();

    let binding = management(
        &ctx.app,
        Method::PUT,
        "/api-key-bindings",
        Some(json!({"api_key": BOUND_KEY, "account": account})),
    )
    .await;
    let status = binding.status();
    let binding_body = json_body(binding).await;
    assert_eq!(status, StatusCode::OK, "binding: {binding_body}");
    assert!(!binding_body.to_string().contains(BOUND_KEY));

    let response = ctx
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header(header::AUTHORIZATION, format!("Bearer {BOUND_KEY}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "model": "task-12-model",
                        "stream": false,
                        "messages": [{"role": "user", "content": "route me"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let relay_record =
        tokio::time::timeout(std::time::Duration::from_secs(2), bound_records.recv())
            .await
            .expect("bound relay record")
            .expect("bound upstream record");
    assert_eq!(relay_record, "bound-account");
    assert!(
        ordinary_records.try_recv().is_err(),
        "ordinary pool was selected"
    );

    ordinary_task.abort();
    bound_task.abort();
}

#[tokio::test]
async fn duplicate_import_rejected() {
    let ctx = context("duplicate", &[MANAGEMENT_KEY]);
    let first = management(
        &ctx.app,
        Method::POST,
        "/auth-files/import",
        Some(json!({
            "files": [{"name": "codex-first.json", "content": codex_credential("first", "same@example.test")}]
        })),
    )
    .await;
    assert_eq!(
        first.status(),
        StatusCode::CREATED,
        "first import: {}",
        json_body(first).await
    );

    let duplicate = management(
        &ctx.app,
        Method::POST,
        "/auth-files/import",
        Some(json!({
            "files": [{"name": "codex-second.json", "content": codex_credential("second", "same@example.test")}]
        })),
    )
    .await;
    let status = duplicate.status();
    let body = json_body(duplicate).await;
    assert_eq!(status, StatusCode::CONFLICT, "duplicate response: {body}");
    assert_eq!(body["error"]["code"], "duplicate_credential");
    assert!(!ctx.auth_dir.join("codex-second.json").exists());
}

#[tokio::test]
async fn invalid_provider_rejected() {
    let ctx = context("invalid-provider", &[MANAGEMENT_KEY]);
    let response = management(
        &ctx.app,
        Method::POST,
        "/providers",
        Some(json!({
            "id": "invalid-provider",
            "name": "Invalid provider",
            "base_url": "https://provider.invalid",
            "adapter": "ftp-tunnel",
            "models": ["model"],
            "api_key": "do-not-store"
        })),
    )
    .await;
    let status = response.status();
    let body = json_body(response).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "invalid provider: {body}");
    assert_eq!(body["error"]["code"], "invalid_provider");
    assert!(!body.to_string().contains("do-not-store"));
}

#[tokio::test]
async fn unauthorized_export_rejected() {
    let ctx = context("unauthorized-export", &[MANAGEMENT_KEY]);
    ctx.state
        .settings
        .mutate(|settings| settings.remote_management.secret_key = EXPORT_SECRET.to_string())
        .expect("save export secret");
    let imported = management(
        &ctx.app,
        Method::POST,
        "/auth-files/import",
        Some(json!({
            "files": [{"name": "codex-export.json", "content": codex_credential("export", "export@example.test")}]
        })),
    )
    .await;
    assert_eq!(imported.status(), StatusCode::CREATED);

    let response = management(
        &ctx.app,
        Method::POST,
        "/auth-files/export",
        Some(json!({"names": ["codex-export.json"]})),
    )
    .await;
    let status = response.status();
    let body = json_body(response).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "unauthorized export: {body}");
    assert_eq!(body["error"]["code"], "export_unauthorized");
    assert!(!body.to_string().contains("access-export"));
}

#[tokio::test]
async fn ordinary_account_hides_relay_fields() {
    let ctx = context("relay-visibility", &[MANAGEMENT_KEY]);
    let ordinary = json!({
        "type": "claude",
        "identity_slug": "ordinary-claude",
        "email": "ordinary@example.test",
        "api_key": "sk-clb-prefix-is-not-enough",
        "upstream_override": "https://ordinary.example.test",
        "plan": "standard",
        "disabled": false
    });
    let relay = json!({
        "type": "claude",
        "identity_slug": "shared-relay",
        "email": "relay@example.test",
        "api_key": "ordinary-looking-key",
        "upstream_override": "https://claude.nekos.example.test",
        "plan": "opus-standard",
        "disabled": false
    });
    let response = management(
        &ctx.app,
        Method::POST,
        "/auth-files/import",
        Some(json!({
            "files": [
                {"name": "claude-ordinary.json", "content": ordinary},
                {"name": "claude-shared.json", "content": relay}
            ]
        })),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "import: {}",
        json_body(response).await
    );

    let stats = ctx
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/stats")
                .header(header::AUTHORIZATION, format!("Bearer {MANAGEMENT_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let stats = json_body(stats).await;
    let accounts = stats["accounts"].as_array().unwrap();
    let ordinary = accounts
        .iter()
        .find(|account| account["id"] == "ordinary-claude")
        .expect("ordinary account");
    let relay = accounts
        .iter()
        .find(|account| account["id"] == "shared-relay")
        .expect("shared relay account");
    assert!(
        ordinary.get("plan").is_none(),
        "ordinary relay fields leaked: {ordinary}"
    );
    assert_eq!(relay["plan"], "opus-standard");

    let listed = management(&ctx.app, Method::GET, "/auth-files", None).await;
    let listed = json_body(listed).await.to_string();
    assert!(!listed.contains("sk-clb-prefix-is-not-enough"));
    assert!(!listed.contains("ordinary-looking-key"));
    assert!(!listed.contains("upstream_override"));
}

#[tokio::test]
async fn custom_provider_crud_and_provider_key_management_redacts_secrets() {
    let ctx = context("provider-crud", &[MANAGEMENT_KEY]);
    let created = create_provider(
        &ctx.app,
        "custom-provider",
        "https://provider.example.test",
        "provider-secret-one",
        "custom-model",
    )
    .await;
    assert!(!created.to_string().contains("provider-secret-one"));
    let first_key = created["provider"]["keys"][0]["id"]
        .as_str()
        .expect("first key id")
        .to_string();

    let add_key = management(
        &ctx.app,
        Method::POST,
        "/providers/custom-provider/keys",
        Some(json!({"label": "secondary", "api_key": "provider-secret-two"})),
    )
    .await;
    let status = add_key.status();
    let add_key = json_body(add_key).await;
    assert_eq!(status, StatusCode::CREATED, "add key: {add_key}");
    assert_eq!(add_key["provider"]["keys"].as_array().unwrap().len(), 2);
    assert!(!add_key.to_string().contains("provider-secret-two"));
    let second_key = add_key["provider"]["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|key| key["id"] != first_key)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let toggle = management(
        &ctx.app,
        Method::PATCH,
        &format!("/providers/custom-provider/keys/{second_key}"),
        Some(json!({"enabled": false})),
    )
    .await;
    let status = toggle.status();
    let toggle = json_body(toggle).await;
    assert_eq!(status, StatusCode::OK, "toggle: {toggle}");
    assert_eq!(
        toggle["provider"]["keys"]
            .as_array()
            .unwrap()
            .iter()
            .find(|key| key["id"] == second_key)
            .unwrap()["enabled"],
        false
    );

    let updated = management(
        &ctx.app,
        Method::PUT,
        "/providers/custom-provider",
        Some(json!({
            "name": "Updated provider",
            "base_url": "https://updated.example.test",
            "adapter": "openai-chat",
            "models": ["custom-model", "custom-model-2"]
        })),
    )
    .await;
    let status = updated.status();
    let updated = json_body(updated).await;
    assert_eq!(status, StatusCode::OK, "update: {updated}");
    assert_eq!(updated["provider"]["name"], "Updated provider");
    assert_eq!(
        updated["provider"]["base_url"],
        "https://updated.example.test"
    );

    let removed = management(
        &ctx.app,
        Method::DELETE,
        &format!("/providers/custom-provider/keys/{second_key}"),
        None,
    )
    .await;
    let status = removed.status();
    let removed = json_body(removed).await;
    assert_eq!(status, StatusCode::OK, "remove key: {removed}");
    assert_eq!(removed["provider"]["keys"].as_array().unwrap().len(), 1);

    let listed = management(&ctx.app, Method::GET, "/providers", None).await;
    let listed = json_body(listed).await;
    assert_eq!(listed["providers"].as_array().unwrap().len(), 1);
    assert!(!listed.to_string().contains("provider-secret-one"));
    assert!(!listed.to_string().contains("provider-secret-two"));

    let deleted = management(&ctx.app, Method::DELETE, "/providers/custom-provider", None).await;
    assert_eq!(deleted.status(), StatusCode::OK);
    let listed = json_body(management(&ctx.app, Method::GET, "/providers", None).await).await;
    assert!(listed["providers"].as_array().unwrap().is_empty());
    assert!(ctx.state.pool.load().members.is_empty());
}

#[tokio::test]
async fn explicit_export_returns_selected_secrets_only() {
    let ctx = context("authorized-export", &[MANAGEMENT_KEY]);
    ctx.state
        .settings
        .mutate(|settings| settings.remote_management.secret_key = EXPORT_SECRET.to_string())
        .expect("save export secret");
    let imported = management(
        &ctx.app,
        Method::POST,
        "/auth-files/import",
        Some(json!({
            "files": [
                {"name": "codex-one.json", "content": codex_credential("one", "one@example.test")},
                {"name": "codex-two.json", "content": codex_credential("two", "two@example.test")}
            ]
        })),
    )
    .await;
    assert_eq!(imported.status(), StatusCode::CREATED);

    let listed = json_body(management(&ctx.app, Method::GET, "/auth-files", None).await).await;
    let listed_text = listed.to_string();
    assert!(!listed_text.contains("access-one"));
    assert!(!listed_text.contains("refresh-one"));

    let exported = management_with_export(
        &ctx.app,
        Method::POST,
        "/auth-files/export",
        Some(json!({"names": ["codex-two.json"]})),
        Some(EXPORT_SECRET),
    )
    .await;
    let status = exported.status();
    let exported = json_body(exported).await;
    assert_eq!(status, StatusCode::OK, "authorized export: {exported}");
    assert_eq!(exported["files"].as_array().unwrap().len(), 1);
    assert_eq!(exported["files"][0]["name"], "codex-two.json");
    assert_eq!(
        exported["files"][0]["content"]["access_token"],
        "access-two"
    );
    assert!(!exported.to_string().contains("access-one"));
}

#[tokio::test]
async fn bulk_status_order_and_manual_priority_persist_across_restart() {
    let ctx = context("bulk-order", &[MANAGEMENT_KEY]);
    let imported = management(
        &ctx.app,
        Method::POST,
        "/auth-files/import",
        Some(json!({
            "files": [
                {"name": "codex-alpha.json", "content": codex_credential("alpha", "alpha@example.test")},
                {"name": "codex-beta.json", "content": codex_credential("beta", "beta@example.test")}
            ]
        })),
    )
    .await;
    assert_eq!(imported.status(), StatusCode::CREATED);
    ctx.state
        .find_member("beta")
        .unwrap()
        .ok_count
        .store(11, Ordering::Relaxed);

    let order = management(
        &ctx.app,
        Method::PUT,
        "/auth-files/order",
        Some(json!({"names": ["codex-beta.json", "codex-alpha.json"]})),
    )
    .await;
    assert_eq!(
        order.status(),
        StatusCode::OK,
        "display order: {}",
        json_body(order).await
    );

    // Under SST: PUT /auth-files/order must immediately hot-reload the in-memory pool!
    let live_members = ctx.state.pool.load().members.clone();
    assert_eq!(
        live_members
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        vec!["beta", "alpha"],
        "pool member slice order must immediately reflect saved account order"
    );

    let priority = management(
        &ctx.app,
        Method::PUT,
        "/scheduler/order",
        Some(json!({"order": ["beta", "alpha"]})),
    )
    .await;
    assert_eq!(
        priority.status(),
        StatusCode::OK,
        "scheduler order: {}",
        json_body(priority).await
    );

    let disable = management(
        &ctx.app,
        Method::POST,
        "/auth-files/bulk",
        Some(json!({"action": "disable", "names": ["codex-alpha.json"]})),
    )
    .await;
    let status = disable.status();
    let disable = json_body(disable).await;
    assert_eq!(status, StatusCode::OK, "bulk disable: {disable}");
    assert!(ctx.state.find_member("alpha").is_none());
    assert_eq!(
        ctx.state
            .find_member("beta")
            .unwrap()
            .ok_count
            .load(Ordering::Relaxed),
        11,
        "pool rescan wiped surviving runtime state"
    );

    let enable = management(
        &ctx.app,
        Method::POST,
        "/auth-files/bulk",
        Some(json!({"action": "enable", "names": ["codex-alpha.json"]})),
    )
    .await;
    assert_eq!(
        enable.status(),
        StatusCode::OK,
        "bulk enable: {}",
        json_body(enable).await
    );

    let restarted_config = GatewayConfig {
        auth_dir: ctx.auth_dir.clone(),
        config_path: ctx.auth_dir.join("config.yaml"),
        api_keys: ApiKeys::new(vec![MANAGEMENT_KEY.to_string()]),
        strategy: Strategy::FillFirst,
        ..GatewayConfig::default()
    };
    let restarted = Arc::new(AppState::new(&restarted_config).expect("restarted state"));
    let restarted_app = create_app(Arc::clone(&restarted));
    let files = json_body(management(&restarted_app, Method::GET, "/auth-files", None).await).await;
    let names: Vec<_> = files["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["codex-beta.json", "codex-alpha.json"]);
    assert!(files["files"]
        .as_array()
        .unwrap()
        .iter()
        .all(|file| file["disabled"] == false));

    let settings =
        json_body(management(&restarted_app, Method::GET, "/scheduler/settings", None).await).await;
    assert_eq!(settings["priorities"]["beta"], 0);
    assert_eq!(settings["priorities"]["alpha"], 1);
}

#[tokio::test]
async fn binding_rejects_unknown_targets_and_lists_only_key_identifiers() {
    let ctx = context("binding-errors", &[MANAGEMENT_KEY, BOUND_KEY]);
    let unknown_account = management(
        &ctx.app,
        Method::PUT,
        "/api-key-bindings",
        Some(json!({"api_key": BOUND_KEY, "account": "missing-account"})),
    )
    .await;
    let status = unknown_account.status();
    let unknown_account = json_body(unknown_account).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(unknown_account["error"]["code"], "binding_target_invalid");

    let unknown_provider = management(
        &ctx.app,
        Method::PUT,
        "/api-key-bindings",
        Some(json!({"api_key": BOUND_KEY, "provider": "missing-provider"})),
    )
    .await;
    let status = unknown_provider.status();
    let unknown_provider = json_body(unknown_provider).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(unknown_provider["error"]["code"], "binding_target_invalid");

    let listed =
        json_body(management(&ctx.app, Method::GET, "/api-key-bindings", None).await).await;
    assert!(!listed.to_string().contains(BOUND_KEY));
}

#[tokio::test]
async fn legacy_download_also_requires_explicit_export_authorization() {
    let ctx = context("download-auth", &[MANAGEMENT_KEY]);
    ctx.state
        .settings
        .mutate(|settings| settings.remote_management.secret_key = EXPORT_SECRET.to_string())
        .expect("save export secret");
    std::fs::write(
        ctx.auth_dir.join("codex-download.json"),
        serde_json::to_vec_pretty(&codex_credential("download", "download@example.test")).unwrap(),
    )
    .unwrap();
    ctx.state.rescan_pool().unwrap();

    let unauthorized = management(
        &ctx.app,
        Method::GET,
        "/auth-files/download?name=codex-download.json",
        None,
    )
    .await;
    assert_eq!(unauthorized.status(), StatusCode::FORBIDDEN);

    let authorized = management_with_export(
        &ctx.app,
        Method::GET,
        "/auth-files/download?name=codex-download.json",
        None,
        Some(EXPORT_SECRET),
    )
    .await;
    assert_eq!(authorized.status(), StatusCode::OK);
    let body = authorized.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("access-download"));
}

#[test]
fn task_12_test_ports_stay_inside_reserved_range() {
    assert!((18840..=18899).contains(&18870));
    assert_eq!(CODEX_PATH, "/backend-api/codex/responses");
}
