//! Tests for provider typed registry contributions and capability profiles.
//! Parity with legacy lists and dynamic snapshot support.

use mahoquot_registry::{
    embedded_snapshot, CatalogSource, CatalogVersion, ModelCapability, ModelDescriptor, ModelId,
    ProviderBinding, ProviderId, ProviderPolicy, RegistryBuilder,
};

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

const LEGACY_CODEX_MODELS: [&str; 12] = [
    "gpt-4.1",
    "gpt-4o",
    "gpt-5.3-codex",
    "gpt-5.3-codex-spark",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.5",
    "gpt-5.6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-image-1.5",
    "gpt-image-2",
];

const LEGACY_CURSOR_MODELS: [&str; 6] = [
    "cursor-fast",
    "cursor-small",
    "cursor/auto",
    "cursor/auto-balance",
    "cursor/auto-cost",
    "cursor/auto-intelligence",
];

const LEGACY_KIRO_MODELS: [&str; 8] = [
    "kiro/auto",
    "kiro/claude-haiku-4-5-20251001",
    "kiro/claude-haiku-4-5-20251001-thinking",
    "kiro/claude-haiku-4.5",
    "kiro/claude-opus-4.6",
    "kiro/claude-sonnet-4-5-20250929",
    "kiro/claude-sonnet-4-5-20250929-thinking",
    "kiro/claude-sonnet-4.6",
];

const LEGACY_VERTEX_MODELS: [&str; 21] = [
    "gemini-1.5-flash",
    "gemini-1.5-flash-001",
    "gemini-1.5-flash-002",
    "gemini-1.5-flash-8b",
    "gemini-1.5-pro",
    "gemini-1.5-pro-001",
    "gemini-1.5-pro-002",
    "gemini-2.0-flash",
    "gemini-2.0-flash-001",
    "gemini-2.0-pro-exp-02-05",
    "gemini-2.5-flash",
    "gemini-2.5-pro",
    "gemini-3-flash",
    "gemini-3-flash-preview",
    "gemini-3-pro",
    "gemini-3.1-flash-lite",
    "gemini-3.1-pro-preview",
    "gemini-3.5-flash",
    "gemini-3.6-flash",
    "gemini-3.7-flash",
    "gemini-3.7-flash-thinking",
];

const LEGACY_ZCODE_MODELS: [&str; 5] =
    ["glm-4.6", "glm-5.1", "glm-5.2", "glm-5.3", "glm-5.3-flash"];

#[test]
fn test_antigravity_typed_contribution_and_capabilities() {
    let contrib = mahoquot_providers::antigravity::default_contribution();
    assert_eq!(contrib.provider_id, ProviderId::antigravity());
    assert_eq!(contrib.policy(), ProviderPolicy::Closed);

    for id in LEGACY_ANTIGRAVITY_MODELS {
        assert!(
            contrib.supports_model(id),
            "Antigravity contribution must support legacy model {id}"
        );
        assert!(
            mahoquot_providers::antigravity::is_antigravity_model(id),
            "is_antigravity_model must return true for legacy model {id}"
        );
    }
    assert_eq!(contrib.models.len(), 14);

    // Capability profile verification
    let image_caps = contrib
        .capability_profile("gemini-3.1-flash-image")
        .expect("capability profile for gemini-3.1-flash-image");
    assert!(image_caps.contains(&ModelCapability::Image));
    assert!(image_caps.contains(&ModelCapability::Chat));

    let thinking_caps = contrib
        .capability_profile("claude-opus-4-6-thinking")
        .expect("capability profile for claude-opus-4-6-thinking");
    assert!(thinking_caps.contains(&ModelCapability::Chat));
    assert!(thinking_caps.contains(&ModelCapability::Tools));

    // Aliases
    assert!(contrib.supports_model("gemini-3.1-pro-high"));
    assert!(contrib.supports_model("gemini-3.1-pro-preview"));

    // Exclusions
    assert!(!contrib.supports_model("gpt-5.6-sol"));
    assert!(!contrib.supports_model("glm-5.2"));
}

#[test]
fn test_claude_typed_contribution_and_capabilities() {
    let contrib = mahoquot_providers::claude::default_contribution();
    assert_eq!(contrib.provider_id, ProviderId::claude());
    assert_eq!(contrib.policy(), ProviderPolicy::Closed);

    for id in LEGACY_CLAUDE_MODELS {
        assert!(
            contrib.supports_model(id),
            "Claude contribution must support legacy model {id}"
        );
        assert!(
            mahoquot_providers::claude::is_claude_model(id),
            "is_claude_model must return true for legacy model {id}"
        );
    }
    assert_eq!(contrib.models.len(), 12);

    let thinking_caps = contrib
        .capability_profile("claude-opus-4-5-20251101-thinking")
        .expect("thinking model capabilities");
    assert!(thinking_caps.contains(&ModelCapability::Chat));
    assert!(thinking_caps.contains(&ModelCapability::Tools));

    assert!(!contrib.supports_model("gpt-5.6-sol"));
    assert!(!contrib.supports_model("glm-5.2"));
}

#[test]
fn test_codex_typed_contribution_and_capabilities() {
    let contrib = mahoquot_providers::codex::default_contribution();
    assert_eq!(contrib.provider_id, ProviderId::codex());
    assert_eq!(contrib.policy(), ProviderPolicy::Open);

    for id in LEGACY_CODEX_MODELS {
        assert!(
            contrib.supports_model(id),
            "Codex contribution must support legacy model {id}"
        );
        assert!(
            mahoquot_providers::codex::is_codex_model(id),
            "is_codex_model must return true for legacy model {id}"
        );
    }
    assert_eq!(contrib.models.len(), 12);

    let img_caps = contrib
        .capability_profile("gpt-image-1.5")
        .expect("image model capabilities");
    assert!(img_caps.contains(&ModelCapability::Image));
}

#[test]
fn test_cursor_typed_contribution_and_capabilities() {
    let contrib = mahoquot_providers::cursor::default_contribution();
    assert_eq!(contrib.provider_id, ProviderId::cursor());
    assert_eq!(contrib.policy(), ProviderPolicy::Closed);

    for id in LEGACY_CURSOR_MODELS {
        assert!(
            contrib.supports_model(id),
            "Cursor contribution must support legacy model {id}"
        );
        assert!(
            mahoquot_providers::cursor::is_cursor_model(id),
            "is_cursor_model must return true for legacy model {id}"
        );
    }
    assert_eq!(contrib.models.len(), 6);

    let auto_caps = contrib
        .capability_profile("cursor/auto")
        .expect("cursor/auto capabilities");
    assert!(auto_caps.contains(&ModelCapability::Chat));
    assert!(auto_caps.contains(&ModelCapability::Tools));

    assert!(!contrib.supports_model("glm-5.2"));
}

#[test]
fn test_kiro_typed_contribution_and_capabilities() {
    let contrib = mahoquot_providers::kiro::default_contribution();
    assert_eq!(contrib.provider_id, ProviderId::kiro());
    assert_eq!(contrib.policy(), ProviderPolicy::Closed);

    for id in LEGACY_KIRO_MODELS {
        assert!(
            contrib.supports_model(id),
            "Kiro contribution must support prefixed model {id}"
        );
    }
    assert_eq!(contrib.models.len(), 8);

    // Kiro unprefixed upstream IDs must also be supported
    assert!(mahoquot_providers::kiro::is_kiro_model("claude-sonnet-4.6"));
    assert!(mahoquot_providers::kiro::is_kiro_model("claude-opus-4.6"));
    assert!(mahoquot_providers::kiro::is_kiro_model("auto"));
    assert!(contrib.supports_model("claude-sonnet-4.6"));

    // Aliases
    assert!(contrib.supports_model("auto-kiro"));

    // Capabilities
    let thinking_caps = contrib
        .capability_profile("kiro/claude-sonnet-4-5-20250929-thinking")
        .expect("kiro thinking capabilities");
    assert!(thinking_caps.contains(&ModelCapability::Chat));
    assert!(thinking_caps.contains(&ModelCapability::Tools));
}

#[test]
fn test_vertex_typed_contribution_and_capabilities() {
    let contrib = mahoquot_providers::vertex::default_contribution();
    assert_eq!(contrib.provider_id, ProviderId::vertex());
    assert_eq!(contrib.policy(), ProviderPolicy::Closed);

    for id in LEGACY_VERTEX_MODELS {
        assert!(
            contrib.supports_model(id),
            "Vertex contribution must support legacy model {id}"
        );
        assert!(
            mahoquot_providers::vertex::is_vertex_model(id),
            "is_vertex_model must return true for legacy model {id}"
        );
    }
    assert_eq!(contrib.models.len(), 21);

    // Vertex prefix support (catalog rule: authority.prefixes = true)
    assert!(contrib.has_prefix_support());
    assert!(contrib.supports_model("gemini-9.9-ultra-future"));
    assert!(contrib.supports_model("google/custom-deployed-model"));
    assert!(mahoquot_providers::vertex::is_vertex_model(
        "gemini-9.9-ultra-future"
    ));
    assert!(mahoquot_providers::vertex::is_vertex_model(
        "google/custom-deployed-model"
    ));

    // Exclusions
    assert!(!contrib.supports_model("claude-sonnet-4-6"));
    assert!(!contrib.supports_model("gpt-5.6-sol"));
}

#[test]
fn test_zcode_typed_contribution_and_capabilities() {
    let contrib = mahoquot_providers::zcode::default_contribution();
    assert_eq!(contrib.provider_id, ProviderId::zcode());
    assert_eq!(contrib.policy(), ProviderPolicy::Closed);

    for id in LEGACY_ZCODE_MODELS {
        assert!(
            contrib.supports_model(id),
            "Zcode contribution must support legacy model {id}"
        );
        assert!(
            mahoquot_providers::zcode::is_zcode_model(id),
            "is_zcode_model must return true for legacy model {id}"
        );
    }
    assert_eq!(contrib.models.len(), 5);

    let glm_caps = contrib
        .capability_profile("glm-5.3-flash")
        .expect("glm-5.3-flash capabilities");
    assert!(glm_caps.contains(&ModelCapability::Chat));
    assert!(glm_caps.contains(&ModelCapability::Tools));

    assert!(!contrib.supports_model("claude-sonnet-4-6"));
    assert!(!contrib.supports_model("gpt-5.6-sol"));
}

#[test]
fn test_dynamic_snapshot_fixture_adds_fictitious_model_without_rust_constants_change() {
    // QA Scenario: run a fixture catalog containing `gemini-next-flash-high`,
    // and prove the contribution contains the model with Antigravity binding
    // WITHOUT changing any Rust constants.
    let base_snapshot = embedded_snapshot();
    let mut builder = RegistryBuilder::new(
        CatalogVersion::new(base_snapshot.version().as_u64() + 1),
        CatalogSource::LocalOverride,
    );

    // Copy existing providers and models
    for (pid, policy) in base_snapshot.providers() {
        builder.register_provider(pid.clone(), *policy);
    }
    for model in base_snapshot.models().values() {
        builder.add_model(model.clone()).unwrap();
    }
    for alias in base_snapshot.aliases().values() {
        builder.add_alias_rule(alias.clone()).unwrap();
    }

    // Add fictitious model with Antigravity binding
    let fictitious_id = ModelId::new("gemini-next-flash-high").unwrap();
    let fictitious_model = ModelDescriptor::new(fictitious_id.clone(), "google")
        .with_display_name("Gemini Next Flash High")
        .with_capabilities([ModelCapability::Chat, ModelCapability::Tools])
        .with_binding(
            ProviderBinding::new(
                ProviderId::antigravity(),
                ProviderPolicy::Closed,
                CatalogSource::LocalOverride,
            )
            .with_capabilities([ModelCapability::Chat, ModelCapability::Tools])
            .with_priority(100),
        );
    builder.add_model(fictitious_model).unwrap();

    let custom_snapshot = builder.build().unwrap();

    // Verify Antigravity extracts this new model from the custom snapshot
    let antigravity_contrib = mahoquot_providers::antigravity::contribution(&custom_snapshot);
    assert!(
        antigravity_contrib.supports_model("gemini-next-flash-high"),
        "Antigravity contribution from dynamic snapshot must contain fictitious model"
    );
    assert_eq!(antigravity_contrib.models.len(), 15);
    assert!(
        mahoquot_providers::antigravity::is_antigravity_model_in_snapshot(
            &custom_snapshot,
            "gemini-next-flash-high"
        )
    );

    // Verify capability profile on the newly added fictitious model
    let caps = antigravity_contrib
        .capability_profile("gemini-next-flash-high")
        .expect("capability profile for gemini-next-flash-high");
    assert!(caps.contains(&ModelCapability::Chat));
    assert!(caps.contains(&ModelCapability::Tools));

    // Embedded snapshot unchanged
    assert!(!mahoquot_providers::antigravity::is_antigravity_model(
        "gemini-next-flash-high"
    ));
}
