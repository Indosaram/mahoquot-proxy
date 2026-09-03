//! Tests for provider discovery authority and typed contributions (Task 7)
//!
//! Scope:
//! - Closed providers (Claude, Antigravity, Kiro, Cursor, Vertex, Zcode) can only expose
//!   catalog-declared models. Dynamic discovery cannot add models here.
//! - Discovered providers discover models dynamically with explicit authority masks
//!   and cannot overwrite catalog capabilities or alias targets.
//! - Open provider (Codex) accepts arbitrary model identifiers dynamically, with optional
//!   negative-space exclusions.
//! - Typed ProviderContribution and DiscoveredModel types.
//! - RegistryBuilder rejects unauthorized contributions.

use mahoquot_registry::{
    AuthorityMask, CatalogSource, CatalogVersion, DiscoveredModel, ModelCapability,
    ModelDescriptor, ModelId, ProviderBinding, ProviderContribution, ProviderId, ProviderPolicy,
    RegistryBuilder, RegistryError,
};

#[test]
fn test_closed_provider_rejects_dynamic_discovery() {
    let mut builder = RegistryBuilder::new(CatalogVersion(1), CatalogSource::EmbeddedFallback);
    builder.register_provider(ProviderId::claude(), ProviderPolicy::Closed);

    // Initial catalog-declared model for claude
    let declared_model =
        ModelDescriptor::new(ModelId::new("claude-sonnet-4-6").unwrap(), "anthropic").with_binding(
            ProviderBinding::new(
                ProviderId::claude(),
                ProviderPolicy::Closed,
                CatalogSource::EmbeddedFallback,
            ),
        );
    builder.add_model(declared_model).unwrap();

    // Contribution containing a new dynamic/discovered model under Closed provider Claude
    let dynamic_model = DiscoveredModel::new(ModelId::new("claude-dynamic-fake").unwrap());
    let contribution =
        ProviderContribution::from_discovered_models(ProviderId::claude(), vec![dynamic_model]);

    let err = builder.apply_contribution(contribution);
    assert!(
        matches!(err, Err(RegistryError::UnauthorizedContribution { .. })),
        "Closed provider must reject dynamic discovery contributions, got: {err:?}"
    );
}

#[test]
fn test_discovered_provider_accepts_bounded_discovery() {
    let mut builder = RegistryBuilder::new(CatalogVersion(1), CatalogSource::EmbeddedFallback);
    let custom_pid = ProviderId::new("custom-gateway").unwrap();
    builder.register_provider(custom_pid.clone(), ProviderPolicy::Discovered);

    // Initial catalog model to ensure catalog is routable
    let initial_model = ModelDescriptor::new(ModelId::new("base-model").unwrap(), "test-owner")
        .with_binding(ProviderBinding::new(
            custom_pid.clone(),
            ProviderPolicy::Discovered,
            CatalogSource::EmbeddedFallback,
        ));
    builder.add_model(initial_model).unwrap();

    // Discovered contribution with bounded authority mask
    let disc_model = DiscoveredModel::new(ModelId::new("discovered-model-1").unwrap())
        .with_capabilities([ModelCapability::Chat, ModelCapability::Tools])
        .with_context_limit(128_000)
        .with_authority(AuthorityMask::MODELS_ONLY); // Cannot grant capabilities if authority mask is MODELS_ONLY!

    let contribution =
        ProviderContribution::from_discovered_models(custom_pid.clone(), vec![disc_model]);

    let res = builder.apply_contribution(contribution);
    assert!(
        res.is_ok(),
        "Discovered provider should accept valid discovered models: {res:?}"
    );

    let snapshot = builder.build().expect("build snapshot");
    let resolved = snapshot.resolve("discovered-model-1").expect("resolved");
    assert_eq!(resolved.canonical_id.as_str(), "discovered-model-1");
    assert_eq!(resolved.eligible_bindings.len(), 1);
    assert_eq!(resolved.eligible_bindings[0].provider_id, custom_pid);
    assert_eq!(
        resolved.eligible_bindings[0].policy,
        ProviderPolicy::Discovered
    );

    // Since authority mask was MODELS_ONLY, capabilities were not authoritative from discovery
    let binding = &resolved.eligible_bindings[0];
    assert!(
        binding.capabilities.is_empty(),
        "MODELS_ONLY mask must not grant capabilities"
    );
}

#[test]
fn test_discovered_provider_cannot_overwrite_catalog_capabilities_or_aliases() {
    let mut builder = RegistryBuilder::new(CatalogVersion(1), CatalogSource::EmbeddedFallback);
    let custom_pid = ProviderId::new("custom-gateway").unwrap();
    builder.register_provider(custom_pid.clone(), ProviderPolicy::Discovered);

    // Catalog-declared model with Image capability from RemoteSigned catalog
    let catalog_binding = ProviderBinding::new(
        custom_pid.clone(),
        ProviderPolicy::Discovered,
        CatalogSource::RemoteSigned,
    )
    .with_capabilities([ModelCapability::Chat, ModelCapability::Image])
    .with_authority(AuthorityMask::ALL)
    .with_priority(100);

    let catalog_model =
        ModelDescriptor::new(ModelId::new("catalog-model").unwrap(), "catalog-owner")
            .with_binding(catalog_binding);
    builder.add_model(catalog_model).unwrap();

    // Add an alias: "alias-model" -> "catalog-model"
    builder
        .add_alias(
            ModelId::new("alias-model").unwrap(),
            ModelId::new("catalog-model").unwrap(),
            Some(custom_pid.clone()),
        )
        .unwrap();

    // Discovered contribution attempts to supply an empty or different capability set
    // with lower precedence (CatalogSource::Discovered vs RemoteSigned or LocalOverride)
    let disc_model = DiscoveredModel::new(ModelId::new("catalog-model").unwrap())
        .with_capabilities([ModelCapability::Chat]) // Missing Image!
        .with_authority(AuthorityMask::CAPABILITIES_ONLY);

    let contribution =
        ProviderContribution::from_discovered_models(custom_pid.clone(), vec![disc_model]);

    builder
        .apply_contribution(contribution)
        .expect("contribution applied");

    let snapshot = builder.build().unwrap();
    let resolved = snapshot.resolve("catalog-model").unwrap();

    // Catalog Image capability must NOT have been erased/overwritten by lower-precedence discovery
    assert!(
        resolved
            .effective_capabilities
            .contains(&ModelCapability::Image),
        "Discovered model must not overwrite higher-precedence catalog capabilities"
    );

    // Alias target must still resolve to "catalog-model"
    let alias_resolved = snapshot.resolve("alias-model").unwrap();
    assert_eq!(alias_resolved.canonical_id.as_str(), "catalog-model");
}

#[test]
fn test_open_provider_allows_passthrough_with_exclusions() {
    let mut builder = RegistryBuilder::new(CatalogVersion(1), CatalogSource::EmbeddedFallback);
    builder.register_provider(ProviderId::codex(), ProviderPolicy::Open);

    // Open provider accepts arbitrary contributions
    let disc_model = DiscoveredModel::new(ModelId::new("arbitrary-codex-model").unwrap())
        .with_capabilities([ModelCapability::Chat]);
    let contribution =
        ProviderContribution::from_discovered_models(ProviderId::codex(), vec![disc_model]);

    assert!(builder.apply_contribution(contribution).is_ok());

    // Add negative-space exclusion for a specific model under Codex
    builder.add_exclusion(
        ModelId::new("excluded-codex-model").unwrap(),
        Some(ProviderId::codex()),
    );

    let snapshot = builder.build().unwrap();

    // Arbitrary model resolves via Codex
    let resolved = snapshot.resolve("arbitrary-codex-model").unwrap();
    assert_eq!(resolved.canonical_id.as_str(), "arbitrary-codex-model");
    assert_eq!(
        resolved.eligible_bindings[0].provider_id,
        ProviderId::codex()
    );

    // Unclaimed model also resolves dynamically via Open Codex
    let unclaimed = snapshot.resolve("some-unseen-model").unwrap();
    assert_eq!(unclaimed.canonical_id.as_str(), "some-unseen-model");
    assert_eq!(
        unclaimed.eligible_bindings[0].provider_id,
        ProviderId::codex()
    );

    // Excluded model is blocked
    let err = snapshot.resolve("excluded-codex-model");
    assert!(
        matches!(
            err,
            Err(RegistryError::UnknownModel(_)) | Err(RegistryError::ModelExcluded { .. })
        ),
        "Excluded model should not resolve: {err:?}"
    );
}

#[test]
fn test_unauthorized_contribution_for_unknown_provider() {
    let mut builder = RegistryBuilder::new(CatalogVersion(1), CatalogSource::EmbeddedFallback);
    let unknown_pid = ProviderId::new("unknown-provider").unwrap();

    let contribution = ProviderContribution::from_discovered_models(
        unknown_pid.clone(),
        vec![DiscoveredModel::new(ModelId::new("m1").unwrap())],
    );

    let err = builder.apply_contribution(contribution);
    assert!(
        matches!(err, Err(RegistryError::UnknownProvider(ref pid)) if *pid == unknown_pid),
        "Applying contribution for unregistered provider must fail with UnknownProvider: {err:?}"
    );
}
