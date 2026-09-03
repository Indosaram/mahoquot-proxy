use mahoquot_registry::*;

#[test]
fn test_invalid_and_empty_ids() {
    // Empty provider ID
    assert_eq!(ProviderId::new(""), Err(RegistryError::EmptyProviderId));
    assert_eq!(ProviderId::new("   "), Err(RegistryError::EmptyProviderId));

    // Invalid provider ID with spaces or control characters
    assert!(matches!(
        ProviderId::new("provider with space"),
        Err(RegistryError::InvalidProviderId { .. })
    ));
    assert!(matches!(
        ProviderId::new("provider\nnewline"),
        Err(RegistryError::InvalidProviderId { .. })
    ));

    // Valid provider ID
    let pid = ProviderId::new("antigravity").unwrap();
    assert_eq!(pid.as_str(), "antigravity");

    // Empty model ID
    assert_eq!(ModelId::new(""), Err(RegistryError::EmptyModelId));
    assert_eq!(ModelId::new("   \t"), Err(RegistryError::EmptyModelId));

    // Invalid model ID with spaces or control characters
    assert!(matches!(
        ModelId::new("model with spaces"),
        Err(RegistryError::InvalidModelId { .. })
    ));
    assert!(matches!(
        ModelId::new("model\x00null"),
        Err(RegistryError::InvalidModelId { .. })
    ));

    // Valid model ID
    let mid = ModelId::new("claude-sonnet-4-6").unwrap();
    assert_eq!(mid.as_str(), "claude-sonnet-4-6");
}

#[test]
fn test_provider_id_canonical() {
    assert_eq!(ProviderId::canonical("openai").unwrap().as_str(), "codex");
    assert_eq!(ProviderId::canonical("OpenAI").unwrap().as_str(), "codex");
    assert_eq!(ProviderId::canonical("anthropic").unwrap().as_str(), "claude");
    assert_eq!(ProviderId::canonical("Anthropic").unwrap().as_str(), "claude");
    assert_eq!(ProviderId::canonical("google-vertex").unwrap().as_str(), "vertex");
    assert_eq!(ProviderId::canonical("Google-Vertex").unwrap().as_str(), "vertex");
    assert_eq!(ProviderId::canonical("DeepSeek").unwrap().as_str(), "deepseek");
    assert_eq!(ProviderId::canonical("deepseek").unwrap().as_str(), "deepseek");
    assert_eq!(ProviderId::canonical("Mistral").unwrap().as_str(), "mistral");
    assert_eq!(ProviderId::canonical("  cohere  ").unwrap().as_str(), "cohere");
}

#[test]
fn test_duplicate_binding_merge() {
    let pid = ProviderId::new("generic-provider").unwrap();

    // Base binding from remote signed catalog with chat capability and upstream ID
    let mut base = ProviderBinding::new(
        pid.clone(),
        ProviderPolicy::Discovered,
        CatalogSource::RemoteSigned,
    )
    .with_capabilities([ModelCapability::Chat])
    .with_upstream_id("upstream-base")
    .with_priority(10)
    .with_authority(AuthorityMask::ALL);

    // Incoming binding from local override with higher precedence,
    // but its authority mask ONLY covers capabilities (not upstream_id).
    let override_binding = ProviderBinding::new(
        pid.clone(),
        ProviderPolicy::Discovered,
        CatalogSource::LocalOverride,
    )
    .with_capabilities([ModelCapability::Chat, ModelCapability::Tools])
    .with_upstream_id("upstream-should-be-ignored")
    .with_priority(20)
    .with_authority(AuthorityMask::CAPABILITIES_ONLY);

    base.merge_with(&override_binding);

    // High precedence capabilities merged because authority.capabilities = true
    assert!(base.capabilities.contains(&ModelCapability::Chat));
    assert!(base.capabilities.contains(&ModelCapability::Tools));
    // Upstream ID was NOT updated because authority.upstream_id = false
    assert_eq!(base.upstream_model_id.as_deref(), Some("upstream-base"));
    // Source should now be LocalOverride
    assert_eq!(base.source, CatalogSource::LocalOverride);
}

#[test]
fn test_stable_ordering() {
    let mut builder = RegistryBuilder::new(CatalogVersion(1), CatalogSource::EmbeddedFallback);
    builder.register_provider(ProviderId::claude(), ProviderPolicy::Closed);

    // Insert models out of alphabetical order
    let m_z = ModelDescriptor::new(ModelId::new("z-model").unwrap(), "anthropic");
    let m_a = ModelDescriptor::new(ModelId::new("a-model").unwrap(), "anthropic");
    let m_m = ModelDescriptor::new(ModelId::new("m-model").unwrap(), "anthropic");

    builder.add_model(m_z).unwrap();
    builder.add_model(m_a).unwrap();
    builder.add_model(m_m).unwrap();

    let snapshot = builder.build().unwrap();
    let model_keys: Vec<&str> = snapshot.models().keys().map(|k| k.as_str()).collect();

    // BTreeMap guarantees sorted alphabetical ordering
    assert_eq!(model_keys, vec!["a-model", "m-model", "z-model"]);
}

#[test]
fn test_alias_cycles_and_depth() {
    let mut builder = RegistryBuilder::new(CatalogVersion(1), CatalogSource::EmbeddedFallback);
    builder.register_provider(ProviderId::claude(), ProviderPolicy::Closed);
    builder
        .add_model(ModelDescriptor::new(
            ModelId::new("canonical").unwrap(),
            "anthropic",
        ))
        .unwrap();

    // Self cycle: a -> a
    let mut b1 = RegistryBuilder::new(CatalogVersion(1), CatalogSource::EmbeddedFallback);
    b1.register_provider(ProviderId::claude(), ProviderPolicy::Closed);
    b1.add_model(ModelDescriptor::new(
        ModelId::new("canonical").unwrap(),
        "anthropic",
    ))
    .unwrap();
    b1.add_alias(
        ModelId::new("loop-a").unwrap(),
        ModelId::new("loop-a").unwrap(),
        None,
    )
    .unwrap();
    assert!(matches!(b1.build(), Err(RegistryError::AliasCycle { .. })));

    // Indirect cycle: a -> b -> a
    let mut b2 = RegistryBuilder::new(CatalogVersion(1), CatalogSource::EmbeddedFallback);
    b2.register_provider(ProviderId::claude(), ProviderPolicy::Closed);
    b2.add_model(ModelDescriptor::new(
        ModelId::new("canonical").unwrap(),
        "anthropic",
    ))
    .unwrap();
    b2.add_alias(ModelId::new("a").unwrap(), ModelId::new("b").unwrap(), None)
        .unwrap();
    b2.add_alias(ModelId::new("b").unwrap(), ModelId::new("a").unwrap(), None)
        .unwrap();
    assert!(matches!(b2.build(), Err(RegistryError::AliasCycle { .. })));

    // Chain depth exceeded: > 10
    let mut b3 = RegistryBuilder::new(CatalogVersion(1), CatalogSource::EmbeddedFallback);
    b3.register_provider(ProviderId::claude(), ProviderPolicy::Closed);
    b3.add_model(ModelDescriptor::new(
        ModelId::new("target").unwrap(),
        "anthropic",
    ))
    .unwrap();
    for i in 0..12 {
        let from = ModelId::new(format!("chain-{}", i)).unwrap();
        let to = if i == 11 {
            ModelId::new("target").unwrap()
        } else {
            ModelId::new(format!("chain-{}", i + 1)).unwrap()
        };
        b3.add_alias(from, to, None).unwrap();
    }
    assert!(matches!(
        b3.build(),
        Err(RegistryError::AliasDepthExceeded { .. })
    ));
}

#[test]
fn test_unknown_targets() {
    let mut builder = RegistryBuilder::new(CatalogVersion(1), CatalogSource::EmbeddedFallback);
    builder.register_provider(ProviderId::claude(), ProviderPolicy::Closed);
    // Alias to nonexistent model without Open provider
    builder
        .add_alias(
            ModelId::new("alias-model").unwrap(),
            ModelId::new("nonexistent-model").unwrap(),
            None,
        )
        .unwrap();

    let res = builder.build();
    assert!(matches!(res, Err(RegistryError::UnknownAliasTarget { .. })));
}

#[test]
fn test_capability_lookup() {
    let mut builder = RegistryBuilder::new(CatalogVersion(1), CatalogSource::EmbeddedFallback);
    let claude_id = ProviderId::claude();
    builder.register_provider(claude_id.clone(), ProviderPolicy::Closed);

    let mut model = ModelDescriptor::new(ModelId::new("claude-vision").unwrap(), "anthropic");
    model.capabilities.insert(ModelCapability::Chat);

    let binding = ProviderBinding::new(
        claude_id,
        ProviderPolicy::Closed,
        CatalogSource::EmbeddedFallback,
    )
    .with_capabilities([ModelCapability::Image, ModelCapability::Tools]);
    model.bindings.insert(ProviderId::claude(), binding);

    builder.add_model(model).unwrap();
    let snapshot = builder.build().unwrap();

    let caps = snapshot.capabilities_for("claude-vision").unwrap();
    assert!(caps.contains(&ModelCapability::Chat));
    assert!(caps.contains(&ModelCapability::Image));
    assert!(caps.contains(&ModelCapability::Tools));
    assert!(!caps.contains(&ModelCapability::Video));
}

#[test]
fn test_deterministic_serialization() {
    let mut builder = RegistryBuilder::new(CatalogVersion(42), CatalogSource::RemoteSigned);
    builder.register_provider(ProviderId::antigravity(), ProviderPolicy::Closed);
    builder.register_provider(ProviderId::codex(), ProviderPolicy::Open);

    let mut m1 = ModelDescriptor::new(ModelId::new("gemini-2.5-flash").unwrap(), "google");
    m1.capabilities.insert(ModelCapability::Chat);
    m1.capabilities.insert(ModelCapability::Image);

    let binding = ProviderBinding::new(
        ProviderId::antigravity(),
        ProviderPolicy::Closed,
        CatalogSource::RemoteSigned,
    )
    .with_capabilities([ModelCapability::Chat, ModelCapability::Image]);
    m1.bindings.insert(ProviderId::antigravity(), binding);

    builder.add_model(m1).unwrap();
    let snapshot = builder.build().unwrap();

    let json1 = snapshot.to_json_canonical().unwrap();
    let roundtrip: RegistrySnapshot = RegistrySnapshot::from_json(&json1).unwrap();
    let json2 = roundtrip.to_json_canonical().unwrap();

    assert_eq!(
        json1, json2,
        "serialization must be strictly deterministic and byte-stable"
    );
}

#[test]
fn resolves_closed_discovered_and_open_without_ambiguity() {
    let mut builder = RegistryBuilder::new(CatalogVersion(1), CatalogSource::EmbeddedFallback);

    let p_closed = ProviderId::claude();
    let p_discovered = ProviderId::new("custom-gateway").unwrap();
    let p_open = ProviderId::codex();

    builder.register_provider(p_closed.clone(), ProviderPolicy::Closed);
    builder.register_provider(p_discovered.clone(), ProviderPolicy::Discovered);
    builder.register_provider(p_open.clone(), ProviderPolicy::Open);

    // 1. Closed provider has explicit binding for "claude-sonnet-4-6"
    let mut m_closed =
        ModelDescriptor::new(ModelId::new("claude-sonnet-4-6").unwrap(), "anthropic");
    m_closed.bindings.insert(
        p_closed.clone(),
        ProviderBinding::new(
            p_closed.clone(),
            ProviderPolicy::Closed,
            CatalogSource::EmbeddedFallback,
        )
        .with_priority(100),
    );
    builder.add_model(m_closed).unwrap();

    // 2. Discovered provider has binding for "deepseek-chat"
    let mut m_disc = ModelDescriptor::new(ModelId::new("deepseek-chat").unwrap(), "deepseek");
    m_disc.bindings.insert(
        p_discovered.clone(),
        ProviderBinding::new(
            p_discovered.clone(),
            ProviderPolicy::Discovered,
            CatalogSource::Discovered,
        )
        .with_priority(50),
    );
    builder.add_model(m_disc).unwrap();

    let snapshot = builder.build().unwrap();

    // --- Assertion 1: Closed binding wins ---
    // When resolving "claude-sonnet-4-6", the closed binding wins and is eligible.
    let resolved_claude = snapshot
        .resolve("claude-sonnet-4-6")
        .expect("claude model must resolve");
    assert_eq!(resolved_claude.canonical_id.as_str(), "claude-sonnet-4-6");
    assert_eq!(resolved_claude.eligible_bindings.len(), 1);
    assert_eq!(resolved_claude.eligible_bindings[0].provider_id, p_closed);
    assert_eq!(
        resolved_claude.eligible_bindings[0].policy,
        ProviderPolicy::Closed
    );
    println!(
        "[PASS] Closed binding wins: claude-sonnet-4-6 resolved to ProviderId({}) with policy {:?}",
        resolved_claude.eligible_bindings[0].provider_id,
        resolved_claude.eligible_bindings[0].policy
    );

    // --- Assertion 2: Open Codex cannot steal a claimed ID ---
    // Codex (Open) is in the registry, but "claude-sonnet-4-6" is claimed by closed claude,
    // and "deepseek-chat" is claimed by discovered custom-gateway.
    // Codex CANNOT appear in the eligible bindings for either!
    assert!(resolved_claude
        .eligible_bindings
        .iter()
        .all(|b| b.provider_id != p_open));

    let resolved_deepseek = snapshot
        .resolve("deepseek-chat")
        .expect("deepseek model must resolve");
    assert_eq!(resolved_deepseek.eligible_bindings.len(), 1);
    assert_eq!(
        resolved_deepseek.eligible_bindings[0].provider_id,
        p_discovered
    );
    assert!(resolved_deepseek
        .eligible_bindings
        .iter()
        .all(|b| b.provider_id != p_open));
    println!("[PASS] Open Codex cannot steal claimed IDs: codex absent from bindings for claude-sonnet-4-6 and deepseek-chat");

    // --- Assertion 3: Discovered fallback is bounded ---
    // Discovered provider can ONLY serve its discovered/bound models. It cannot serve arbitrary models.
    // An unclaimed model "custom-unclaimed-model" is served by Open Codex, NOT Discovered custom-gateway.
    let resolved_unclaimed = snapshot
        .resolve("custom-unclaimed-model")
        .expect("open codex serves unclaimed");
    assert_eq!(resolved_unclaimed.eligible_bindings.len(), 1);
    assert_eq!(resolved_unclaimed.eligible_bindings[0].provider_id, p_open);
    assert!(resolved_unclaimed
        .eligible_bindings
        .iter()
        .all(|b| b.provider_id != p_discovered));
    assert!(resolved_unclaimed
        .eligible_bindings
        .iter()
        .all(|b| b.provider_id != p_closed));
    println!("[PASS] Discovered fallback is bounded: custom-unclaimed-model served by Open Codex, not Discovered custom-gateway");

    // If we build a registry WITHOUT the Open provider, unknown model fails cleanly:
    let mut builder_no_open =
        RegistryBuilder::new(CatalogVersion(1), CatalogSource::EmbeddedFallback);
    builder_no_open.register_provider(p_closed, ProviderPolicy::Closed);
    builder_no_open.register_provider(p_discovered, ProviderPolicy::Discovered);
    let mut m_disc2 = ModelDescriptor::new(ModelId::new("deepseek-chat").unwrap(), "deepseek");
    m_disc2.bindings.insert(
        ProviderId::new("custom-gateway").unwrap(),
        ProviderBinding::new(
            ProviderId::new("custom-gateway").unwrap(),
            ProviderPolicy::Discovered,
            CatalogSource::Discovered,
        ),
    );
    builder_no_open.add_model(m_disc2).unwrap();
    let snapshot_no_open = builder_no_open.build().unwrap();

    let err = snapshot_no_open.resolve("nonexistent-unclaimed");
    assert!(
        matches!(err, Err(RegistryError::UnknownModel(m)) if m.as_str() == "nonexistent-unclaimed")
    );
    println!("[PASS] Without open provider: unknown model fails with typed UnknownModel error");
}
