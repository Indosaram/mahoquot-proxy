mod common;

use std::sync::Arc;
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use common::unique_temp_dir;
use http_body_util::BodyExt;
use mahoquot_gateway::account::{AccountMember, ProviderAccount, ProviderKind};
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::inbound::ApiKeys;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use mahoquot_providers::devin::{
    DevinAccount, DEVIN_CREDENTIALS_PATH_ENV, DEVIN_DEFAULT_API_SERVER_URL, DEVIN_TYPE,
};
use mahoquot_types::{PoolMember, Strategy};
use serde_json::{json, Value};
use tower::ServiceExt;

const MASTER_KEY: &str = "test-management-master-key";
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

fn test_gateway_config(auth_dir: &std::path::Path) -> GatewayConfig {
    GatewayConfig {
        port: 0,
        auth_dir: auth_dir.to_path_buf(),
        strategy: Strategy::StrictRoundRobin,
        max_failover: 3,
        log_level: "info".to_string(),
        api_keys: ApiKeys::new(vec![MASTER_KEY.to_string()]),
        models_env: None,
        refresh_url: mahoquot_providers::refresh::REFRESH_TOKEN_URL.to_string(),
        auth_refresh_enabled: false,
        usage_poll_secs: 120,
        config_path: auth_dir.join("config.yaml"),
        catalog_cache_path: None,
        history_queue_capacity: 1024,
        history_batch_size: 64,
    }
}

struct TestContext {
    auth_dir: std::path::PathBuf,
    state: Arc<AppState>,
    app: axum::Router,
}

async fn create_test_context(label: &str) -> TestContext {
    let auth_dir = unique_temp_dir(label);
    let config = test_gateway_config(&auth_dir);
    let state = Arc::new(AppState::new(&config).expect("create app state"));
    let app = create_app(Arc::clone(&state));
    TestContext {
        auth_dir,
        state,
        app,
    }
}

async fn send_management_request(
    app: &axum::Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> axum::response::Response {
    let mut req_builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {MASTER_KEY}"));

    let req_body = match body {
        Some(val) => {
            req_builder = req_builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(val.to_string())
        }
        None => Body::empty(),
    };

    app.clone()
        .oneshot(req_builder.body(req_body).unwrap())
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// 1. Manual Auth-File Upload & Stable Identity Replacement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_manual_auth_file_upload_and_stable_identity_replacement() {
    let ctx = create_test_context("devin-manual").await;

    // A. Upload valid Devin credential via POST /v0/management/auth-files
    let token_1 = "devin-session-token$initial_12345";
    let upload_body = json!({
        "name": "devin-work.json",
        "content": {
            "type": "devin",
            "identity_slug": "devin-work",
            "label": "Devin Work Initial",
            "access_token": token_1,
            "api_server_url": DEVIN_DEFAULT_API_SERVER_URL,
            "disabled": false
        }
    });

    let resp = send_management_request(
        &ctx.app,
        Method::POST,
        "/v0/management/auth-files",
        Some(upload_body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp_json = body_json(resp).await;
    assert_eq!(resp_json["status"], "ok");

    // Verify file exists on disk
    let file_path = ctx.auth_dir.join("devin-work.json");
    assert!(file_path.exists());

    // Verify member in pool
    let member = ctx.state.find_member("devin-work").expect("member in pool");
    assert_eq!(member.kind(), ProviderKind::Devin);
    assert_eq!(member.access_token(), token_1);
    assert_eq!(member.email(), None);
    assert!(!member.is_manually_disabled());

    // Upstream headers: literal Basic <token>-<token>
    let headers = member.build_upstream_headers();
    let auth_hdr = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .expect("authorization header");
    assert_eq!(auth_hdr.1, format!("Basic {token_1}-{token_1}"));
    assert!(!auth_hdr.1.contains("Bearer"));

    // B. Stable Identity Replacement: upload new token for same identity
    let token_2 = "devin-session-token$replaced_67890";
    let replace_body = json!({
        "name": "devin-work.json",
        "content": {
            "type": "devin",
            "identity_slug": "devin-work",
            "label": "Devin Work Replaced",
            "access_token": token_2,
            "api_server_url": DEVIN_DEFAULT_API_SERVER_URL,
            "disabled": false
        }
    });

    let resp2 = send_management_request(
        &ctx.app,
        Method::POST,
        "/v0/management/auth-files",
        Some(replace_body),
    )
    .await;
    assert_eq!(resp2.status(), StatusCode::OK);

    // Pool member identity is preserved ("devin-work"), but token is updated
    let member2 = ctx.state.find_member("devin-work").expect("member in pool");
    assert_eq!(member2.id(), "devin-work");
    assert_eq!(member2.access_token(), token_2);
    let headers2 = member2.build_upstream_headers();
    let auth_hdr2 = headers2
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .expect("auth header");
    assert_eq!(auth_hdr2.1, format!("Basic {token_2}-{token_2}"));

    // C. Validation rejections
    // Invalid token with whitespace
    let bad_body_ws = json!({
        "name": "devin-bad.json",
        "content": {
            "type": "devin",
            "identity_slug": "devin-bad",
            "access_token": "token with spaces"
        }
    });
    let bad_resp = send_management_request(
        &ctx.app,
        Method::POST,
        "/v0/management/auth-files",
        Some(bad_body_ws),
    )
    .await;
    assert_eq!(bad_resp.status(), StatusCode::BAD_REQUEST);

    // Empty access token
    let bad_body_empty = json!({
        "name": "devin-bad.json",
        "content": {
            "type": "devin",
            "identity_slug": "devin-bad",
            "access_token": ""
        }
    });
    let bad_resp2 = send_management_request(
        &ctx.app,
        Method::POST,
        "/v0/management/auth-files",
        Some(bad_body_empty),
    )
    .await;
    assert_eq!(bad_resp2.status(), StatusCode::BAD_REQUEST);

    // Empty identity
    let bad_body_noid = json!({
        "name": "devin-bad.json",
        "content": {
            "type": "devin",
            "identity_slug": "   ",
            "access_token": "token-ok"
        }
    });
    let bad_resp3 = send_management_request(
        &ctx.app,
        Method::POST,
        "/v0/management/auth-files",
        Some(bad_body_noid),
    )
    .await;
    assert_eq!(bad_resp3.status(), StatusCode::BAD_REQUEST);

    std::fs::remove_dir_all(ctx.auth_dir).ok();
}

// ---------------------------------------------------------------------------
// 2. CLI Import on Proxy Host & Rejection of Arbitrary File Paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_cli_import_resolved_on_proxy_host_atomic_and_unchanged_source() {
    let _env_guard = ENV_LOCK.lock().await;
    let ctx = create_test_context("devin-cli-import").await;

    // Create a temporary Devin CLI credentials file on the proxy host
    let cli_dir = unique_temp_dir("devin-mock-cli");
    let cli_file = cli_dir.join("credentials.toml");
    let initial_toml_content = r#"windsurf_api_key = "devin-cli-session-token$host999"
api_server_url = "https://server.codeium.com"
"#;
    std::fs::write(&cli_file, initial_toml_content).expect("write cli toml");
    let initial_mtime = std::fs::metadata(&cli_file).unwrap().modified().unwrap();

    // Set DEVIN_CREDENTIALS_PATH environment variable for proxy host resolution
    std::env::set_var(DEVIN_CREDENTIALS_PATH_ENV, cli_file.to_str().unwrap());

    // A. Rejection of arbitrary file paths in import request
    for path_key in ["path", "file_path", "file", "credentials_path", "source_path"] {
        let evil_body = json!({
            "identity": "devin-evil",
            path_key: "/etc/passwd"
        });
        let evil_resp = send_management_request(
            &ctx.app,
            Method::POST,
            "/v0/management/devin/import-cli",
            Some(evil_body),
        )
        .await;
        assert_eq!(
            evil_resp.status(),
            StatusCode::BAD_REQUEST,
            "must reject {path_key}"
        );
        let evil_json = body_json(evil_resp).await;
        assert!(
            evil_json["error"].as_str().unwrap().contains("arbitrary file paths"),
            "error should mention arbitrary file paths"
        );
    }

    // B. Legitimate CLI import: accepts only identity/label
    let import_body = json!({
        "identity": "cli-work",
        "label": "Devin CLI Work Account"
    });
    let import_resp = send_management_request(
        &ctx.app,
        Method::POST,
        "/v0/management/devin/import-cli",
        Some(import_body),
    )
    .await;
    assert_eq!(import_resp.status(), StatusCode::OK);
    let import_json = body_json(import_resp).await;
    assert_eq!(import_json["status"], "ok");
    assert_eq!(import_json["name"], "devin-cli-work.json");
    assert_eq!(import_json["identity_slug"], "cli-work");

    // Verify unchanged source TOML (atomic persistence in auth_dir, source untouched)
    let post_import_bytes = std::fs::read(&cli_file).expect("read source toml");
    assert_eq!(
        std::str::from_utf8(&post_import_bytes).unwrap(),
        initial_toml_content,
        "source TOML must be completely unchanged"
    );
    let post_import_mtime = std::fs::metadata(&cli_file).unwrap().modified().unwrap();
    assert_eq!(initial_mtime, post_import_mtime);

    // Verify imported auth file in auth_dir
    let saved_auth_file = ctx.auth_dir.join("devin-cli-work.json");
    assert!(saved_auth_file.exists());
    let saved_json: Value = serde_json::from_slice(&std::fs::read(&saved_auth_file).unwrap()).unwrap();
    assert_eq!(saved_json["type"], "devin");
    assert_eq!(saved_json["identity_slug"], "cli-work");
    assert_eq!(saved_json["label"], "Devin CLI Work Account");
    assert_eq!(saved_json["access_token"], "devin-cli-session-token$host999");
    assert_eq!(saved_json["api_server_url"], "https://server.codeium.com");
    assert_eq!(saved_json["disabled"], false);

    // Verify member loaded in pool
    let member = ctx.state.find_member("cli-work").expect("member in pool");
    assert_eq!(member.kind(), ProviderKind::Devin);
    assert_eq!(member.access_token(), "devin-cli-session-token$host999");

    // C. Re-import: update source TOML with fresh token and import again with same identity
    let updated_toml_content = r#"windsurf_api_key = "devin-cli-session-token$fresh_rotate_001"
api_server_url = "https://server.codeium.com"
"#;
    std::fs::write(&cli_file, updated_toml_content).expect("write updated toml");

    let reimport_body = json!({
        "identity": "cli-work"
    });
    let reimport_resp = send_management_request(
        &ctx.app,
        Method::POST,
        "/v0/management/devin/import-cli",
        Some(reimport_body),
    )
    .await;
    assert_eq!(reimport_resp.status(), StatusCode::OK);

    // Verify token was updated in pool while preserving identity
    let member_reloaded = ctx.state.find_member("cli-work").expect("member in pool");
    assert_eq!(member_reloaded.id(), "cli-work");
    assert_eq!(member_reloaded.access_token(), "devin-cli-session-token$fresh_rotate_001");

    // Clean up
    std::env::remove_var(DEVIN_CREDENTIALS_PATH_ENV);
    std::fs::remove_dir_all(cli_dir).ok();
    std::fs::remove_dir_all(ctx.auth_dir).ok();
}

#[tokio::test]
async fn test_distinct_identities_work_and_devin_work_isolated_lifecycle() {
    let _env_guard = ENV_LOCK.lock().await;
    let ctx = create_test_context("devin-distinct-ids").await;

    // Create a temporary Devin CLI credentials file on the proxy host
    let cli_dir = unique_temp_dir("devin-distinct-cli");
    let cli_file = cli_dir.join("credentials.toml");
    let initial_toml = r#"windsurf_api_key = "devin-initial-token-111"
api_server_url = "https://server.codeium.com"
"#;
    std::fs::write(&cli_file, initial_toml).expect("write cli toml");
    std::env::set_var(DEVIN_CREDENTIALS_PATH_ENV, cli_file.to_str().unwrap());

    // 1. Import identity "work"
    let resp1 = send_management_request(
        &ctx.app,
        Method::POST,
        "/v0/management/devin/import-cli",
        Some(json!({
            "identity": "work",
            "label": "Devin Work"
        })),
    )
    .await;
    assert_eq!(resp1.status(), StatusCode::OK);
    let json1 = body_json(resp1).await;
    assert_eq!(json1["name"], "devin-work.json");
    assert_eq!(json1["identity_slug"], "work");

    // 2. Import identity "devin-work" (must not collide with "work" -> devin-work.json!)
    let resp2 = send_management_request(
        &ctx.app,
        Method::POST,
        "/v0/management/devin/import-cli",
        Some(json!({
            "identity": "devin-work",
            "label": "Devin Namespaced Work"
        })),
    )
    .await;
    assert_eq!(resp2.status(), StatusCode::OK);
    let json2 = body_json(resp2).await;
    assert_eq!(json2["name"], "devin-devin-work.json");
    assert_eq!(json2["identity_slug"], "devin-work");

    // 3. Prove separate files exist on disk simultaneously
    let file_work = ctx.auth_dir.join("devin-work.json");
    let file_devin_work = ctx.auth_dir.join("devin-devin-work.json");
    assert!(file_work.exists(), "devin-work.json must exist");
    assert!(file_devin_work.exists(), "devin-devin-work.json must exist as a separate file");

    // 4. Verify exact inventory identity_slug in GET /v0/management/auth-files
    let list_resp = send_management_request(&ctx.app, Method::GET, "/v0/management/auth-files", None).await;
    assert_eq!(list_resp.status(), StatusCode::OK);
    let list_json = body_json(list_resp).await;
    let files = list_json["files"].as_array().expect("files array");

    let entry_work = files
        .iter()
        .find(|f| f["name"] == "devin-work.json")
        .expect("devin-work.json entry");
    assert_eq!(entry_work["identity_slug"], "work");
    assert_eq!(entry_work["label"], "Devin Work");
    assert_eq!(entry_work["disabled"], false);

    let entry_devin_work = files
        .iter()
        .find(|f| f["name"] == "devin-devin-work.json")
        .expect("devin-devin-work.json entry");
    assert_eq!(entry_devin_work["identity_slug"], "devin-work");
    assert_eq!(entry_devin_work["label"], "Devin Namespaced Work");
    assert_eq!(entry_devin_work["disabled"], false);

    // 5. Disable "work" account via status patch
    let patch_resp = send_management_request(
        &ctx.app,
        Method::PATCH,
        "/v0/management/auth-files/status",
        Some(json!({
            "name": "devin-work.json",
            "disabled": true
        })),
    )
    .await;
    assert_eq!(patch_resp.status(), StatusCode::OK);

    // Verify disabled state isolated to target
    let member_work = ctx.state.find_member("work").expect("member work");
    assert!(member_work.is_manually_disabled());
    let member_devin_work = ctx.state.find_member("devin-work").expect("member devin-work");
    assert!(!member_devin_work.is_manually_disabled());

    // 6. Reimport target "work" only with fresh token in source TOML
    let updated_toml = r#"windsurf_api_key = "devin-updated-token-222"
api_server_url = "https://server.codeium.com"
"#;
    std::fs::write(&cli_file, updated_toml).expect("write updated toml");

    let reimport_resp = send_management_request(
        &ctx.app,
        Method::POST,
        "/v0/management/devin/import-cli",
        Some(json!({
            "identity": "work"
        })),
    )
    .await;
    assert_eq!(reimport_resp.status(), StatusCode::OK);

    // 7. Verify reimport only updated target "work", preserving disabled and label
    let reloaded_work = ctx.state.find_member("work").expect("reloaded work");
    assert_eq!(reloaded_work.access_token(), "devin-updated-token-222");
    assert!(reloaded_work.is_manually_disabled(), "must preserve disabled state across reimport");

    let file_work_content: Value = serde_json::from_slice(&std::fs::read(&file_work).unwrap()).unwrap();
    assert_eq!(file_work_content["identity_slug"], "work");
    assert_eq!(file_work_content["label"], "Devin Work", "must preserve label across reimport");
    assert_eq!(file_work_content["disabled"], true);

    // Verify non-target "devin-work" remained untouched
    let file_devin_work_content: Value = serde_json::from_slice(&std::fs::read(&file_devin_work).unwrap()).unwrap();
    assert_eq!(file_devin_work_content["identity_slug"], "devin-work");
    assert_eq!(file_devin_work_content["label"], "Devin Namespaced Work");
    assert_eq!(file_devin_work_content["access_token"], "devin-initial-token-111");
    assert_eq!(file_devin_work_content["disabled"], false);

    let untouched_devin_work = ctx.state.find_member("devin-work").expect("devin-work member");
    assert_eq!(untouched_devin_work.access_token(), "devin-initial-token-111");
    assert!(!untouched_devin_work.is_manually_disabled());

    // Clean up
    std::env::remove_var(DEVIN_CREDENTIALS_PATH_ENV);
    std::fs::remove_dir_all(cli_dir).ok();
    std::fs::remove_dir_all(ctx.auth_dir).ok();
}

// ---------------------------------------------------------------------------
// 3. Lifecycle Operations: Disabled, Delete, Rescan, Redacted Outputs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_devin_lifecycle_disable_delete_rescan_and_redaction() {
    let ctx = create_test_context("devin-lifecycle").await;

    let secret_token = "devin-super-secret-session-token-xyz";
    let upload_body = json!({
        "name": "devin-test.json",
        "content": {
            "type": "devin",
            "identity_slug": "devin-test",
            "label": "Devin Lifecycle Test",
            "access_token": secret_token,
            "disabled": false
        }
    });

    let resp = send_management_request(
        &ctx.app,
        Method::POST,
        "/v0/management/auth-files",
        Some(upload_body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // A. Redacted outputs in listing
    let list_resp = send_management_request(&ctx.app, Method::GET, "/v0/management/auth-files", None).await;
    assert_eq!(list_resp.status(), StatusCode::OK);
    let list_json = body_json(list_resp).await;
    let files = list_json["files"].as_array().expect("files array");
    let devin_entry = files
        .iter()
        .find(|f| f["name"] == "devin-test.json")
        .expect("found devin entry");
    assert_eq!(devin_entry["type"], "devin");
    assert_eq!(devin_entry["disabled"], false);
    // Secret token MUST NOT be exposed in describe listing!
    let listing_str = serde_json::to_string(&list_json).unwrap();
    assert!(
        !listing_str.contains(secret_token),
        "secret token must never appear in auth-files listing"
    );

    // B. Disable account via PATCH /v0/management/auth-files/status
    let disable_body = json!({
        "name": "devin-test.json",
        "disabled": true
    });
    let patch_resp = send_management_request(
        &ctx.app,
        Method::PATCH,
        "/v0/management/auth-files/status",
        Some(disable_body),
    )
    .await;
    assert_eq!(patch_resp.status(), StatusCode::OK);

    // Verify member is disabled in pool
    let member = ctx.state.find_member("devin-test").expect("found member");
    assert!(member.is_manually_disabled());
    assert_eq!(member.health(), mahoquot_types::Health::Disabled);

    // C. Re-enable account
    let enable_body = json!({
        "name": "devin-test.json",
        "disabled": false
    });
    let patch_resp2 = send_management_request(
        &ctx.app,
        Method::PATCH,
        "/v0/management/auth-files/status",
        Some(enable_body),
    )
    .await;
    assert_eq!(patch_resp2.status(), StatusCode::OK);
    let member2 = ctx.state.find_member("devin-test").expect("found member");
    assert!(!member2.is_manually_disabled());

    // D. Delete account via POST /v0/management/auth-files/delete?name=devin-test.json
    let delete_resp = send_management_request(
        &ctx.app,
        Method::POST,
        "/v0/management/auth-files/delete?name=devin-test.json",
        None,
    )
    .await;
    assert_eq!(delete_resp.status(), StatusCode::OK);

    // Verify file deleted and member removed from pool
    assert!(!ctx.auth_dir.join("devin-test.json").exists());
    assert!(ctx.state.find_member("devin-test").is_none());

    std::fs::remove_dir_all(ctx.auth_dir).ok();
}

// ---------------------------------------------------------------------------
// 4. No OAuth Refresh & Quota Unsupported Contracts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_devin_no_oauth_refresh_and_quota_unsupported() {
    let raw_acct = DevinAccount {
        provider_type: DEVIN_TYPE.to_string(),
        identity_slug: "devin-refresh-test".to_string(),
        label: Some("Devin Refresh".to_string()),
        email: None,
        access_token: "devin-session-token$norefresh".to_string(),
        api_server_url: DEVIN_DEFAULT_API_SERVER_URL.to_string(),
        disabled: false,
    };
    let member = Arc::new(AccountMember::for_test_with_id(
        "devin-refresh-test",
        ProviderAccount::Devin(raw_acct),
    ));

    // Expiry check: Devin CLI token is never expired for automatic refresh
    let now_unix = 1_800_000_000;
    assert!(!member.is_expired(now_unix));

    // Refresh execution: Devin account must reject refresh and return Ok(false)
    let client = reqwest::Client::new();
    let refresh_result = member
        .refresh(&client, "https://example.com/oauth/token", None)
        .await;
    assert!(
        matches!(refresh_result, Ok(false)),
        "Devin must never attempt OAuth refresh, should return Ok(false)"
    );

    // Upstream headers
    let headers = member.build_upstream_headers();
    let auth = headers.iter().find(|(k, _)| k == "authorization").unwrap();
    assert_eq!(
        auth.1,
        "Basic devin-session-token$norefresh-devin-session-token$norefresh"
    );

    // Quota: Devin quota must remain unsupported (unknown in UI, not 0% or free)
    let ctx = create_test_context("devin-quota").await;
    // We can verify refresh_account_usage returns Unsupported
    let quota_res = mahoquot_gateway::quota::refresh_account_usage(&ctx.state, &member).await;
    // QuotaError::Unsupported or Err
    assert!(
        quota_res.is_err(),
        "Devin quota must remain unsupported so UI renders unknown instead of fake 0%"
    );

    std::fs::remove_dir_all(ctx.auth_dir).ok();
}

// ---------------------------------------------------------------------------
// 5. Routing Isolation: devin/* Rejected by Codex Fallback & Unroutable Until Discovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_codex_rejects_devin_models_and_devin_unroutable_until_discovery() {
    let codex_kind = ProviderKind::Codex;
    let devin_kind = ProviderKind::Devin;

    // Codex must not serve devin/* or devin-* models
    assert!(
        !codex_kind.serves_model("devin/glm-5-2"),
        "Codex must reject devin/* model"
    );
    assert!(
        !codex_kind.serves_model("devin/swe-1-7"),
        "Codex must reject devin/* model"
    );
    assert!(
        !codex_kind.serves_model("devin-model"),
        "Codex must reject devin-* model"
    );

    // Devin kind does not serve models until discovery phase
    assert!(
        !devin_kind.serves_model("devin/glm-5-2"),
        "Devin provider kind must not serve models until discovery phase"
    );

    let raw_acct = DevinAccount {
        provider_type: DEVIN_TYPE.to_string(),
        identity_slug: "devin-routing-test".to_string(),
        label: None,
        email: None,
        access_token: "tok".to_string(),
        api_server_url: DEVIN_DEFAULT_API_SERVER_URL.to_string(),
        disabled: false,
    };
    let member = Arc::new(AccountMember::for_test_with_id(
        "devin-routing-test",
        ProviderAccount::Devin(raw_acct),
    ));

    // Devin account does not support models until discovery phase
    assert!(!member.supports_model("devin/glm-5-2"));
    assert!(!member.supports_model("gpt-5.6-sol"));
}

#[tokio::test]
async fn test_disabled_and_unloaded_credential_exposes_canonical_identity_slug_in_inventory() {
    let ctx = create_test_context("devin-inventory-canonical").await;

    // 1. Write a disabled Devin credential directly to disk
    let disabled_doc = json!({
        "type": "devin",
        "identity_slug": "offline-worker",
        "label": "Offline Worker",
        "access_token": "token-offline-secret-xyz",
        "api_server_url": DEVIN_DEFAULT_API_SERVER_URL,
        "disabled": true
    });
    std::fs::write(
        ctx.auth_dir.join("devin-offline-worker.json"),
        serde_json::to_string_pretty(&disabled_doc).unwrap(),
    )
    .expect("write disabled devin file");

    // 2. Write a credential without identity_slug in the document
    let legacy_doc = json!({
        "type": "devin",
        "label": "Legacy File Only",
        "access_token": "token-legacy-secret-abc",
        "disabled": false
    });
    std::fs::write(
        ctx.auth_dir.join("devin-legacy-worker.json"),
        serde_json::to_string_pretty(&legacy_doc).unwrap(),
    )
    .expect("write legacy devin file");

    // 3. Query inventory directly via GET /v0/management/auth-files
    let list_resp = send_management_request(&ctx.app, Method::GET, "/v0/management/auth-files", None).await;
    assert_eq!(list_resp.status(), StatusCode::OK);
    let list_json = body_json(list_resp).await;
    let files = list_json["files"].as_array().expect("files array");

    // Verify disabled credential exposes canonical identity_slug
    let offline_entry = files
        .iter()
        .find(|f| f["name"] == "devin-offline-worker.json")
        .expect("offline worker entry");
    assert_eq!(offline_entry["type"], "devin");
    assert_eq!(offline_entry["identity_slug"], "offline-worker");
    assert_eq!(offline_entry["identity"], "offline-worker");
    assert_eq!(offline_entry["account"], "offline-worker");
    assert_eq!(offline_entry["disabled"], true);
    assert_eq!(offline_entry["status"], "disabled");
    assert_eq!(offline_entry["label"], "Offline Worker");

    // Verify file-only credential without explicit identity_slug exposes canonical derived slug
    let legacy_entry = files
        .iter()
        .find(|f| f["name"] == "devin-legacy-worker.json")
        .expect("legacy worker entry");
    assert_eq!(legacy_entry["type"], "devin");
    assert_eq!(legacy_entry["identity_slug"], "legacy-worker");
    assert_eq!(legacy_entry["identity"], "legacy-worker");
    assert_eq!(legacy_entry["account"], "legacy-worker");
    assert_eq!(legacy_entry["disabled"], false);
    assert_eq!(legacy_entry["label"], "Legacy File Only");

    // Verify secret tokens are completely absent from describe output
    let listing_str = serde_json::to_string(&list_json).unwrap();
    assert!(!listing_str.contains("token-offline-secret-xyz"));
    assert!(!listing_str.contains("token-legacy-secret-abc"));

    std::fs::remove_dir_all(ctx.auth_dir).ok();
}

// ---------------------------------------------------------------------------
// 6. Security Defect Regressions: Path Escape, Typed Payload, Validation & Non-Devin Isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_devin_import_cli_rejects_path_escape_and_invalid_identities() {
    let _env_guard = ENV_LOCK.lock().await;
    let sandbox = unique_temp_dir("devin-sandbox-escape");
    let auth_dir = sandbox.join("auth");
    std::fs::create_dir_all(&auth_dir).expect("create auth dir");

    let config = test_gateway_config(&auth_dir);
    let state = Arc::new(AppState::new(&config).expect("create app state"));
    let app = create_app(Arc::clone(&state));

    let cli_dir = sandbox.join("cli");
    std::fs::create_dir_all(&cli_dir).expect("create cli dir");
    let cli_file = cli_dir.join("credentials.toml");
    let initial_toml = r#"windsurf_api_key = "devin-token-escape-test"
api_server_url = "https://server.codeium.com"
"#;
    std::fs::write(&cli_file, initial_toml).expect("write cli toml");
    std::env::set_var(DEVIN_CREDENTIALS_PATH_ENV, cli_file.to_str().unwrap());

    // Path escape attempts
    let escape_cases = vec![
        "x/../../escape",
        "../../escape",
        "sub/worker",
        "sub\\worker",
        "worker\x00escape",
        "worker\nescape",
        "..",
        ".",
        "worker:bad",
    ];

    for bad_id in escape_cases {
        let evil_body = json!({
            "identity": bad_id
        });
        let resp = send_management_request(
            &app,
            Method::POST,
            "/v0/management/devin/import-cli",
            Some(evil_body),
        )
        .await;

        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "must reject unsafe identity `{bad_id}` with 400 Bad Request"
        );

        // Verify no files created outside auth_dir
        let entries_sandbox: Vec<_> = std::fs::read_dir(&sandbox)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            entries_sandbox.len(),
            2,
            "only `auth` and `cli` dirs may exist in sandbox, found: {:?}",
            entries_sandbox
        );

        // Verify source TOML remains unchanged
        let toml_bytes = std::fs::read(&cli_file).unwrap();
        assert_eq!(std::str::from_utf8(&toml_bytes).unwrap(), initial_toml);
    }

    // Must reject unsafe identity BEFORE reading CLI: even if CLI credentials do not exist on host,
    // an unsafe identity MUST return 400 Bad Request, NOT 404 Not Found!
    std::fs::remove_file(&cli_file).expect("remove cli toml to test pre-read rejection");
    let resp_nonexistent = send_management_request(
        &app,
        Method::POST,
        "/v0/management/devin/import-cli",
        Some(json!({ "identity": "x/../../escape" })),
    )
    .await;
    assert_eq!(
        resp_nonexistent.status(),
        StatusCode::BAD_REQUEST,
        "must reject unsafe identity with 400 before reading host CLI"
    );

    // Clean up
    std::env::remove_var(DEVIN_CREDENTIALS_PATH_ENV);
    std::fs::remove_dir_all(sandbox).ok();
}

#[tokio::test]
async fn test_devin_import_cli_typed_payload_allowlist_and_conflicts() {
    let _env_guard = ENV_LOCK.lock().await;
    let sandbox = unique_temp_dir("devin-sandbox-typed");
    let auth_dir = sandbox.join("auth");
    std::fs::create_dir_all(&auth_dir).expect("create auth dir");

    let config = test_gateway_config(&auth_dir);
    let state = Arc::new(AppState::new(&config).expect("create app state"));
    let app = create_app(Arc::clone(&state));

    let cli_dir = sandbox.join("cli");
    std::fs::create_dir_all(&cli_dir).expect("create cli dir");
    let cli_file = cli_dir.join("credentials.toml");
    let initial_toml = r#"windsurf_api_key = "devin-token-typed-test"
api_server_url = "https://server.codeium.com"
"#;
    std::fs::write(&cli_file, initial_toml).expect("write cli toml");
    std::env::set_var(DEVIN_CREDENTIALS_PATH_ENV, cli_file.to_str().unwrap());

    // 1. Unknown fields rejected
    let resp = send_management_request(
        &app,
        Method::POST,
        "/v0/management/devin/import-cli",
        Some(json!({
            "identity": "valid-id",
            "unknown_field": "disallowed"
        })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "must reject unknown fields");

    // 2. Wrong types rejected
    let resp = send_management_request(
        &app,
        Method::POST,
        "/v0/management/devin/import-cli",
        Some(json!({
            "identity": 12345
        })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "must reject non-string identity");

    let resp = send_management_request(
        &app,
        Method::POST,
        "/v0/management/devin/import-cli",
        Some(json!({
            "label": false
        })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "must reject non-string label");

    // 3. Explicit empty identity rejected (must not silently default to "devin")
    let resp = send_management_request(
        &app,
        Method::POST,
        "/v0/management/devin/import-cli",
        Some(json!({
            "identity": ""
        })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "must reject explicit empty identity");

    let resp = send_management_request(
        &app,
        Method::POST,
        "/v0/management/devin/import-cli",
        Some(json!({
            "identity": "   "
        })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "must reject whitespace identity");

    // 4. Conflicting alias values rejected
    let resp = send_management_request(
        &app,
        Method::POST,
        "/v0/management/devin/import-cli",
        Some(json!({
            "identity": "work",
            "identity_slug": "different-work"
        })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "must reject conflicting alias values");

    // 5. Omitted default succeeds
    let resp = send_management_request(
        &app,
        Method::POST,
        "/v0/management/devin/import-cli",
        Some(json!({})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "omitted default should succeed");
    let json_resp = body_json(resp).await;
    assert_eq!(json_resp["identity_slug"], "devin");

    std::env::remove_var(DEVIN_CREDENTIALS_PATH_ENV);
    std::fs::remove_dir_all(sandbox).ok();
}

#[tokio::test]
async fn test_disk_rescan_validates_devin_account_and_skips_invalid() {
    let ctx = create_test_context("devin-rescan-validate").await;

    // 1. Write an invalid Devin account to auth_dir (whitespace in token)
    let invalid_token_doc = json!({
        "type": "devin",
        "identity_slug": "bad-token-acct",
        "label": "Bad Token Account",
        "access_token": "token with spaces",
        "api_server_url": DEVIN_DEFAULT_API_SERVER_URL,
        "disabled": false
    });
    std::fs::write(
        ctx.auth_dir.join("devin-bad-token-acct.json"),
        serde_json::to_string_pretty(&invalid_token_doc).unwrap(),
    )
    .expect("write bad token file");

    // 2. Write an invalid Devin account (URL with embedded credentials)
    let invalid_url_doc = json!({
        "type": "devin",
        "identity_slug": "bad-url-acct",
        "label": "Bad URL Account",
        "access_token": "valid-token-12345",
        "api_server_url": "https://user:pass@server.codeium.com",
        "disabled": false
    });
    std::fs::write(
        ctx.auth_dir.join("devin-bad-url-acct.json"),
        serde_json::to_string_pretty(&invalid_url_doc).unwrap(),
    )
    .expect("write bad url file");

    // 3. Write a valid Devin account
    let valid_doc = json!({
        "type": "devin",
        "identity_slug": "good-acct",
        "label": "Good Account",
        "access_token": "valid-token-12345",
        "api_server_url": DEVIN_DEFAULT_API_SERVER_URL,
        "disabled": false
    });
    std::fs::write(
        ctx.auth_dir.join("devin-good-acct.json"),
        serde_json::to_string_pretty(&valid_doc).unwrap(),
    )
    .expect("write good file");

    // 4. Write a valid Devin account with missing identity_slug (established compatibility contract)
    let missing_id_doc = json!({
        "type": "devin",
        "label": "Derived Identity",
        "access_token": "valid-token-67890",
        "api_server_url": DEVIN_DEFAULT_API_SERVER_URL,
        "disabled": false
    });
    std::fs::write(
        ctx.auth_dir.join("devin-derived-worker.json"),
        serde_json::to_string_pretty(&missing_id_doc).unwrap(),
    )
    .expect("write derived file");

    // Rescan pool
    ctx.state.rescan_pool().expect("rescan pool");

    // Invalid accounts must NOT be loaded into the pool
    assert!(
        ctx.state.find_member("bad-token-acct").is_none(),
        "account with invalid token must be skipped by loader"
    );
    assert!(
        ctx.state.find_member("bad-url-acct").is_none(),
        "account with invalid url must be skipped by loader"
    );

    // Valid accounts MUST be loaded into the pool
    let good_member = ctx.state.find_member("good-acct").expect("good-acct loaded");
    assert_eq!(good_member.access_token(), "valid-token-12345");

    let derived_member = ctx.state.find_member("derived-worker").expect("derived-worker loaded");
    assert_eq!(derived_member.access_token(), "valid-token-67890");

    std::fs::remove_dir_all(ctx.auth_dir).ok();
}

#[tokio::test]
async fn test_manual_auth_file_upload_persists_validated_normalized_content() {
    let ctx = create_test_context("devin-manual-normalized").await;

    // Upload with trimmable label and trimmable email
    let upload_body = json!({
        "name": "devin-norm.json",
        "content": {
            "type": "devin",
            "identity_slug": "norm-test",
            "label": "   Trimmed Label   ",
            "email": "   worker@devin.ai   ",
            "access_token": "devin-session-token$normalized123",
            "api_server_url": "https://server.codeium.com",
            "disabled": false
        }
    });

    let resp = send_management_request(
        &ctx.app,
        Method::POST,
        "/v0/management/auth-files",
        Some(upload_body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Read persisted file from disk
    let file_path = ctx.auth_dir.join("devin-norm.json");
    let disk_raw = std::fs::read_to_string(&file_path).expect("read file");
    let disk_json: Value = serde_json::from_str(&disk_raw).expect("parse json");

    // Persisted content must be the normalized object
    assert_eq!(disk_json["type"], "devin", "persisted type must be normalized to `devin`");
    assert_eq!(disk_json["label"], "Trimmed Label", "persisted label must be trimmed");
    assert_eq!(disk_json["email"], "worker@devin.ai", "persisted email must be trimmed");
    assert_eq!(disk_json["identity_slug"], "norm-test");
    assert_eq!(disk_json["access_token"], "devin-session-token$normalized123");

    // Verify in-memory member matches persisted disk content
    let member = ctx.state.find_member("norm-test").expect("loaded in pool");
    assert_eq!(member.id(), "norm-test");
    assert_eq!(member.email().as_deref(), Some("worker@devin.ai"));
    assert_eq!(member.access_token(), "devin-session-token$normalized123");

    // Verify inventory describes normalized label
    let list_resp = send_management_request(&ctx.app, Method::GET, "/v0/management/auth-files", None).await;
    assert_eq!(list_resp.status(), StatusCode::OK);
    let list_json = body_json(list_resp).await;
    let files = list_json["files"].as_array().expect("files array");
    let norm_entry = files.iter().find(|f| f["name"] == "devin-norm.json").expect("entry found");
    assert_eq!(norm_entry["label"], "Trimmed Label");

    std::fs::remove_dir_all(ctx.auth_dir).ok();
}

#[tokio::test]
async fn test_describe_does_not_synthesize_identity_for_non_devin_providers() {
    let ctx = create_test_context("devin-non-devin-describe").await;

    // Non-devin provider file without explicit identity_slug in document
    let codex_doc = json!({
        "type": "codex",
        "email": "user@example.com",
        "access_token": "tok",
        "refresh_token": "ref",
        "expired": "2099-01-01T00:00:00Z"
    });
    std::fs::write(
        ctx.auth_dir.join("codex-user.json"),
        serde_json::to_string_pretty(&codex_doc).unwrap(),
    )
    .expect("write codex file");

    let list_resp = send_management_request(&ctx.app, Method::GET, "/v0/management/auth-files", None).await;
    assert_eq!(list_resp.status(), StatusCode::OK);
    let list_json = body_json(list_resp).await;
    let files = list_json["files"].as_array().expect("files array");

    let codex_entry = files
        .iter()
        .find(|f| f["name"] == "codex-user.json")
        .expect("found codex entry");

    // Pre-existing contract: non-devin without explicit identity_slug does NOT have identity_slug synthesized
    assert!(
        codex_entry.get("identity_slug").is_none(),
        "non-devin provider must not have identity_slug synthesized in describe"
    );

    std::fs::remove_dir_all(ctx.auth_dir).ok();
}

// ---------------------------------------------------------------------------
// 7. Leak Regressions: Malformed Typed JSON Does Not Leak Raw Values
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_devin_malformed_typed_json_http_response_does_not_leak_secret() {
    let ctx = create_test_context("devin-http-leak").await;

    let secret_sentinel = "secret-sentinel-value-12345";
    let bad_upload = json!({
        "name": "devin-leak.json",
        "content": {
            "type": "devin",
            "identity_slug": "leak-test",
            "access_token": "valid-token-12345",
            "disabled": secret_sentinel
        }
    });

    let resp = send_management_request(
        &ctx.app,
        Method::POST,
        "/v0/management/auth-files",
        Some(bad_upload),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let resp_str = String::from_utf8_lossy(&resp_bytes);

    assert!(
        !resp_str.contains(secret_sentinel),
        "HTTP error response must NOT embed secret sentinel from serde type error: {resp_str}"
    );

    std::fs::remove_dir_all(ctx.auth_dir).ok();
}

#[tokio::test]
async fn test_devin_reload_from_file_display_debug_does_not_leak_secret() {
    let ctx = create_test_context("devin-reload-leak").await;

    // 1. Initially valid file
    let valid_doc = json!({
        "type": "devin",
        "identity_slug": "reload-leak",
        "access_token": "valid-token-12345",
        "disabled": false
    });
    let file_path = ctx.auth_dir.join("devin-reload-leak.json");
    std::fs::write(&file_path, serde_json::to_string_pretty(&valid_doc).unwrap()).unwrap();

    ctx.state.rescan_pool().expect("rescan pool");
    let member = ctx.state.find_member("reload-leak").expect("member loaded");

    // 2. Overwrite file with malformed typed JSON embedding a secret sentinel
    let secret_sentinel = "reload-secret-sentinel-999";
    let malformed_doc = json!({
        "type": "devin",
        "identity_slug": "reload-leak",
        "access_token": "valid-token-12345",
        "disabled": secret_sentinel
    });
    std::fs::write(&file_path, serde_json::to_string_pretty(&malformed_doc).unwrap()).unwrap();

    // 3. Call reload_from_file
    let err = member.reload_from_file().expect_err("reload must fail on malformed JSON");

    let display_str = format!("{err}");
    let debug_str = format!("{err:?}");

    assert!(
        !display_str.contains(secret_sentinel),
        "LoadError Display must NOT contain secret sentinel: {display_str}"
    );
    assert!(
        !debug_str.contains(secret_sentinel),
        "LoadError Debug must NOT contain secret sentinel: {debug_str}"
    );

    std::fs::remove_dir_all(ctx.auth_dir).ok();
}

#[derive(Clone, Default)]
struct BufferWriter(Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for BufferWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufferWriter {
    type Writer = BufferWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn test_devin_loader_skips_malformed_typed_json_without_raw_value_diagnostic() {
    let auth_dir = unique_temp_dir("devin-loader-leak");

    let secret_sentinel = "loader-secret-sentinel-888";
    let malformed_doc = json!({
        "type": "devin",
        "identity_slug": "loader-leak",
        "access_token": "valid-token-12345",
        "disabled": secret_sentinel
    });
    std::fs::write(
        auth_dir.join("devin-loader-leak.json"),
        serde_json::to_string_pretty(&malformed_doc).unwrap(),
    )
    .unwrap();

    let log_buf = Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = BufferWriter(Arc::clone(&log_buf));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer)
        .finish();

    let members = tracing::subscriber::with_default(subscriber, || {
        mahoquot_gateway::account::load_account_members(&auth_dir).unwrap()
    });

    assert!(
        members.iter().all(|m| m.id() != "loader-leak"),
        "loader must skip malformed typed JSON account"
    );

    let logs = String::from_utf8(log_buf.lock().unwrap().clone()).unwrap();
    assert!(
        !logs.contains(secret_sentinel),
        "loader diagnostic must NOT embed secret sentinel: {logs}"
    );

    std::fs::remove_dir_all(auth_dir).ok();
}

#[test]
fn test_management_contract_schema_and_parity_matrix_for_devin_import_cli() {
    // 1. Schema check in docs/management-contract-v1.schema.json
    let schema_str = include_str!("../../../docs/management-contract-v1.schema.json");
    let schema: Value = serde_json::from_str(schema_str).expect("parse schema");

    let devin_import = &schema["$defs"]["devin-import-cli"];
    assert!(
        devin_import.is_object(),
        "schema must define $defs['devin-import-cli']"
    );

    let req = &devin_import["properties"]["request"];
    assert_eq!(req["type"], "object");
    assert_eq!(req["properties"]["identity"]["type"], "string");
    assert_eq!(req["properties"]["identity"]["minLength"], 1);
    assert_eq!(req["properties"]["identity_slug"]["type"], "string");
    assert_eq!(req["properties"]["identity_slug"]["minLength"], 1);
    assert_eq!(req["properties"]["label"]["type"], "string");

    let resp = &devin_import["properties"]["response"];
    assert_eq!(resp["type"], "object");
    assert_eq!(resp["properties"]["status"]["const"], "ok");
    assert_eq!(resp["properties"]["name"]["type"], "string");
    assert_eq!(resp["properties"]["identity_slug"]["type"], "string");

    // 2. Route-owner map equality in x-route-registration-owners
    assert_eq!(
        schema["x-route-registration-owners"]["POST /v0/management/devin/import-cli"],
        "crates/gateway/src/management/creds.rs"
    );
}

