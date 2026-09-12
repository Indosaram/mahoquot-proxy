//! Regression tests for Devin provider identity and Discovered-only registry policy.
//!
//! Contracts under test (plan sections 5 and 6.4):
//! - `ProviderId::devin()` yields the canonical provider id "devin".
//! - No codeium/windsurf aliasing: those ids remain themselves and are never mapped to devin.
//! - The embedded catalog registers "devin" with `ProviderPolicy::Discovered` and ships zero
//!   static routable Devin models.
//! - Unknown `devin/<uid>` requests are typed UnknownModel errors and never fall through to an
//!   Open provider's fallback (Codex must not steal the devin namespace).
//! - A Discovered contribution with public model id `devin/<exact model_uid>` becomes routable;
//!   suffix variants (`-max`, `-none`, `-medium`, `-1m`) are preserved verbatim as distinct ids.

use std::collections::BTreeSet;

use mahoquot_registry::{
    CatalogSource, CatalogVersion, ContributionItem, DiscoveredModel, ModelCapability,
    ModelDescriptor, ModelId, ProviderBinding, ProviderContribution, ProviderId, ProviderPolicy,
    RegistryBuilder,
};

#[test]
fn provider_id_devin_identity() {
    assert_eq!(ProviderId::devin().as_str(), "devin");
    // canonical() normalizes case/whitespace but must not alias to another provider.
    let canon = ProviderId::canonical(" Devin ").unwrap();
    assert_eq!(canon, ProviderId::devin());
    assert_eq!(canon.as_str(), "devin");
    // No codeium/windsurf aliasing to devin.
    assert_eq!(
        ProviderId::canonical("codeium").unwrap().as_str(),
        "codeium"
    );
    assert_eq!(
        ProviderId::canonical("windsurf").unwrap().as_str(),
        "windsurf"
    );
    assert_ne!(
        ProviderId::canonical("codeium").unwrap(),
        ProviderId::devin()
    );
    assert_ne!(
        ProviderId::canonical("windsurf").unwrap(),
        ProviderId::devin()
    );
}

#[test]
fn embedded_catalog_registers_devin_discovered_without_static_models() {
    let snapshot = mahoquot_registry::embedded_snapshot();
    let providers = snapshot.providers();
    assert_eq!(
        providers.get(&ProviderId::devin()),
        Some(&ProviderPolicy::Discovered),
        "embedded catalog must register devin with the Discovered policy"
    );

    // Zero static routable Devin models: no model is bound to devin, none is owned by devin,
    // and no alias points into a devin model.
    for desc in snapshot.models().values() {
        assert!(
            desc.binding_for(&ProviderId::devin()).is_none(),
            "embedded catalog must not ship a static Devin binding for model '{}'",
            desc.id
        );
        assert_ne!(
            desc.owned_by, "devin",
            "embedded catalog must not ship a static Devin-owned model '{}'",
            desc.id
        );
        for alias in &desc.aliases {
            assert!(
                !alias.as_str().starts_with("devin/"),
                "embedded catalog must not alias into the devin namespace ('{alias}')"
            );
        }
    }
    for rule in snapshot.aliases().values() {
        assert!(
            !rule.target.as_str().starts_with("devin/"),
            "embedded catalog alias '{}' must not target the devin namespace",
            rule.alias
        );
    }
    assert!(
        snapshot.validate().is_ok(),
        "embedded catalog with the devin provider entry must remain valid"
    );
}

#[test]
fn unknown_devin_model_is_not_served_by_open_fallback() {
    let snapshot = mahoquot_registry::embedded_snapshot();
    // Codex is Open in the embedded catalog; it must not steal unknown devin/* ids.
    assert_eq!(
        snapshot.providers().get(&ProviderId::codex()),
        Some(&ProviderPolicy::Open)
    );
    let err = snapshot.resolve("devin/never-discovered-uid").unwrap_err();
    assert!(
        matches!(err, mahoquot_registry::RegistryError::UnknownModel(ref m) if m.as_str() == "devin/never-discovered-uid"),
        "unknown devin/* must be a typed UnknownModel error, got: {err:?}"
    );
}

#[test]
fn discovered_contribution_makes_devin_uid_routable_with_suffix_variants() {
    let mut builder = RegistryBuilder::new(CatalogVersion::new(1), CatalogSource::Discovered);
    builder.register_provider(ProviderId::devin(), ProviderPolicy::Discovered);

    let uid_max = ModelId::new("devin/glm-5-2-max").unwrap();
    let disc = DiscoveredModel::new(uid_max.clone())
        .with_capabilities([ModelCapability::Chat, ModelCapability::Tools])
        .with_context_limit(200_000)
        .with_authority(mahoquot_registry::AuthorityMask::MODELS_ONLY);
    builder
        .apply_contribution(
            ProviderContribution::from_discovered_models(ProviderId::devin(), vec![disc])
                .with_policy(ProviderPolicy::Discovered),
        )
        .unwrap();
    let snapshot = builder.build().unwrap();
    snapshot.validate().unwrap();

    let resolved = snapshot.resolve("devin/glm-5-2-max").unwrap();
    assert_eq!(resolved.canonical_id, uid_max);
    // The base uid without the suffix is a different model and stays unroutable.
    let base_err = snapshot.resolve("devin/glm-5-2").unwrap_err();
    assert!(
        matches!(base_err, mahoquot_registry::RegistryError::UnknownModel(_)),
        "base uid must not be inferred from a suffix variant, got: {base_err:?}"
    );
}

#[test]
fn devin_contribution_rejected_when_provider_policy_is_closed() {
    let mut builder = RegistryBuilder::new(CatalogVersion::new(1), CatalogSource::Discovered);
    builder.register_provider(ProviderId::devin(), ProviderPolicy::Closed);
    let err = builder
        .apply_contribution(
            ProviderContribution::from_discovered_models(
                ProviderId::devin(),
                vec![DiscoveredModel::new(ModelId::new("devin/glm-5-2").unwrap())],
            )
            .with_policy(ProviderPolicy::Closed),
        )
        .unwrap_err();
    assert!(
        matches!(
            err,
            mahoquot_registry::RegistryError::UnauthorizedContribution { ref provider_id, policy: ProviderPolicy::Closed, .. }
                if *provider_id == ProviderId::devin()
        ),
        "closed devin must reject dynamic discovery, got: {err:?}"
    );
}

#[test]
fn devin_discovered_contribution_is_mergable_after_catalog_registration() {
    // Simulate the signed-catalog-first flow: embedded catalog (devin registered, no models),
    // then a live discovery contribution must merge into the same generation.
    let mut builder = RegistryBuilder::new(
        mahoquot_registry::embedded_snapshot().version,
        CatalogSource::RemoteSigned,
    );
    builder.register_provider(ProviderId::devin(), ProviderPolicy::Discovered);
    builder
        .apply_contribution(
            ProviderContribution::from_discovered_models(
                ProviderId::devin(),
                vec![
                    DiscoveredModel::new(ModelId::new("devin/glm-5-2").unwrap()),
                    DiscoveredModel::new(ModelId::new("devin/glm-5-2-1m").unwrap()),
                ],
            )
            .with_policy(ProviderPolicy::Discovered),
        )
        .unwrap();
    let snapshot = builder.build().unwrap();
    snapshot.validate().unwrap();
    assert!(snapshot.resolve("devin/glm-5-2").is_ok());
    assert!(snapshot.resolve("devin/glm-5-2-1m").is_ok());
    // Distinct suffix variants stay distinct canonical models.
    assert_ne!(
        snapshot.resolve("devin/glm-5-2").unwrap().canonical_id,
        snapshot.resolve("devin/glm-5-2-1m").unwrap().canonical_id
    );
}

#[test]
fn devin_contribution_items_roundtrip_ids_verbatim() {
    let ids = ["devin/swe-1-7", "devin/glm-5-2-max", "devin/glm-5-2-none"];
    let models = ids
        .iter()
        .map(|id| DiscoveredModel::new(ModelId::new(id).unwrap()))
        .collect();
    let contribution = ProviderContribution::from_discovered_models(ProviderId::devin(), models)
        .with_policy(ProviderPolicy::Discovered);
    assert_eq!(
        contribution.policy(),
        ProviderPolicy::Discovered,
        "contribution without explicit policy defaults to Closed; explicit Discovered must stick"
    );
    assert_eq!(contribution.provider_id, ProviderId::devin());
    let got = contribution.model_ids();
    let expected: BTreeSet<String> = ids.iter().map(|s| s.to_string()).collect();
    let got_set: BTreeSet<String> = got.iter().map(|m| m.as_str().to_string()).collect();
    assert_eq!(
        got_set, expected,
        "public model ids must be devin/<exact uid> verbatim"
    );
    // Sanity: Discovered items, not descriptors.
    for item in &contribution.models {
        assert!(matches!(item, ContributionItem::Discovered(_)));
    }
}

#[test]
fn devin_vision_support_does_not_grant_image_generation_capability() {
    let mut builder = RegistryBuilder::new(CatalogVersion::new(1), CatalogSource::Discovered);
    builder.register_provider(ProviderId::devin(), ProviderPolicy::Discovered);

    // Devin models support vision input, but must NEVER have ModelCapability::Image set,
    // as Image capability exposes the image generation surface.
    let uid = ModelId::new("devin/glm-5-2").unwrap();
    let disc = DiscoveredModel::new(uid.clone())
        .with_capabilities([ModelCapability::Chat, ModelCapability::Tools])
        .with_authority(mahoquot_registry::AuthorityMask::MODELS_ONLY.with_capabilities(true));
    builder
        .apply_contribution(
            ProviderContribution::from_discovered_models(ProviderId::devin(), vec![disc])
                .with_policy(ProviderPolicy::Discovered),
        )
        .unwrap();
    let snapshot = builder.build().unwrap();
    let resolved = snapshot.resolve("devin/glm-5-2").unwrap();

    assert!(resolved
        .effective_capabilities
        .contains(&ModelCapability::Chat));
    assert!(resolved
        .effective_capabilities
        .contains(&ModelCapability::Tools));
    assert!(
        !resolved
            .effective_capabilities
            .contains(&ModelCapability::Image),
        "vision inputs must not set ModelCapability::Image (image generation surface)"
    );
}

#[test]
fn devin_signed_catalog_compatibility_and_no_upstream_signature_claim() {
    use ed25519_dalek::SigningKey;
    use mahoquot_registry::envelope::*;

    // 1. Embedded fallback catalog explicitly preserves CatalogSource::EmbeddedFallback
    // and does NOT claim upstream signature.
    let embedded = mahoquot_registry::embedded_snapshot();
    assert_eq!(
        embedded.source,
        CatalogSource::EmbeddedFallback,
        "locally edited catalog must remain EmbeddedFallback, not claim RemoteSigned"
    );

    // 2. Signed-catalog compatibility: a RemoteSigned catalog containing Devin's
    // Discovered provider policy signs and verifies cleanly.
    let mut rng = rand::rngs::OsRng;
    let signing_key = SigningKey::generate(&mut rng);
    let key_id = "test-key-2026-devin";
    let keyring = Keyring::new().with_key(key_id, signing_key.verifying_key());
    let signer = CatalogSigner::new(signing_key, key_id);

    let mut builder = RegistryBuilder::new(CatalogVersion::new(10), CatalogSource::RemoteSigned);
    builder.register_provider(ProviderId::claude(), ProviderPolicy::Closed);
    builder.register_provider(ProviderId::devin(), ProviderPolicy::Discovered);

    // Must have at least one fallback-routable model for signed catalog validity
    let mut model = ModelDescriptor::new(ModelId::new("claude-sonnet-4-6").unwrap(), "anthropic");
    model.capabilities.insert(ModelCapability::Chat);
    let binding = ProviderBinding::new(
        ProviderId::claude(),
        ProviderPolicy::Closed,
        CatalogSource::RemoteSigned,
    )
    .with_capabilities([ModelCapability::Chat]);
    builder.add_model(model).unwrap();
    builder
        .add_binding(ModelId::new("claude-sonnet-4-6").unwrap(), binding)
        .unwrap();

    let snapshot = builder.build().unwrap();
    let payload = canonicalize_json(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
    let now = 1000;

    let envelope = signer
        .sign_catalog(snapshot.version(), now, None, &payload)
        .expect("signing with devin provider entry should succeed");

    let verified = verify_catalog_envelope(
        &envelope,
        &payload,
        &keyring,
        Some(CatalogVersion(5)),
        Some(CatalogVersion(8)),
        now,
        DEFAULT_CLOCK_SKEW_SECS,
    )
    .expect("verification with devin provider entry should succeed");

    assert_eq!(
        verified.providers().get(&ProviderId::devin()),
        Some(&ProviderPolicy::Discovered)
    );
}
