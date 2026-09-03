use std::path::PathBuf;

use mahoquot_gateway::management::settings::{ModelCatalogSettings, Settings, SettingsError};
use mahoquot_gateway::management::store::SettingsStore;
use mahoquot_registry::{
    ModelDescriptor, ModelId, ProviderBinding, ProviderId, ProviderPolicy, RegistryError,
};
use serde_json::json;

fn temp_store(tag: &str) -> (SettingsStore, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "mahoquot-test-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir created");
    let path = dir.join("config.yaml");
    let store =
        SettingsStore::load_or(path.clone(), Settings::default()).expect("store initialized");
    (store, path)
}

#[test]
fn test_unknown_alias_target_rejection() {
    let (store, path) = temp_store("unknown-alias");

    // Live document starts with default valid settings
    let initial_yaml = std::fs::read_to_string(&path).expect("initial file exists");
    assert_eq!(store.current().oauth_model_alias, serde_json::Value::Null);

    // Attempt to set an alias pointing to a model that does not exist
    let result = store.mutate(|s| {
        s.oauth_model_alias = json!({
            "my-custom-alias": "void-model-never-registered-xyz"
        });
    });

    // 1. Invariant: Alias target must exist in active registry snapshot
    match result {
        Err(SettingsError::Validation(RegistryError::UnknownAliasTarget { alias, target })) => {
            assert_eq!(alias.as_str(), "my-custom-alias");
            assert_eq!(target.as_str(), "void-model-never-registered-xyz");
        }
        other => panic!("expected UnknownAliasTarget rejection, got {other:?}"),
    }

    // 2. Invariant: Atomic non-mutation (in-memory document completely intact)
    assert_eq!(store.current().oauth_model_alias, serde_json::Value::Null);

    // 3. Invariant: Disk file completely intact (zero partial application)
    let current_yaml = std::fs::read_to_string(&path).expect("file readable");
    assert_eq!(current_yaml, initial_yaml);
}

#[test]
fn test_alias_cycle_rejection() {
    let (store, path) = temp_store("alias-cycle");
    let initial_yaml = std::fs::read_to_string(&path).expect("initial file exists");

    // 1. Direct cycle: A -> A
    let direct_result = store.mutate(|s| {
        s.oauth_model_alias = json!({
            "model-self": "model-self"
        });
    });
    match direct_result {
        Err(SettingsError::Validation(RegistryError::AliasCycle { alias, cycle })) => {
            assert_eq!(alias.as_str(), "model-self");
            assert!(cycle.contains(&ModelId::new("model-self").unwrap()));
        }
        other => panic!("expected direct AliasCycle rejection, got {other:?}"),
    }
    assert_eq!(store.current().oauth_model_alias, serde_json::Value::Null);

    // 2. Indirect cycle: A -> B -> A
    let indirect_result = store.mutate(|s| {
        s.oauth_model_alias = json!({
            "cycle-a": "cycle-b",
            "cycle-b": "cycle-a"
        });
    });
    match indirect_result {
        Err(SettingsError::Validation(RegistryError::AliasCycle { alias, .. })) => {
            assert!(alias.as_str() == "cycle-a" || alias.as_str() == "cycle-b");
        }
        other => panic!("expected indirect AliasCycle rejection, got {other:?}"),
    }
    assert_eq!(store.current().oauth_model_alias, serde_json::Value::Null);

    // 3. Three-hop cycle: A -> B -> C -> A
    let three_hop_result = store.mutate(|s| {
        s.oauth_model_alias = json!({
            "a": "b",
            "b": "c",
            "c": "a"
        });
    });
    match three_hop_result {
        Err(SettingsError::Validation(RegistryError::AliasCycle { .. })) => {}
        other => panic!("expected three-hop AliasCycle rejection, got {other:?}"),
    }
    assert_eq!(store.current().oauth_model_alias, serde_json::Value::Null);

    // 4. Exceeded depth: depth > 10 (11 hops to a real catalog model)
    // Chain: m0 -> m1 -> m2 -> ... -> m10 -> claude-sonnet-4-6 (11 hops total)
    let depth_exceeded_result = store.mutate(|s| {
        s.oauth_model_alias = json!({
            "m0": "m1",
            "m1": "m2",
            "m2": "m3",
            "m3": "m4",
            "m4": "m5",
            "m5": "m6",
            "m6": "m7",
            "m7": "m8",
            "m8": "m9",
            "m9": "m10",
            "m10": "claude-sonnet-4-6"
        });
    });
    match depth_exceeded_result {
        Err(SettingsError::Validation(RegistryError::AliasDepthExceeded {
            alias,
            depth,
            max_depth,
        })) => {
            assert_eq!(alias.as_str(), "m0");
            assert_eq!(depth, 11);
            assert_eq!(max_depth, 10);
        }
        other => panic!("expected AliasDepthExceeded rejection, got {other:?}"),
    }

    // Atomic non-mutation: file and live document unchanged
    let current_yaml = std::fs::read_to_string(&path).expect("file readable");
    assert_eq!(current_yaml, initial_yaml);
}

#[test]
fn test_complete_provider_blackout_rejection() {
    let (store, path) = temp_store("provider-blackout");
    let initial_yaml = std::fs::read_to_string(&path).expect("initial file exists");

    // All 14 fallback-routable models for Antigravity in the active catalog
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
    ];

    // Attempting to exclude 100% of Antigravity models without an explicit override
    let blackout_result = store.mutate(|s| {
        s.oauth_excluded_models.insert(
            "antigravity".to_string(),
            all_ag_models.iter().map(|s| s.to_string()).collect(),
        );
    });

    match blackout_result {
        Err(SettingsError::Validation(RegistryError::ProviderBlackout { provider_id })) => {
            assert_eq!(provider_id.as_str(), "antigravity");
        }
        other => panic!("expected ProviderBlackout rejection, got {other:?}"),
    }

    // In-memory document and disk remain intact
    assert!(store.current().oauth_excluded_models.is_empty());
    let current_yaml = std::fs::read_to_string(&path).expect("file readable");
    assert_eq!(current_yaml, initial_yaml);

    // Partial exclusion (e.g. 13 of 14 models excluded, 1 model remains) is permitted
    let partial_result = store.mutate(|s| {
        s.oauth_excluded_models.insert(
            "antigravity".to_string(),
            all_ag_models[..13].iter().map(|s| s.to_string()).collect(),
        );
    });
    assert!(partial_result.is_ok(), "partial exclusion must succeed");
    assert_eq!(
        store
            .current()
            .oauth_excluded_models
            .get("antigravity")
            .unwrap()
            .len(),
        13
    );

    // With explicit override: user specifies `allowed-blackouts` containing `antigravity`
    let full_with_override = store.mutate(|s| {
        s.model_catalog = Some(ModelCatalogSettings {
            allowed_blackouts: vec!["antigravity".to_string()],
            ..ModelCatalogSettings::default()
        });
        s.oauth_excluded_models.insert(
            "antigravity".to_string(),
            all_ag_models.iter().map(|s| s.to_string()).collect(),
        );
    });
    assert!(
        full_with_override.is_ok(),
        "blackout with explicit override must succeed"
    );
    assert_eq!(
        store
            .current()
            .oauth_excluded_models
            .get("antigravity")
            .unwrap()
            .len(),
        14
    );
}

#[test]
fn test_atomic_rollback_and_non_mutation_on_reload() {
    let (store, path) = temp_store("atomic-rollback");

    // 1. Establish valid live document
    store
        .mutate(|s| {
            s.request_retry = 7;
            s.oauth_model_alias = json!({
                "claude-latest": "claude-sonnet-4-6"
            });
        })
        .expect("valid initial mutate succeeds");

    assert_eq!(store.current().request_retry, 7);
    let valid_yaml = std::fs::read_to_string(&path).expect("file readable");
    assert!(valid_yaml.contains("request-retry: 7"));

    // 2. Failed mutation leaves memory and disk unchanged
    let mutate_fail = store.mutate(|s| {
        s.request_retry = 99;
        s.oauth_model_alias = json!({
            "bad": "bad" // cyclic alias
        });
    });
    assert!(mutate_fail.is_err());
    assert_eq!(store.current().request_retry, 7);
    assert_eq!(
        store.current().oauth_model_alias,
        json!({"claude-latest": "claude-sonnet-4-6"})
    );
    let disk_yaml_after_failed_mutate = std::fs::read_to_string(&path).expect("file readable");
    assert_eq!(disk_yaml_after_failed_mutate, valid_yaml);

    // 3. External disk corruption / invalid settings on reload
    // Corrupt the disk file with an alias pointing to void
    let corrupted_yaml =
        valid_yaml.replace("claude-sonnet-4-6", "unknown-target-pointing-to-void-404");
    std::fs::write(&path, corrupted_yaml).expect("corrupted file written");

    // Call store.reload()
    let reload_result = store.reload();
    match reload_result {
        Err(SettingsError::Validation(RegistryError::UnknownAliasTarget { .. })) => {}
        other => panic!("expected UnknownAliasTarget on reload, got {other:?}"),
    }

    // In-memory document was NOT replaced: still holds request_retry = 7 and valid claude-sonnet-4-6 alias
    assert_eq!(store.current().request_retry, 7);
    assert_eq!(
        store.current().oauth_model_alias,
        json!({"claude-latest": "claude-sonnet-4-6"})
    );
}

#[test]
fn test_old_yaml_roundtrips_byte_semantically() {
    let raw = "port: 18801\nauth-dir: /tmp/auth\nrequest-retry: 5\noauth-excluded-models:\n  openai:\n    - gpt-4-old\n";
    let settings = Settings::from_yaml(raw).expect("parses old YAML");
    assert_eq!(settings.port, 18801);
    assert_eq!(settings.request_retry, 5);
    assert_eq!(settings.model_catalog, None);

    let rendered = settings.to_yaml().expect("renders back to YAML");
    // Does not inject "model-catalog" section
    assert!(
        !rendered.contains("model-catalog:"),
        "must not add model-catalog key: {rendered}"
    );

    let reparsed = Settings::from_yaml(&rendered).expect("reparses cleanly");
    assert_eq!(settings, reparsed);
}

#[test]
fn test_unsafe_url_rejection() {
    let (store, _) = temp_store("unsafe-urls");

    // 1. Scheme other than https / localhost http
    let insecure_http = store.mutate(|s| {
        s.model_catalog = Some(ModelCatalogSettings {
            url: "http://insecure-public-host.com/models.json".to_string(),
            ..ModelCatalogSettings::default()
        });
    });
    match insecure_http {
        Err(SettingsError::InvalidCatalogConfig(msg)) => {
            assert!(msg.contains("insecure"), "got {msg}");
        }
        other => panic!("expected insecure url rejection, got {other:?}"),
    }

    // 2. file:// scheme
    let file_scheme = store.mutate(|s| {
        s.model_catalog = Some(ModelCatalogSettings {
            url: "file:///etc/passwd".to_string(),
            ..ModelCatalogSettings::default()
        });
    });
    assert!(file_scheme.is_err());

    // 3. Embedded credentials
    let creds_url = store.mutate(|s| {
        s.model_catalog = Some(ModelCatalogSettings {
            url: "https://user:password@raw.githubusercontent.com/catalog.json".to_string(),
            ..ModelCatalogSettings::default()
        });
    });
    match creds_url {
        Err(SettingsError::InvalidCatalogConfig(msg)) => {
            assert!(msg.contains("credentials"), "got {msg}");
        }
        other => panic!("expected embedded credentials rejection, got {other:?}"),
    }

    // 4. Valid localhost http is accepted (for local test fixtures)
    let local_http = store.mutate(|s| {
        s.model_catalog = Some(ModelCatalogSettings {
            url: "http://127.0.0.1:18870/models.json".to_string(),
            signature_url: "http://127.0.0.1:18870/models.json.sig".to_string(),
            ..ModelCatalogSettings::default()
        });
    });
    assert!(
        local_http.is_ok(),
        "localhost http must be accepted for local tests"
    );
}

#[test]
fn test_custom_provider_models_validation() {
    let (store, _) = temp_store("custom-models");

    // Add a custom model descriptor to model-catalog
    let custom_id = ModelId::new("gemini-next-flash-high").unwrap();
    let custom_desc = ModelDescriptor::new(custom_id.clone(), "google").with_binding(
        ProviderBinding::new(
            ProviderId::antigravity(),
            ProviderPolicy::Closed,
            mahoquot_registry::CatalogSource::LocalOverride,
        )
        .with_capabilities([mahoquot_registry::ModelCapability::Chat]),
    );

    // Mutate settings with custom model and an alias pointing to it
    let result = store.mutate(|s| {
        s.model_catalog = Some(ModelCatalogSettings {
            custom_models: vec![custom_desc],
            ..ModelCatalogSettings::default()
        });
        s.oauth_model_alias = json!({
            "antigravity": [
                { "name": "gemini-next-flash-high", "alias": "flash-next" }
            ]
        });
    });
    assert!(
        result.is_ok(),
        "custom model and alias pointing to it must succeed"
    );

    // Target alias resolves against the candidate snapshot
    let snapshot = store.active_snapshot();
    let candidate = store
        .current()
        .validate_against_registry(&snapshot)
        .expect("candidate composition valid");
    let resolved = candidate
        .resolve("flash-next")
        .expect("resolves flash-next");
    assert_eq!(resolved.canonical_id.as_str(), "gemini-next-flash-high");
}
