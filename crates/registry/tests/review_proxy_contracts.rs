use mahoquot_registry::*;

#[test]
fn excluded_closed_binding_does_not_become_an_open_provider_route() {
    // Given: a closed model whose only binding is excluded.
    let mut registry = embedded_registry_snapshot().unwrap();
    let model = ModelId::new("claude-3-7-sonnet-20250219").unwrap();
    registry.exclusions.insert(ModelExclusionRule {
        model_id: model.clone(),
        provider_id: Some(ProviderId::claude()),
    });

    // When: resolving it with Codex still registered as an open provider.
    let resolved = registry.resolve(model.as_str());

    // Then: the closed model cannot leak into Codex's negative space.
    assert!(matches!(resolved, Err(RegistryError::UnknownModel(_))));
}
