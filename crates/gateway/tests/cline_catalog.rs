mod common;

use axum::{routing::get, Json, Router};
use mahoquot_gateway::{cline_catalog, config::GatewayConfig, state::AppState};
use serde_json::{json, Value};
use std::sync::{Arc, RwLock};

#[tokio::test]
async fn official_free_catalog_updates_accounts_runtime_and_profile_and_retains_on_failure() {
    let dir = common::unique_temp_dir("cline-catalog");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, extra) in [("a", "paid/a"), ("b", "paid/b")] {
        std::fs::write(
            dir.join(format!("generic-{name}.json")),
            json!({
                "type":"generic", "provider":"cline", "adapter":"openai-chat",
                "base_url":"https://api.cline.bot/api/v1", "api_key":format!("secret-{name}"),
                "models":[extra,"cline-free/retired"], "custom":name
            })
            .to_string(),
        )
        .unwrap();
    }
    let config = GatewayConfig {
        auth_dir: dir.clone(),
        config_path: dir.join("config.yaml"),
        auth_refresh_enabled: false,
        ..GatewayConfig::default()
    };
    let state = Arc::new(AppState::new(&config).unwrap());
    state
        .settings
        .mutate(|settings| {
            let p = settings.omo_profile.get_or_insert_default();
            p.revision = 2;
            p.compat.insert("custom".into(), json!(true));
            p.models = vec![
                json!({"id":"other/model"}).as_object().unwrap().clone(),
                json!({"id":"cline-free/retired"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ];
            p.remove = vec!["gpt-5.6-sol".into()];
        })
        .unwrap();
    let document = Arc::new(RwLock::new(json!({"free":[
        {"id":"vendor/new-free","name":"New"},
        {"id":"cline-free/new","name":"New Cline"}
    ],"recommended":[{"id":"paid/not-free","name":"Paid"}]})));
    let response = Arc::clone(&document);
    let app = Router::new()
        .route(
            "/ai/cline/models",
            get(|| async {
                Json(json!({"data":[
                    {"id":"vendor/new:free", "context_length":12345,
                     "top_provider":{"max_completion_tokens":2345},
                     "architecture":{"input_modalities":["text","image","video"]},
                     "supported_parameters":["reasoning","tools"]}
                ]}))
            }),
        )
        .route(
            "/ai/cline/recommended-models",
            get(move || {
                let response = Arc::clone(&response);
                async move { Json(response.read().unwrap().clone()) }
            }),
        );
    let mut bound = None;
    for port in 18840..=18899 {
        if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            bound = Some(listener);
            break;
        }
    }
    let listener = bound.expect("test port in 18840-18899");
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://{address}");
    cline_catalog::refresh(Arc::clone(&state), &base)
        .await
        .unwrap();
    for (name, extra) in [("a", "paid/a"), ("b", "paid/b")] {
        let account: Value = serde_json::from_slice(
            &std::fs::read(dir.join(format!("generic-{name}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(
            account["models"],
            json!([extra, "cline-free/new", "vendor/new-free"])
        );
        assert_eq!(account["api_key"], format!("secret-{name}"));
        assert_eq!(account["custom"], name);
    }
    assert!(state
        .pool
        .load()
        .members
        .iter()
        .all(|m| m.supports_model("vendor/new-free") && !m.supports_model("cline-free/retired")));
    assert!(state
        .runtime
        .load()
        .models()
        .iter()
        .any(|m| m.id == "vendor/new-free"));
    let profile = state.settings.current().omo_profile.clone().unwrap();
    assert_eq!(profile.revision, 3);
    assert_eq!(profile.compat["custom"], true);
    assert!(profile.remove.contains(&"gpt-5.6-sol".into()));
    assert!(profile.remove.contains(&"cline-free/retired".into()));
    assert!(profile.models.iter().any(|m| m["id"] == "other/model"));
    let discovered = profile
        .models
        .iter()
        .find(|m| m["id"] == "vendor/new-free")
        .unwrap();
    assert_eq!(discovered["contextWindow"], 12345);
    assert_eq!(discovered["maxTokens"], 2345);
    assert_eq!(discovered["input"], json!(["text", "image"]));
    assert_eq!(discovered["reasoning"], true);
    assert_eq!(discovered["cost"]["input"], 0);
    cline_catalog::refresh(Arc::clone(&state), &base)
        .await
        .unwrap();
    assert_eq!(
        state
            .settings
            .current()
            .omo_profile
            .as_ref()
            .unwrap()
            .revision,
        3
    );
    *document.write().unwrap() = json!({"recommended":[]});
    assert!(cline_catalog::refresh(Arc::clone(&state), &base)
        .await
        .is_err());
    assert_eq!(
        state.settings.current().omo_profile.as_ref().unwrap(),
        &profile
    );
    *document.write().unwrap() = json!({"free":[{"id":"cline-free/new","name":"New Cline"}]});
    cline_catalog::refresh(Arc::clone(&state), &base)
        .await
        .unwrap();
    let profile = state.settings.current().omo_profile.clone().unwrap();
    assert_eq!(profile.revision, 4);
    assert!(profile.remove.contains(&"vendor/new-free".into()));
    assert!(state
        .pool
        .load()
        .members
        .iter()
        .all(|m| !m.supports_model("vendor/new-free")));
    *document.write().unwrap() =
        json!({"free":[{"id":"same","name":"a"},{"id":"same","name":"b"}]});
    assert!(cline_catalog::refresh(Arc::clone(&state), &base)
        .await
        .is_err());
    assert_eq!(
        state.settings.current().omo_profile.as_ref().unwrap(),
        &profile
    );
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    assert!(cline_catalog::refresh(Arc::clone(&state), &base)
        .await
        .is_err());
    assert_eq!(
        state.settings.current().omo_profile.as_ref().unwrap(),
        &profile
    );
    std::fs::remove_dir_all(dir).unwrap();
}
