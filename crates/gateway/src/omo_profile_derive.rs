//! Derive the client model catalog without mutating registry or stored settings.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use mahoquot_registry::{ModelCapability, RegistrySnapshot as Registry, ThinkingCapability};
use serde_json::{json, Map, Value};

use crate::management::settings::OmoProfile;

/// Return a new revision only when the canonical model contents change.
///
/// `maxTokens` comes from the catalog's optional `max_output_tokens` metadata.
/// It is null (unknown) when the catalog omits it or the value does not parse;
/// the client then falls back to its own default. A guess would be worse than
/// null: a limit below the provider's real ceiling truncates every long answer.
/// Reading the clock for `updated_at` is the only external dependency; nothing
/// is saved.
pub fn derive_profile(registry: &Registry, existing: &OmoProfile) -> Option<OmoProfile> {
    if registry.models().is_empty() {
        return None;
    }

    // `remove` is the operator's tombstone list: a model named there must never
    // be re-introduced by derivation. Honouring it also keeps the emitted
    // document valid, since the management validator rejects any id present in
    // both `models` and `remove`.
    let tombstones: BTreeSet<&str> = existing.remove.iter().map(String::as_str).collect();
    let mut models = Vec::new();
    let mut derived = BTreeSet::new();
    for (id, descriptor) in registry.models() {
        if tombstones.contains(id.as_str()) {
            continue;
        }
        let Ok(resolved) = registry.resolve(id.as_str()) else {
            continue;
        };
        if resolved.eligible_bindings.is_empty()
            || !descriptor.metadata.contains_key("context_limit")
        {
            continue;
        }
        let mut input = vec!["text"];
        if descriptor.has_capability(ModelCapability::Image) {
            input.push("image");
        }
        let reasoning = descriptor.bindings.keys().any(|provider| {
            matches!(
                descriptor.thinking_capability(provider.as_str()),
                Ok(ThinkingCapability::Supported(_))
            )
        });
        // Presence, not parseability, is the inclusion contract. Keep unknown
        // numeric metadata null rather than silently excluding the model.
        let context = descriptor.metadata["context_limit"].parse::<u64>().ok();
        let output = descriptor
            .metadata
            .get("max_output_tokens")
            .and_then(|limit| limit.parse::<u64>().ok());
        let model = json!({
            "id": id.as_str(),
            "name": descriptor.display_name.as_deref().unwrap_or(id.as_str()),
            "contextWindow": context,
            "maxTokens": output,
            "input": input,
            "reasoning": reasoning,
        });
        models.push(model.as_object().expect("model is an object").clone());
        derived.insert(id.as_str());
    }

    // Preserve curated entries unless the registry can no longer serve them.
    // The embedded catalog is not exhaustive: routable identifiers such as
    // `gpt-6-luna` resolve through the open-provider fallback with no catalog
    // descriptor at all, so "absent from the catalog" must never mean delete.
    // Dropping is reserved for entries the registry actively refuses (globally
    // excluded, or unbound with no open provider left to serve them).
    models.extend(
        existing
            .models
            .iter()
            .filter(|model| {
                let Some(id) = model.get("id").and_then(Value::as_str) else {
                    return false;
                };
                !derived.contains(id) && !tombstones.contains(id) && registry.resolve(id).is_ok()
            })
            .cloned(),
    );
    sort_models(&mut models);
    // Never blank a populated profile. Derivation can legitimately drop every
    // curated entry (all excluded, or no provider left to serve them); writing
    // that empty list would wipe four machines at once, so the last good list
    // stands until the registry can serve something again.
    if models.is_empty() && !existing.models.is_empty() {
        return None;
    }
    let mut previous = existing.models.clone();
    sort_models(&mut previous);
    // JSON value equality compares object contents independent of key order,
    // including nested opaque fields on entries preserved verbatim.
    if Value::from(models.clone()) == Value::from(previous) {
        return None;
    }

    Some(OmoProfile {
        revision: existing.revision + 1,
        updated_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after the Unix epoch")
            .as_secs(),
        compat: existing.compat.clone(),
        models,
        remove: existing.remove.clone(),
    })
}

fn sort_models(models: &mut [Map<String, Value>]) {
    models.sort_by(|a, b| {
        a.get("id")
            .and_then(Value::as_str)
            .cmp(&b.get("id").and_then(Value::as_str))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use mahoquot_registry::{
        CatalogSource, CatalogVersion, ModelDescriptor, ModelExclusionRule, ModelId,
        ProviderBinding, ProviderId, ProviderPolicy,
    };

    fn model(id: &str, metadata: bool, eligible: bool) -> ModelDescriptor {
        let mut model = ModelDescriptor::new(ModelId::new(id).unwrap(), "test")
            .with_capabilities([ModelCapability::Chat]);
        if metadata {
            model
                .metadata
                .insert("context_limit".into(), "200000".into());
        }
        if eligible {
            model = model.with_binding(ProviderBinding::new(
                ProviderId::new("test").unwrap(),
                ProviderPolicy::Closed,
                CatalogSource::EmbeddedFallback,
            ));
        }
        model
    }

    fn registry(models: Vec<ModelDescriptor>) -> Registry {
        Registry {
            version: CatalogVersion::new(1),
            source: CatalogSource::EmbeddedFallback,
            models: models
                .into_iter()
                .map(|model| (model.id.clone(), model))
                .collect(),
            providers: Default::default(),
            aliases: Default::default(),
            exclusions: Default::default(),
        }
    }

    /// Registry variant with one open provider, so identifiers absent from the
    /// catalog resolve through the open-provider fallback (production shape).
    fn registry_with_open_provider(models: Vec<ModelDescriptor>) -> Registry {
        let mut registry = registry(models);
        registry.providers.insert(
            ProviderId::new("open-provider").unwrap(),
            ProviderPolicy::Open,
        );
        registry
    }

    fn entry(id: &str) -> Map<String, Value> {
        json!({"id": id, "name": "Manual", "custom": {"keep": [1, 2]}})
            .as_object()
            .unwrap()
            .clone()
    }

    #[test]
    fn only_eligible_metadata_models_are_included() {
        let mut registry = registry(vec![
            model("b", true, true),
            model("a", true, true),
            model("no-metadata", false, true),
            model("no-binding", true, false),
            model("excluded", true, true),
        ]);
        registry.exclusions.insert(ModelExclusionRule {
            model_id: ModelId::new("excluded").unwrap(),
            provider_id: None,
        });
        let result = derive_profile(&registry, &OmoProfile::default()).unwrap();
        assert_eq!(
            result
                .models
                .iter()
                .map(|m| m["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    #[test]
    fn new_model_without_metadata_is_excluded() {
        assert!(derive_profile(
            &registry(vec![model("new", false, true)]),
            &OmoProfile::default()
        )
        .is_none());
    }

    #[test]
    fn existing_model_without_metadata_is_preserved_verbatim() {
        let existing = OmoProfile {
            models: vec![entry("manual")],
            ..Default::default()
        };
        let result = derive_profile(
            &registry(vec![model("manual", false, true), model("new", true, true)]),
            &existing,
        )
        .unwrap();
        assert_eq!(result.models[0], existing.models[0]);
    }

    #[test]
    fn existing_unresolvable_or_ineligible_models_are_dropped_but_never_all() {
        let existing = OmoProfile {
            models: vec![entry("excluded"), entry("unbound")],
            ..Default::default()
        };
        let mut registry = registry(vec![
            model("excluded", true, true),
            model("unbound", true, false),
        ]);
        registry.exclusions.insert(ModelExclusionRule {
            model_id: ModelId::new("excluded").unwrap(),
            provider_id: None,
        });
        // Both entries are unservable, so the stored list stands untouched
        // rather than collapsing to an empty catalog.
        assert!(derive_profile(&registry, &existing).is_none());
    }

    #[test]
    fn curated_model_absent_from_catalog_is_preserved() {
        let existing = OmoProfile {
            models: vec![entry("gpt-6-luna")],
            ..Default::default()
        };
        let result = derive_profile(
            &registry_with_open_provider(vec![model("new", true, true)]),
            &existing,
        )
        .unwrap();
        let ids = result
            .models
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["gpt-6-luna", "new"]);
        assert_eq!(result.models[0], existing.models[0]);
    }

    #[test]
    fn curated_model_refused_by_registry_is_dropped() {
        let existing = OmoProfile {
            models: vec![entry("excluded"), entry("gpt-6-luna")],
            ..Default::default()
        };
        let mut registry =
            registry_with_open_provider(vec![model("new", true, true), model("gone", true, true)]);
        registry.exclusions.insert(ModelExclusionRule {
            model_id: ModelId::new("excluded").unwrap(),
            provider_id: None,
        });
        let result = derive_profile(&registry, &existing).unwrap();
        let ids = result
            .models
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["gone", "gpt-6-luna", "new"]);
    }

    #[test]
    fn an_all_unservable_profile_is_not_wiped() {
        let existing = OmoProfile {
            models: vec![entry("excluded")],
            ..Default::default()
        };
        let mut registry = registry_with_open_provider(vec![model("unready", false, true)]);
        registry.exclusions.insert(ModelExclusionRule {
            model_id: ModelId::new("excluded").unwrap(),
            provider_id: None,
        });
        assert!(derive_profile(&registry, &existing).is_none());
    }

    #[test]
    fn curated_model_without_open_provider_is_dropped() {
        let existing = OmoProfile {
            models: vec![entry("unserved")],
            ..Default::default()
        };
        let result =
            derive_profile(&registry(vec![model("ready", true, true)]), &existing).unwrap();
        let ids = result
            .models
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["ready"]);
    }

    #[test]
    fn empty_registry_does_not_wipe_profile() {
        let existing = OmoProfile {
            models: vec![entry("keep")],
            ..Default::default()
        };
        assert!(derive_profile(&registry(vec![]), &existing).is_none());
        assert!(derive_profile(&registry(vec![]), &OmoProfile::default()).is_none());
    }

    #[test]
    fn second_derivation_and_reordering_do_not_change_revision() {
        let registry = registry(vec![model("b", true, true), model("a", true, true)]);
        let mut first = derive_profile(&registry, &OmoProfile::default()).unwrap();
        assert!(derive_profile(&registry, &first).is_none());
        first.models.reverse();
        assert!(derive_profile(&registry, &first).is_none());
    }

    #[test]
    fn compat_and_remove_are_unchanged() {
        let existing = OmoProfile {
            compat: json!({"sendSessionAffinityHeaders": true, "sessionAffinityFormat": "custom", "supportsReasoningEffort": false, "opaque": {"x": 1}}).as_object().unwrap().clone(),
            remove: vec!["z".into(), "a".into(), "z".into()],
            ..Default::default()
        };
        let result = derive_profile(&registry(vec![model("b", true, true)]), &existing).unwrap();
        assert_eq!(result.compat, existing.compat);
        assert_eq!(result.remove, existing.remove);
    }

    #[test]
    fn tombstoned_model_is_never_reintroduced() {
        // `remove` is the operator's tombstone list. Re-adding a tombstoned id
        // would also produce a document the management validator rejects, since
        // an id may not appear in both `models` and `remove`.
        let existing = OmoProfile {
            models: vec![entry("keep")],
            remove: vec!["retired".into()],
            ..Default::default()
        };
        let result = derive_profile(
            &registry_with_open_provider(vec![
                model("retired", true, true),
                model("fresh", true, true),
            ]),
            &existing,
        )
        .unwrap();
        let ids = result
            .models
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["fresh", "keep"]);
        assert_eq!(result.remove, existing.remove);
    }

    #[test]
    fn curated_entry_that_is_also_tombstoned_is_dropped() {
        let existing = OmoProfile {
            models: vec![entry("retired")],
            remove: vec!["retired".into()],
            ..Default::default()
        };
        let result = derive_profile(
            &registry_with_open_provider(vec![model("fresh", true, true)]),
            &existing,
        )
        .unwrap();
        let ids = result
            .models
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["fresh"]);
    }

    #[test]
    fn fields_use_descriptor_metadata_and_capabilities() {
        let mut descriptor = model("vision", true, true).with_display_name("Vision");
        descriptor.capabilities.insert(ModelCapability::Image);
        descriptor
            .metadata
            .insert("thinking.test".into(), r#"{"min":1024,"max":8192}"#.into());
        let result = derive_profile(&registry(vec![descriptor]), &OmoProfile::default()).unwrap();
        assert_eq!(
            json!(result.models[0]),
            json!({"id": "vision", "name": "Vision", "contextWindow": 200000, "maxTokens": null, "input": ["text", "image"], "reasoning": true})
        );
    }

    #[test]
    fn output_limit_metadata_becomes_max_tokens() {
        // A catalog-supplied ceiling must reach the client verbatim: the client
        // otherwise applies its own default and truncates long answers there.
        let mut descriptor = model("neon", true, true);
        descriptor
            .metadata
            .insert("max_output_tokens".into(), "65536".into());
        let result = derive_profile(&registry(vec![descriptor]), &OmoProfile::default()).unwrap();
        assert_eq!(result.models[0]["maxTokens"], json!(65536));
        assert_eq!(result.models[0]["contextWindow"], json!(200000));
    }

    #[test]
    fn absent_or_unparsable_output_limit_stays_null() {
        let mut unparsable = model("a", true, true);
        unparsable
            .metadata
            .insert("max_output_tokens".into(), "unknown".into());
        let mut empty = model("b", true, true);
        empty
            .metadata
            .insert("max_output_tokens".into(), String::new());
        let result =
            derive_profile(&registry(vec![unparsable, empty]), &OmoProfile::default()).unwrap();
        for model in result.models {
            assert_eq!(model["maxTokens"], Value::Null);
        }
    }

    #[test]
    fn unknown_and_unsupported_thinking_are_not_reasoning() {
        let mut unsupported = model("unsupported", true, true);
        unsupported
            .metadata
            .insert("thinking.test".into(), "null".into());
        let result = derive_profile(
            &registry(vec![unsupported, model("unknown", true, true)]),
            &OmoProfile::default(),
        )
        .unwrap();
        for model in result.models {
            assert_eq!(model["reasoning"], false);
            assert_eq!(model["input"], json!(["text"]));
            assert_eq!(model["name"], model["id"]);
        }
    }

    #[test]
    fn changed_profile_increments_revision_and_updates_timestamp() {
        let existing = OmoProfile {
            revision: 19,
            updated_at: 1,
            ..Default::default()
        };
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let result = derive_profile(&registry(vec![model("new", true, true)]), &existing).unwrap();
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(result.revision, 20);
        assert!((before..=after).contains(&result.updated_at));
        assert_eq!(existing.updated_at, 1);
    }
}
