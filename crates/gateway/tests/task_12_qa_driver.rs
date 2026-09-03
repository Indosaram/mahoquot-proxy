mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::inbound::ApiKeys;
use mahoquot_gateway::management::settings::ModelCatalogSettings;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use mahoquot_registry::{
    CatalogSource, ModelDescriptor, ModelId, ProviderBinding, ProviderId, ProviderPolicy,
};
use serde_json::{json, Value};

const QA_PORT: u16 = 18873;

fn create_antigravity_cred(id: &str, disabled: bool) -> String {
    json!({
        "identity_slug": id,
        "access_token": "ya29.fake_antigravity_token",
        "refresh_token": "1//fake_refresh_token",
        "project_id": "test-project-ag",
        "email": format!("{id}@example.com"),
        "expired": "2099-01-01T00:00:00Z",
        "disabled": disabled,
        "type": "antigravity"
    })
    .to_string()
}

fn format_http_exchange(
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    req_body: Option<&str>,
    status: reqwest::StatusCode,
    resp_headers: &reqwest::header::HeaderMap,
    resp_body: &str,
) -> String {
    let mut out = format!("{method} {uri} HTTP/1.1\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    if let Some(body) = req_body {
        if !body.is_empty() {
            out.push_str(body);
            out.push_str("\r\n");
        }
    }

    out.push_str(&format!("HTTP/1.1 {status}\r\n"));
    for (k, v) in resp_headers {
        if let Ok(v_str) = v.to_str() {
            out.push_str(&format!("{k}: {v_str}\r\n"));
        }
    }
    out.push_str("\r\n");
    out.push_str(resp_body);
    out.push_str("\n\n---\n\n");
    out
}

#[tokio::test]
async fn task_12_http_qa_scenario() {
    let dir = common::unique_temp_dir("mahoquot-task-12-qa");
    let config_path = dir.join("config.yaml");
    let auth_dir = dir.join("auth");
    std::fs::create_dir_all(&auth_dir).expect("auth dir created");

    // Write initial Antigravity account (enabled)
    let ag_cred_path = auth_dir.join("antigravity-1.json");
    std::fs::write(&ag_cred_path, create_antigravity_cred("ag-test", false))
        .expect("antigravity cred written");

    // Catalog with newly signed/custom fixture model `gemini-next-flash-high` with Antigravity binding,
    // plus `model-claude-absent` with Claude binding (absent account).
    let custom_ag_id = ModelId::new("gemini-next-flash-high").unwrap();
    let custom_ag_desc = ModelDescriptor::new(custom_ag_id.clone(), "google")
        .with_display_name("Gemini Next Flash High")
        .with_capabilities([mahoquot_registry::ModelCapability::Chat])
        .with_binding(
            ProviderBinding::new(
                ProviderId::antigravity(),
                ProviderPolicy::Closed,
                CatalogSource::LocalOverride,
            )
            .with_capabilities([mahoquot_registry::ModelCapability::Chat]),
        );

    let absent_claude_id = ModelId::new("model-claude-absent").unwrap();
    let absent_claude_desc = ModelDescriptor::new(absent_claude_id.clone(), "anthropic")
        .with_capabilities([mahoquot_registry::ModelCapability::Chat])
        .with_binding(
            ProviderBinding::new(
                ProviderId::claude(),
                ProviderPolicy::Closed,
                CatalogSource::LocalOverride,
            )
            .with_capabilities([mahoquot_registry::ModelCapability::Chat]),
        );

    let init_settings = mahoquot_gateway::management::settings::Settings {
        port: QA_PORT,
        api_keys: vec!["test-admin".to_string()],
        auth_dir: auth_dir.to_string_lossy().to_string(),
        model_catalog: Some(ModelCatalogSettings {
            custom_models: vec![custom_ag_desc, absent_claude_desc],
            ..ModelCatalogSettings::default()
        }),
        ..Default::default()
    };

    init_settings
        .persist(&config_path)
        .expect("config persisted");

    let gateway_config = GatewayConfig {
        config_path: config_path.clone(),
        auth_dir: auth_dir.clone(),
        api_keys: ApiKeys::new(vec!["test-admin".to_string()]),
        port: QA_PORT,
        ..GatewayConfig::default()
    };

    let state = Arc::new(AppState::new(&gateway_config).expect("app state created"));
    let app = create_app(Arc::clone(&state));

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{QA_PORT}"))
        .await
        .expect("bound to port 18873");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    // Give server a moment to start
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client built");

    let base_url = format!("http://127.0.0.1:{QA_PORT}");
    let mut http_evidence = String::new();

    // 1. GET /v1/models
    let v1_uri = format!("{base_url}/v1/models");
    let resp_v1 = client
        .get(&v1_uri)
        .header("Authorization", "Bearer test-admin")
        .header("Host", format!("127.0.0.1:{QA_PORT}"))
        .send()
        .await
        .expect("v1/models response");

    let v1_status = resp_v1.status();
    let v1_headers = resp_v1.headers().clone();
    let v1_text = resp_v1.text().await.expect("v1/models text");

    http_evidence.push_str(&format_http_exchange(
        "GET",
        "/v1/models",
        &[
            ("Host", &format!("127.0.0.1:{QA_PORT}")),
            ("Authorization", "Bearer test-admin"),
        ],
        None,
        v1_status,
        &v1_headers,
        &v1_text,
    ));

    assert_eq!(v1_status, reqwest::StatusCode::OK);
    let v1_json: Value = serde_json::from_str(&v1_text).expect("v1/models parsed JSON");
    assert_eq!(v1_json["object"], "list");
    let v1_data = v1_json["data"].as_array().expect("data array");

    // Must contain gemini-next-flash-high with owned_by "google"
    let ag_model_entry = v1_data
        .iter()
        .find(|item| item["id"] == "gemini-next-flash-high")
        .expect("v1/models must contain gemini-next-flash-high");
    assert_eq!(ag_model_entry["owned_by"], "google");

    // Absent account models must be omitted
    assert!(
        v1_data
            .iter()
            .all(|item| item["id"] != "model-claude-absent"),
        "absent-account models must be omitted from /v1/models"
    );

    // Script verifies every exposed ID has a registry binding
    let active_reg = state.pool.load().registry.clone();
    for item in v1_data {
        let mid = item["id"].as_str().unwrap();
        let resolved = active_reg
            .resolve(mid)
            .unwrap_or_else(|e| panic!("model {mid} must resolve in registry: {e}"));
        assert!(
            !resolved.eligible_bindings.is_empty(),
            "model {mid} must have eligible bindings"
        );
    }

    // 2. GET /v1beta/models
    let beta_uri = format!("{base_url}/v1beta/models");
    let resp_beta = client
        .get(&beta_uri)
        .header("Authorization", "Bearer test-admin")
        .header("Host", format!("127.0.0.1:{QA_PORT}"))
        .send()
        .await
        .expect("v1beta/models response");

    let beta_status = resp_beta.status();
    let beta_headers = resp_beta.headers().clone();
    let beta_text = resp_beta.text().await.expect("v1beta/models text");

    http_evidence.push_str(&format_http_exchange(
        "GET",
        "/v1beta/models",
        &[
            ("Host", &format!("127.0.0.1:{QA_PORT}")),
            ("Authorization", "Bearer test-admin"),
        ],
        None,
        beta_status,
        &beta_headers,
        &beta_text,
    ));

    assert_eq!(beta_status, reqwest::StatusCode::OK);
    let beta_json: Value = serde_json::from_str(&beta_text).expect("v1beta/models parsed JSON");
    let beta_models = beta_json["models"].as_array().expect("models array");

    // Must contain gemini-next-flash-high
    let beta_ag = beta_models
        .iter()
        .find(|item| item["name"] == "models/gemini-next-flash-high")
        .expect("v1beta/models must contain gemini-next-flash-high");
    assert_eq!(beta_ag["displayName"], "gemini-next-flash-high");
    assert!(beta_ag["supportedGenerationMethods"].is_array());

    // Absent account models omitted from v1beta
    assert!(
        beta_models
            .iter()
            .all(|item| item["name"] != "models/model-claude-absent"),
        "absent-account models must be omitted from /v1beta/models"
    );

    // 3. GET single model /v1beta/models/gemini-next-flash-high
    let single_uri = format!("{base_url}/v1beta/models/gemini-next-flash-high");
    let resp_single = client
        .get(&single_uri)
        .header("Authorization", "Bearer test-admin")
        .header("Host", format!("127.0.0.1:{QA_PORT}"))
        .send()
        .await
        .expect("v1beta single model response");

    let single_status = resp_single.status();
    let single_headers = resp_single.headers().clone();
    let single_text = resp_single.text().await.expect("single model text");

    http_evidence.push_str(&format_http_exchange(
        "GET",
        "/v1beta/models/gemini-next-flash-high",
        &[
            ("Host", &format!("127.0.0.1:{QA_PORT}")),
            ("Authorization", "Bearer test-admin"),
        ],
        None,
        single_status,
        &single_headers,
        &single_text,
    ));

    assert_eq!(single_status, reqwest::StatusCode::OK);
    let single_json: Value = serde_json::from_str(&single_text).expect("single model parsed JSON");
    assert_eq!(single_json["name"], "models/gemini-next-flash-high");
    assert_eq!(single_json["displayName"], "gemini-next-flash-high");
    // Single model payload drops supportedGenerationMethods per v1beta contract
    assert!(single_json.get("supportedGenerationMethods").is_none());

    // 4. GET /v0/management/auth-files/models
    let mgmt_models_uri =
        format!("{base_url}/v0/management/auth-files/models?name=antigravity-1.json");
    let resp_mgmt = client
        .get(&mgmt_models_uri)
        .header("Authorization", "Bearer test-admin")
        .header("Host", format!("127.0.0.1:{QA_PORT}"))
        .send()
        .await
        .expect("auth-files/models response");

    let mgmt_status = resp_mgmt.status();
    let mgmt_headers = resp_mgmt.headers().clone();
    let mgmt_text = resp_mgmt.text().await.expect("auth-files/models text");

    http_evidence.push_str(&format_http_exchange(
        "GET",
        "/v0/management/auth-files/models?name=antigravity-1.json",
        &[
            ("Host", &format!("127.0.0.1:{QA_PORT}")),
            ("Authorization", "Bearer test-admin"),
        ],
        None,
        mgmt_status,
        &mgmt_headers,
        &mgmt_text,
    ));

    assert_eq!(mgmt_status, reqwest::StatusCode::OK);
    let mgmt_json: Value = serde_json::from_str(&mgmt_text).unwrap();
    let mgmt_data = mgmt_json["models"]["data"]
        .as_array()
        .expect("mgmt models data");
    assert!(mgmt_data
        .iter()
        .any(|item| item["id"] == "gemini-next-flash-high"));

    // 5. GET /v0/management/model-definitions/stable
    let defs_uri = format!("{base_url}/v0/management/model-definitions/stable");
    let resp_defs = client
        .get(&defs_uri)
        .header("Authorization", "Bearer test-admin")
        .header("Host", format!("127.0.0.1:{QA_PORT}"))
        .send()
        .await
        .expect("model-definitions response");

    let defs_status = resp_defs.status();
    let defs_headers = resp_defs.headers().clone();
    let defs_text = resp_defs.text().await.expect("model-definitions text");

    http_evidence.push_str(&format_http_exchange(
        "GET",
        "/v0/management/model-definitions/stable",
        &[
            ("Host", &format!("127.0.0.1:{QA_PORT}")),
            ("Authorization", "Bearer test-admin"),
        ],
        None,
        defs_status,
        &defs_headers,
        &defs_text,
    ));

    assert_eq!(defs_status, reqwest::StatusCode::OK);
    let defs_json: Value = serde_json::from_str(&defs_text).unwrap();
    assert_eq!(defs_json["channel"], "stable");
    assert_eq!(defs_json["models"], json!([]));

    // 6. Delete/disable the only account and rescan
    std::fs::write(&ag_cred_path, create_antigravity_cred("ag-test", true))
        .expect("updated account to disabled");
    let rescan_result = state.rescan_pool();
    assert!(rescan_result.is_ok());

    // 5. GET /v1/models after disable -> model disappears
    let resp_v1_disabled = client
        .get(&v1_uri)
        .header("Authorization", "Bearer test-admin")
        .header("Host", format!("127.0.0.1:{QA_PORT}"))
        .send()
        .await
        .expect("v1/models disabled response");

    let v1_dis_status = resp_v1_disabled.status();
    let v1_dis_headers = resp_v1_disabled.headers().clone();
    let v1_dis_text = resp_v1_disabled.text().await.expect("v1 text disabled");

    http_evidence.push_str(&format_http_exchange(
        "GET",
        "/v1/models",
        &[
            ("Host", &format!("127.0.0.1:{QA_PORT}")),
            ("Authorization", "Bearer test-admin"),
        ],
        None,
        v1_dis_status,
        &v1_dis_headers,
        &v1_dis_text,
    ));

    assert_eq!(v1_dis_status, reqwest::StatusCode::OK);
    let v1_dis_json: Value = serde_json::from_str(&v1_dis_text).unwrap();
    let v1_dis_data = v1_dis_json["data"].as_array().unwrap();
    assert!(
        v1_dis_data
            .iter()
            .all(|item| item["id"] != "gemini-next-flash-high"),
        "gemini-next-flash-high must disappear after disabling account"
    );

    // 6. GET /v1beta/models after disable -> model disappears
    let resp_beta_disabled = client
        .get(&beta_uri)
        .header("Authorization", "Bearer test-admin")
        .header("Host", format!("127.0.0.1:{QA_PORT}"))
        .send()
        .await
        .expect("v1beta disabled response");

    let beta_dis_status = resp_beta_disabled.status();
    let beta_dis_headers = resp_beta_disabled.headers().clone();
    let beta_dis_text = resp_beta_disabled.text().await.expect("beta text disabled");

    http_evidence.push_str(&format_http_exchange(
        "GET",
        "/v1beta/models",
        &[
            ("Host", &format!("127.0.0.1:{QA_PORT}")),
            ("Authorization", "Bearer test-admin"),
        ],
        None,
        beta_dis_status,
        &beta_dis_headers,
        &beta_dis_text,
    ));

    assert_eq!(beta_dis_status, reqwest::StatusCode::OK);
    let beta_dis_json: Value = serde_json::from_str(&beta_dis_text).unwrap();
    let beta_dis_models = beta_dis_json["models"].as_array().unwrap();
    assert!(
        beta_dis_models
            .iter()
            .all(|item| item["name"] != "models/gemini-next-flash-high"),
        "gemini-next-flash-high must disappear from v1beta after disabling account"
    );

    // 7. GET /v1beta/models/gemini-next-flash-high after disable -> 404 Not Found
    let resp_single_disabled = client
        .get(&single_uri)
        .header("Authorization", "Bearer test-admin")
        .header("Host", format!("127.0.0.1:{QA_PORT}"))
        .send()
        .await
        .expect("single model disabled response");

    let single_dis_status = resp_single_disabled.status();
    let single_dis_headers = resp_single_disabled.headers().clone();
    let single_dis_text = resp_single_disabled
        .text()
        .await
        .expect("single model dis text");

    http_evidence.push_str(&format_http_exchange(
        "GET",
        "/v1beta/models/gemini-next-flash-high",
        &[
            ("Host", &format!("127.0.0.1:{QA_PORT}")),
            ("Authorization", "Bearer test-admin"),
        ],
        None,
        single_dis_status,
        &single_dis_headers,
        &single_dis_text,
    ));

    assert_eq!(single_dis_status, reqwest::StatusCode::NOT_FOUND);

    // Save evidence to both repositories
    let evidence_path_proxy =
        PathBuf::from("/Users/indo/code/project/mahoquot-proxy/.omo/evidence/model-registry/task-12-model-apis.http");
    let evidence_path_quotio = PathBuf::from(
        "/Users/indo/code/project/quotio-rs/.omo/evidence/model-registry/task-12-model-apis.http",
    );

    std::fs::write(&evidence_path_proxy, &http_evidence).expect("wrote evidence to mahoquot-proxy");
    std::fs::write(&evidence_path_quotio, &http_evidence).expect("wrote evidence to quotio-rs");

    // Graceful cleanup
    let _ = shutdown_tx.send(());
    let _ = server_handle.await;

    // Verify port 18873 is released
    tokio::time::sleep(Duration::from_millis(50)).await;
    let bind_check = tokio::net::TcpListener::bind(format!("127.0.0.1:{QA_PORT}")).await;
    assert!(bind_check.is_ok(), "port 18873 must be cleanly released");
    drop(bind_check);

    // Remove temp directory
    let _ = std::fs::remove_dir_all(&dir);
}
