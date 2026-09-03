use mahoquot_registry::{
    embedded_catalog, embedded_catalog_bytes, embedded_registry_snapshot, CatalogSource,
    ModelCapability, ModelId, ProviderId, ProviderPolicy, RegistrySnapshot,
};

// 1. Antigravity models (14)
const LEGACY_ANTIGRAVITY_MODELS: [&str; 14] = [
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

// 2. Claude models (12)
const LEGACY_CLAUDE_MODELS: [&str; 12] = [
    "claude-sonnet-4-6",
    "claude-sonnet-4-5",
    "claude-sonnet-4-5-20250929",
    "claude-sonnet-4-5-20250929-thinking",
    "claude-opus-4-6",
    "claude-opus-4-5",
    "claude-opus-4-5-20251101",
    "claude-opus-4-5-20251101-thinking",
    "claude-haiku-4-5",
    "claude-haiku-4-5-20251001",
    "claude-3-7-sonnet-20250219",
    "claude-3-5-sonnet-20241022",
];

// 3. Zcode models (5)
const LEGACY_ZCODE_MODELS: [&str; 5] =
    ["glm-5.3", "glm-5.3-flash", "glm-5.2", "glm-5.1", "glm-4.6"];

// 4. Kiro models (8) - exposed as kiro/{model}
const LEGACY_KIRO_MODELS: [&str; 8] = [
    "auto",
    "claude-sonnet-4.6",
    "claude-opus-4.6",
    "claude-haiku-4.5",
    "claude-sonnet-4-5-20250929",
    "claude-sonnet-4-5-20250929-thinking",
    "claude-haiku-4-5-20251001",
    "claude-haiku-4-5-20251001-thinking",
];

// 5. Cursor models (4 in /v1/models + 2 cursor-*)
const LEGACY_CURSOR_MODELS: [&str; 6] = [
    "cursor/auto",
    "cursor/auto-cost",
    "cursor/auto-balance",
    "cursor/auto-intelligence",
    "cursor-small",
    "cursor-fast",
];

// 6. Vertex models (21)
const LEGACY_VERTEX_MODELS: [&str; 21] = [
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "gemini-2.0-flash",
    "gemini-2.0-flash-001",
    "gemini-2.0-pro-exp-02-05",
    "gemini-1.5-pro",
    "gemini-1.5-pro-001",
    "gemini-1.5-pro-002",
    "gemini-1.5-flash",
    "gemini-1.5-flash-001",
    "gemini-1.5-flash-002",
    "gemini-1.5-flash-8b",
    "gemini-3-pro",
    "gemini-3-flash",
    "gemini-3.1-pro-preview",
    "gemini-3.1-flash-lite",
    "gemini-3-flash-preview",
    "gemini-3.7-flash",
    "gemini-3.7-flash-thinking",
    "gemini-3.5-flash",
    "gemini-3.6-flash",
];

// 7. Codex default models (7)
const LEGACY_CODEX_DEFAULTS: [&str; 7] = [
    "gpt-5.6-sol",
    "gpt-5.6-luna",
    "gpt-5.6-terra",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.3-codex-spark",
];

// 8. Device OAuth models
const LEGACY_KIMI_MODELS: [&str; 5] = ["k3", "k3[1m]", "kimi-k2.7-code", "kimi-k2.6", "kimi-k2.5"];
const LEGACY_QWEN_MODELS: [&str; 4] = [
    "qwen3.8-max",
    "qwen3.7-max",
    "qwen3.7-plus",
    "qwen3.6-flash",
];
const LEGACY_NOUS_MODELS: [&str; 4] = [
    "tencent/hy3:free",
    "poolside/laguna-s-2.1:free",
    "stepfun/step-3.7-flash:free",
    "poolside/laguna-xs-2.1:free",
];
const LEGACY_COPILOT_MODELS: [&str; 5] =
    ["gpt-4o", "gpt-4.1", "gpt-5.3-codex", "gpt-5.4", "gpt-5.5"];

// 9. Image models
const LEGACY_IMAGE_MODELS: [&str; 2] = ["gpt-image-1.5", "gpt-image-2"];

#[test]
fn embedded_catalog_covers_legacy_sources() {
    let snapshot = embedded_registry_snapshot()
        .expect("embedded catalog must parse and validate successfully");

    assert_eq!(snapshot.source(), CatalogSource::EmbeddedFallback);

    let mut missing_errors = Vec::new();

    // 1. Verify all 14 Antigravity models
    let ag_provider = ProviderId::antigravity();
    for id in LEGACY_ANTIGRAVITY_MODELS {
        let model_id = ModelId::new(id).unwrap();
        match snapshot.get_model(&model_id) {
            Some(desc) => {
                if !desc.bindings.contains_key(&ag_provider) {
                    missing_errors.push(format!(
                        "Antigravity model {id} missing antigravity binding"
                    ));
                }
            }
            None => {
                missing_errors.push(format!("Antigravity model {id} missing from catalog"));
            }
        }
    }

    // 2. Verify all 12 Claude models
    let claude_provider = ProviderId::claude();
    for id in LEGACY_CLAUDE_MODELS {
        let model_id = ModelId::new(id).unwrap();
        match snapshot.get_model(&model_id) {
            Some(desc) => {
                if !desc.bindings.contains_key(&claude_provider) {
                    missing_errors.push(format!("Claude model {id} missing claude binding"));
                }
            }
            None => {
                missing_errors.push(format!("Claude model {id} missing from catalog"));
            }
        }
    }

    // 3. Verify all 5 Zcode models
    let zcode_provider = ProviderId::zcode();
    for id in LEGACY_ZCODE_MODELS {
        let model_id = ModelId::new(id).unwrap();
        match snapshot.get_model(&model_id) {
            Some(desc) => {
                if !desc.bindings.contains_key(&zcode_provider) {
                    missing_errors.push(format!("Zcode model {id} missing zcode binding"));
                }
            }
            None => {
                missing_errors.push(format!("Zcode model {id} missing from catalog"));
            }
        }
    }

    // 4. Verify all 8 Kiro models (as kiro/{model})
    let kiro_provider = ProviderId::kiro();
    for raw in LEGACY_KIRO_MODELS {
        let prefixed = format!("kiro/{raw}");
        let model_id = ModelId::new(&prefixed).unwrap();
        match snapshot.get_model(&model_id) {
            Some(desc) => match desc.bindings.get(&kiro_provider) {
                Some(b) => {
                    if b.upstream_model_id.as_deref() != Some(raw) {
                        missing_errors.push(format!(
                            "Kiro model {prefixed} upstream_model_id expected '{raw}', got '{:?}'",
                            b.upstream_model_id
                        ));
                    }
                }
                None => {
                    missing_errors.push(format!("Kiro model {prefixed} missing kiro binding"));
                }
            },
            None => {
                missing_errors.push(format!("Kiro model {prefixed} missing from catalog"));
            }
        }
    }

    // Verify auto-kiro alias
    match snapshot.resolve("auto-kiro") {
        Ok(resolved_kiro) => {
            if resolved_kiro.canonical_id.as_str() != "kiro/auto" {
                missing_errors.push(format!(
                    "auto-kiro alias expected to resolve to 'kiro/auto', got '{}'",
                    resolved_kiro.canonical_id
                ));
            }
        }
        Err(e) => {
            missing_errors.push(format!("auto-kiro alias failed to resolve: {e:?}"));
        }
    }

    // 5. Verify Cursor models
    let cursor_provider = ProviderId::cursor();
    for id in LEGACY_CURSOR_MODELS {
        let model_id = ModelId::new(id).unwrap();
        match snapshot.get_model(&model_id) {
            Some(desc) => {
                if !desc.bindings.contains_key(&cursor_provider) {
                    missing_errors.push(format!("Cursor model {id} missing cursor binding"));
                }
            }
            None => {
                missing_errors.push(format!("Cursor model {id} missing from catalog"));
            }
        }
    }

    // 6. Verify all 21 Vertex models
    let vertex_provider = ProviderId::vertex();
    for id in LEGACY_VERTEX_MODELS {
        let model_id = ModelId::new(id).unwrap();
        match snapshot.get_model(&model_id) {
            Some(desc) => {
                if !desc.bindings.contains_key(&vertex_provider) {
                    missing_errors.push(format!("Vertex model {id} missing vertex binding"));
                }
            }
            None => {
                missing_errors.push(format!("Vertex model {id} missing from catalog"));
            }
        }
    }

    // 7. Verify all 7 Codex defaults
    let codex_provider = ProviderId::codex();
    for id in LEGACY_CODEX_DEFAULTS {
        let model_id = ModelId::new(id).unwrap();
        match snapshot.get_model(&model_id) {
            Some(desc) => {
                if !desc.bindings.contains_key(&codex_provider) {
                    missing_errors.push(format!("Codex model {id} missing codex binding"));
                }
            }
            None => {
                missing_errors.push(format!("Codex model {id} missing from catalog"));
            }
        }
    }

    // 8. Verify Device OAuth models
    let kimi_provider = ProviderId::new("kimi").unwrap();
    for id in LEGACY_KIMI_MODELS {
        let model_id = ModelId::new(id).unwrap();
        match snapshot.get_model(&model_id) {
            Some(desc) => {
                if !desc.bindings.contains_key(&kimi_provider) {
                    missing_errors.push(format!("Kimi model {id} missing kimi binding"));
                }
            }
            None => {
                missing_errors.push(format!("Kimi model {id} missing from catalog"));
            }
        }
    }

    let qwen_provider = ProviderId::new("qwen").unwrap();
    for id in LEGACY_QWEN_MODELS {
        let model_id = ModelId::new(id).unwrap();
        match snapshot.get_model(&model_id) {
            Some(desc) => {
                if !desc.bindings.contains_key(&qwen_provider) {
                    missing_errors.push(format!("Qwen model {id} missing qwen binding"));
                }
            }
            None => {
                missing_errors.push(format!("Qwen model {id} missing from catalog"));
            }
        }
    }

    let nous_provider = ProviderId::new("nous").unwrap();
    for id in LEGACY_NOUS_MODELS {
        let model_id = ModelId::new(id).unwrap();
        match snapshot.get_model(&model_id) {
            Some(desc) => {
                if !desc.bindings.contains_key(&nous_provider) {
                    missing_errors.push(format!("Nous model {id} missing nous binding"));
                }
            }
            None => {
                missing_errors.push(format!("Nous model {id} missing from catalog"));
            }
        }
    }

    let copilot_provider = ProviderId::new("github-copilot").unwrap();
    for id in LEGACY_COPILOT_MODELS {
        let model_id = ModelId::new(id).unwrap();
        match snapshot.get_model(&model_id) {
            Some(desc) => {
                if !desc.bindings.contains_key(&copilot_provider) {
                    missing_errors
                        .push(format!("Copilot model {id} missing github-copilot binding"));
                }
            }
            None => {
                missing_errors.push(format!("Copilot model {id} missing from catalog"));
            }
        }
    }

    // 9. Verify Image models allowlist
    for id in LEGACY_IMAGE_MODELS {
        let model_id = ModelId::new(id).unwrap();
        match snapshot.get_model(&model_id) {
            Some(desc) => {
                if !desc
                    .effective_capabilities()
                    .contains(&ModelCapability::Image)
                {
                    missing_errors.push(format!("Image model {id} missing Image capability"));
                }
            }
            None => {
                missing_errors.push(format!("Image model {id} missing from catalog"));
            }
        }
    }

    // gemini-3.1-flash-image must also have Image capability
    let flash_img = ModelId::new("gemini-3.1-flash-image").unwrap();
    if let Some(desc) = snapshot.get_model(&flash_img) {
        if !desc
            .effective_capabilities()
            .contains(&ModelCapability::Image)
        {
            missing_errors.push("gemini-3.1-flash-image missing Image capability".to_string());
        }
    }

    // 10. Verify Video surface models are empty
    for (id, desc) in snapshot.models() {
        if desc
            .effective_capabilities()
            .contains(&ModelCapability::Video)
        {
            missing_errors.push(format!("Unexpected Video capability on model {id}"));
        }
    }

    // 11. Verify context limits are populated in metadata
    for (id, desc) in snapshot.models() {
        if !desc.metadata.contains_key("context_limit") {
            missing_errors.push(format!("Model {id} missing context_limit in metadata"));
        }
    }

    if !missing_errors.is_empty() {
        panic!(
            "Embedded catalog failed legacy source coverage with {} errors:\n{}",
            missing_errors.len(),
            missing_errors.join("\n")
        );
    }
}

#[test]
fn embedded_catalog_roundtrips_deterministically() {
    let raw = embedded_catalog();
    assert!(!raw.is_empty(), "embedded catalog JSON must not be empty");

    let bytes = embedded_catalog_bytes();
    assert_eq!(bytes, raw.as_bytes());

    let snapshot = embedded_registry_snapshot()
        .expect("embedded catalog must parse into valid RegistrySnapshot");

    // Serialization roundtrip
    let canonical = snapshot
        .to_json_canonical()
        .expect("snapshot must serialize to canonical JSON");
    let reparsed =
        RegistrySnapshot::from_json(&canonical).expect("canonical JSON must re-parse cleanly");

    assert_eq!(snapshot, reparsed);

    // Re-serialization must be byte-for-byte identical
    let recanonical = reparsed
        .to_json_canonical()
        .expect("reparsed snapshot must serialize");
    assert_eq!(canonical, recanonical);

    // Assert zero invalid IDs and zero duplicate owner conflicts
    for (model_id, desc) in snapshot.models() {
        assert_eq!(
            model_id.as_str(),
            desc.id.as_str(),
            "model key must match descriptor id"
        );
        assert!(
            !desc.owned_by.is_empty(),
            "owned_by must not be empty for {model_id}"
        );
        for (provider_id, binding) in &desc.bindings {
            assert_eq!(
                provider_id, &binding.provider_id,
                "binding key must match provider_id"
            );
        }
    }
}

#[test]
fn test_embedded_catalog_resolves_closed_and_open_models() {
    let snapshot = embedded_registry_snapshot().unwrap();

    // Closed Antigravity model
    let resolved_ag = snapshot.resolve("gemini-3.7-flash-high").unwrap();
    assert_eq!(resolved_ag.canonical_id.as_str(), "gemini-3.7-flash-high");
    assert_eq!(
        resolved_ag.eligible_bindings[0].provider_id.as_str(),
        "antigravity"
    );
    assert_eq!(
        resolved_ag.eligible_bindings[0].policy,
        ProviderPolicy::Closed
    );

    // Closed Claude model
    let resolved_claude = snapshot.resolve("claude-opus-4-6").unwrap();
    assert_eq!(resolved_claude.canonical_id.as_str(), "claude-opus-4-6");
    assert_eq!(
        resolved_claude.eligible_bindings[0].provider_id.as_str(),
        "claude"
    );

    // Kiro alias model
    let resolved_kiro = snapshot.resolve("auto-kiro").unwrap();
    assert_eq!(resolved_kiro.canonical_id.as_str(), "kiro/auto");
    assert_eq!(
        resolved_kiro.eligible_bindings[0].provider_id.as_str(),
        "kiro"
    );
    assert_eq!(
        resolved_kiro.eligible_bindings[0].effective_upstream_id(&resolved_kiro.canonical_id),
        "auto"
    );

    // Open Codex model
    let resolved_codex = snapshot.resolve("gpt-5.6-sol").unwrap();
    assert_eq!(resolved_codex.canonical_id.as_str(), "gpt-5.6-sol");
    assert!(resolved_codex
        .eligible_bindings
        .iter()
        .any(|b| b.provider_id.as_str() == "codex"));
}
