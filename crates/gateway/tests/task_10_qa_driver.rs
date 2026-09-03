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
use serde_json::json;

#[tokio::test]
async fn task_10_http_qa_scenario() {
    let dir = common::unique_temp_dir("mahoquot-task-10-qa");
    let config_path = dir.join("config.yaml");

    // Initial config: defines custom model gemini-next-flash-high with antigravity binding
    let custom_id = ModelId::new("gemini-next-flash-high").unwrap();
    let custom_desc = ModelDescriptor::new(custom_id.clone(), "google").with_binding(
        ProviderBinding::new(
            ProviderId::antigravity(),
            ProviderPolicy::Closed,
            CatalogSource::LocalOverride,
        )
        .with_capabilities([mahoquot_registry::ModelCapability::Chat]),
    );

    let init_settings = mahoquot_gateway::management::settings::Settings {
        port: 18871,
        api_keys: vec!["test-admin".to_string()],
        model_catalog: Some(ModelCatalogSettings {
            custom_models: vec![custom_desc],
            ..ModelCatalogSettings::default()
        }),
        ..Default::default()
    };

    init_settings
        .persist(&config_path)
        .expect("config persisted");

    let gateway_config = GatewayConfig {
        config_path: config_path.clone(),
        auth_dir: dir.join("auth"),
        api_keys: ApiKeys::new(vec!["test-admin".to_string()]),
        ..GatewayConfig::default()
    };
    std::fs::create_dir_all(dir.join("auth")).expect("auth dir created");

    let state = Arc::new(AppState::new(&gateway_config).expect("app state created"));
    let app = create_app(Arc::clone(&state));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:18871")
        .await
        .expect("bound to 127.0.0.1:18871");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    // Allow socket to listen
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = reqwest::Client::new();
    let uri = "http://127.0.0.1:18871/v0/management/oauth-model-alias";

    let mut http_evidence = String::new();
    let mut report_evidence = String::new();

    report_evidence
        .push_str("TASK 10: VALIDATE LOCAL ALIASES, EXCLUSIONS, AND OVERRIDES TRANSACTIONALLY\n");
    report_evidence
        .push_str("=========================================================================\n\n");

    // 1. Valid alias: gemini-next-flash-high <- flash-next
    let valid_payload = json!({
        "antigravity": [
            { "name": "gemini-next-flash-high", "alias": "flash-next" }
        ]
    });

    http_evidence.push_str("PUT /v0/management/oauth-model-alias HTTP/1.1\n");
    http_evidence.push_str("Host: 127.0.0.1:18871\n");
    http_evidence.push_str("Authorization: Bearer test-admin\n");
    http_evidence.push_str("Content-Type: application/json\n\n");
    http_evidence.push_str(&serde_json::to_string_pretty(&valid_payload).unwrap());
    http_evidence.push_str("\n\n");

    let resp = client
        .put(uri)
        .header("Authorization", "Bearer test-admin")
        .json(&valid_payload)
        .send()
        .await
        .expect("valid PUT request sent");

    let status = resp.status();
    let resp_headers = resp.headers().clone();
    let body_text = resp.text().await.expect("valid body text");

    http_evidence.push_str(&format!("HTTP/1.1 {}\n", status));
    for (k, v) in &resp_headers {
        http_evidence.push_str(&format!("{}: {}\n", k, v.to_str().unwrap_or("")));
    }
    http_evidence.push('\n');
    http_evidence.push_str(&body_text);
    http_evidence.push_str("\n\n---\n\n");

    assert_eq!(status, reqwest::StatusCode::OK);
    report_evidence.push_str(&format!(
        "[SCENARIO 1 - VALID ALIAS]\nStatus: {}\nResponse: {}\n",
        status, body_text
    ));

    // Verify local model resolution returns canonical ID
    let current_comp = state.runtime.composition();
    let resolved = current_comp
        .registry()
        .resolve("flash-next")
        .expect("resolves flash-next");
    assert_eq!(resolved.canonical_id.as_str(), "gemini-next-flash-high");
    report_evidence.push_str(&format!(
        "Model resolution for 'flash-next': canonical_id = '{}' (OK)\n\n",
        resolved.canonical_id
    ));

    let gen_before_cycle = state.runtime.generation();

    // 2. Cyclic alias variant: cycle-a -> cycle-b -> cycle-a
    let cyclic_payload = json!({
        "antigravity": [
            { "name": "cycle-b", "alias": "cycle-a" },
            { "name": "cycle-a", "alias": "cycle-b" }
        ]
    });

    http_evidence.push_str("PUT /v0/management/oauth-model-alias HTTP/1.1\n");
    http_evidence.push_str("Host: 127.0.0.1:18871\n");
    http_evidence.push_str("Authorization: Bearer test-admin\n");
    http_evidence.push_str("Content-Type: application/json\n\n");
    http_evidence.push_str(&serde_json::to_string_pretty(&cyclic_payload).unwrap());
    http_evidence.push_str("\n\n");

    let resp2 = client
        .put(uri)
        .header("Authorization", "Bearer test-admin")
        .json(&cyclic_payload)
        .send()
        .await
        .expect("cyclic PUT request sent");

    let status2 = resp2.status();
    let resp_headers2 = resp2.headers().clone();
    let body_text2 = resp2.text().await.expect("cyclic body text");

    http_evidence.push_str(&format!("HTTP/1.1 {}\n", status2));
    for (k, v) in &resp_headers2 {
        http_evidence.push_str(&format!("{}: {}\n", k, v.to_str().unwrap_or("")));
    }
    http_evidence.push('\n');
    http_evidence.push_str(&body_text2);
    http_evidence.push('\n');

    assert_eq!(status2, reqwest::StatusCode::BAD_REQUEST);
    assert!(body_text2.contains("cycle"));

    let gen_after_cycle = state.runtime.generation();
    assert_eq!(
        gen_before_cycle, gen_after_cycle,
        "generation must not advance on rejected update"
    );

    report_evidence.push_str(&format!(
        "[SCENARIO 2 - CYCLIC ALIAS REJECTION]\nStatus: {}\nResponse: {}\n",
        status2, body_text2
    ));
    report_evidence.push_str(&format!("Pool generation before cycle: {}\nPool generation after cycle: {}\nAtomic generation non-mutation confirmed.\n\n", gen_before_cycle, gen_after_cycle));

    // 3. Unknown alias target rejection
    let unknown_payload = json!({
        "antigravity": [
            { "name": "completely-unknown-void-model", "alias": "flash-void" }
        ]
    });
    let resp3 = client
        .put(uri)
        .header("Authorization", "Bearer test-admin")
        .json(&unknown_payload)
        .send()
        .await
        .expect("unknown PUT request sent");

    let status3 = resp3.status();
    let body_text3 = resp3.text().await.expect("unknown body text");
    assert_eq!(status3, reqwest::StatusCode::BAD_REQUEST);
    assert!(body_text3.contains("unknown target"));
    report_evidence.push_str(&format!(
        "[SCENARIO 3 - UNKNOWN TARGET REJECTION]\nStatus: {}\nResponse: {}\n",
        status3, body_text3
    ));
    report_evidence.push_str("Unknown alias target rejection confirmed.\n\n");

    // 4. Provider blackout rejection via /oauth-excluded-models
    let excluded_uri = "http://127.0.0.1:18871/v0/management/oauth-excluded-models";
    let all_ag_models = vec![
        "gemini-3.8-flash-high",
        "gemini-3.7-flash-high",
        "gemini-3.6-flash-high",
        "gemini-3.5-flash-low",
        "gemini-3.5-flash-extra-low",
        "gemini-3.1-flash-lite",
        "gemini-3.1-flash-image",
        "gemini-3.1-pro-low",
        "gemini-3-flash",
        "gemini-3-flash-agent",
        "gemini-pro-agent",
        "claude-sonnet-4-6",
        "claude-opus-4-6-thinking",
        "gpt-oss-120b-medium",
        "gemini-next-flash-high",
    ];
    let blackout_payload = json!({
        "antigravity": all_ag_models
    });
    let resp4 = client
        .put(excluded_uri)
        .header("Authorization", "Bearer test-admin")
        .json(&blackout_payload)
        .send()
        .await
        .expect("blackout PUT request sent");

    let status4 = resp4.status();
    let body_text4 = resp4.text().await.expect("blackout body text");
    assert_eq!(status4, reqwest::StatusCode::BAD_REQUEST);
    assert!(body_text4.contains("provider blackout"));
    report_evidence.push_str(&format!(
        "[SCENARIO 4 - PROVIDER BLACKOUT REJECTION]\nStatus: {}\nResponse: {}\n",
        status4, body_text4
    ));
    report_evidence.push_str("Complete provider blackout rejection confirmed.\n\n");

    // Gracefully shut down server
    let _ = shutdown_tx.send(());
    server_handle.await.expect("server gracefully stopped");

    // Write evidence files
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let evidence_dir = repo_root.join(".omo/evidence/model-registry");
    std::fs::create_dir_all(&evidence_dir).expect("evidence dir created");
    std::fs::write(evidence_dir.join("task-10-settings.http"), &http_evidence)
        .expect("wrote task-10-settings.http");
    std::fs::write(
        evidence_dir.join("task-10-invalid-settings-rejection.txt"),
        &report_evidence,
    )
    .expect("wrote task-10-invalid-settings-rejection.txt");

    // Cleanup temp dir
    let _ = std::fs::remove_dir_all(&dir);
}
