use serde_json::{json, Value};

#[allow(deprecated)]
use mahoquot_providers::{
    ANTIGRAVITY_MODELS, CLAUDE_MODELS, KIRO_MODELS, VERTEX_MODELS, ZCODE_MODELS,
};

use crate::account::{AccountMember, ProviderKind};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ModelEntry {
    pub id: String,
    pub owned_by: String,
}

const DEFAULT_MODELS: [&str; 7] = [
    "gpt-5.6-sol",
    "gpt-5.6-luna",
    "gpt-5.6-terra",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.3-codex-spark",
];

fn member_matches_provider_binding(
    member: &AccountMember,
    provider_id: &mahoquot_registry::ProviderId,
    binding: &mahoquot_registry::ProviderBinding,
    model_id: &mahoquot_registry::ModelId,
) -> bool {
    if binding.policy == mahoquot_registry::ProviderPolicy::Open
        && member.kind() == ProviderKind::Codex
    {
        return true;
    }

    let kind_match = match member.kind() {
        ProviderKind::Codex => provider_id.as_str() == "codex",
        ProviderKind::Antigravity => provider_id.as_str() == "antigravity",
        ProviderKind::Claude => provider_id.as_str() == "claude",
        ProviderKind::Cursor => provider_id.as_str() == "cursor",
        ProviderKind::Kiro => provider_id.as_str() == "kiro",
        ProviderKind::Vertex => provider_id.as_str() == "vertex",
        ProviderKind::Zcode => provider_id.as_str() == "zcode",
        ProviderKind::Generic => member.provider_name() == provider_id.as_str(),
    };

    if !kind_match {
        return false;
    }

    if member.kind() == ProviderKind::Generic {
        if let Some((_, models)) = member.generic_models() {
            if !models.is_empty() {
                let upstream = binding.effective_upstream_id(model_id);
                return models
                    .iter()
                    .any(|m| m == model_id.as_str() || m == upstream);
            }
        }
    }

    true
}

pub fn project_model_entries(
    registry: &mahoquot_registry::RegistrySnapshot,
    members: &[std::sync::Arc<AccountMember>],
) -> Vec<ModelEntry> {
    let active_members: Vec<&std::sync::Arc<AccountMember>> = members
        .iter()
        .filter(|m| !m.is_manually_disabled())
        .collect();

    if active_members.is_empty() {
        return Vec::new();
    }

    let mut entries = Vec::new();

    for (model_id, descriptor) in registry.models() {
        let id_str = model_id.as_str();

        if registry
            .exclusions()
            .contains(&mahoquot_registry::ModelExclusionRule {
                model_id: model_id.clone(),
                provider_id: None,
            })
        {
            continue;
        }

        let mut can_serve = false;

        for (provider_id, binding) in &descriptor.bindings {
            if registry
                .exclusions()
                .contains(&mahoquot_registry::ModelExclusionRule {
                    model_id: model_id.clone(),
                    provider_id: Some(provider_id.clone()),
                })
            {
                continue;
            }

            if active_members
                .iter()
                .any(|m| member_matches_provider_binding(m, provider_id, binding, model_id))
            {
                can_serve = true;
                break;
            }
        }

        if can_serve {
            let owned_by = if !descriptor.owned_by.is_empty() {
                descriptor.owned_by.clone()
            } else if let Some((pid, _)) = descriptor.bindings.first_key_value() {
                pid.as_str().to_string()
            } else {
                "unknown".to_string()
            };

            entries.push(ModelEntry {
                id: id_str.to_string(),
                owned_by,
            });
        }
    }

    entries
}

/// Does a scoped key's provider/account allow list admit this pool member?
///
/// An empty list means "unrestricted" (the settings default for a key minted
/// without that dimension), and an explicit `*` is accepted so an operator can
/// write the wildcard out. Providers are matched on the routing provider name
/// (`claude`, `codex`, ...), not the vendor label carried by a model entry.
pub fn member_matches_scope(
    member: &AccountMember,
    scoped: Option<&crate::management::settings::ScopedApiKey>,
) -> bool {
    let Some(scoped) = scoped else {
        return true;
    };
    let provider = member.provider_name();
    let account = <AccountMember as mahoquot_types::PoolMember>::id(member);
    allow_list_admits(&scoped.allowed_providers, &provider)
        && allow_list_admits(&scoped.allowed_accounts, account)
}

/// An empty or wildcard allow list admits everything; otherwise the candidate
/// must appear verbatim.
pub fn allow_list_admits(allow_list: &[String], candidate: &str) -> bool {
    allow_list.is_empty()
        || allow_list
            .iter()
            .any(|allowed| allowed == "*" || allowed == candidate)
}

/// The catalog a scoped key may see: models served by the accounts it is
/// allowed to route to, narrowed further by its `allowed_models` list.
///
/// This re-projects from the members rather than filtering the published
/// entries, because a model entry's `owned_by` is a vendor label and cannot
/// answer "which of my accounts could serve this".
pub fn scoped_model_entries(
    pool: &crate::state::PoolSnapshot,
    scoped: &crate::management::settings::ScopedApiKey,
) -> Vec<ModelEntry> {
    let permitted: Vec<std::sync::Arc<AccountMember>> = pool
        .members
        .iter()
        .filter(|member| member_matches_scope(member, Some(scoped)))
        .cloned()
        .collect();

    // No permitted account can serve anything, so the catalog is empty rather
    // than the full pool's.
    if permitted.is_empty() {
        return Vec::new();
    }

    let visible = project_model_entries(&pool.registry, &permitted);
    // Keep the published entry order and any account-contributed models that
    // projection alone would not reproduce.
    let mut entries: Vec<ModelEntry> = pool
        .models
        .iter()
        .filter(|entry| visible.iter().any(|candidate| candidate.id == entry.id))
        .cloned()
        .collect();
    for entry in visible {
        if !entries.iter().any(|kept| kept.id == entry.id) {
            entries.push(entry);
        }
    }

    entries
        .into_iter()
        .filter(|entry| allow_list_admits(&scoped.allowed_models, &entry.id))
        .collect()
}

pub fn model_ids_from_env(raw: Option<&str>) -> Vec<String> {
    if let Some(s) = raw {
        let parsed: Vec<String> = s
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(ToString::to_string)
            .collect();
        if !parsed.is_empty() {
            return parsed;
        }
    }
    DEFAULT_MODELS.iter().map(|&s| s.to_string()).collect()
}

/// Advertise a model only when an account that can actually serve it is loaded,
/// so a client selecting from /v1/models never gets a routing failure.
#[allow(deprecated)]
pub fn model_entries(providers: &[ProviderKind], env_override: Option<&str>) -> Vec<ModelEntry> {
    let mut entries: Vec<ModelEntry> = Vec::new();

    if providers.contains(&ProviderKind::Codex) {
        for id in model_ids_from_env(env_override) {
            entries.push(ModelEntry {
                id,
                owned_by: "openai".to_string(),
            });
        }
    }

    if providers.contains(&ProviderKind::Antigravity) {
        for id in ANTIGRAVITY_MODELS {
            entries.push(ModelEntry {
                id: id.to_string(),
                owned_by: "google".to_string(),
            });
        }
    }

    for (kind, owned_by, models) in [
        (ProviderKind::Claude, "anthropic", CLAUDE_MODELS),
        (ProviderKind::Zcode, "z-ai", ZCODE_MODELS),
        (ProviderKind::Kiro, "kiro", KIRO_MODELS),
    ] {
        if providers.contains(&kind) {
            for id in models {
                let id = if kind == ProviderKind::Kiro {
                    format!("kiro/{id}")
                } else {
                    id.to_string()
                };
                entries.push(ModelEntry {
                    id,
                    owned_by: owned_by.to_string(),
                });
            }
        }
    }
    if providers.contains(&ProviderKind::Cursor) {
        for id in [
            "cursor/auto",
            "cursor/auto-cost",
            "cursor/auto-balance",
            "cursor/auto-intelligence",
        ] {
            entries.push(ModelEntry {
                id: id.to_string(),
                owned_by: "cursor".to_string(),
            });
        }
    }
    if providers.contains(&ProviderKind::Vertex) {
        for id in VERTEX_MODELS {
            entries.push(ModelEntry {
                id: id.to_string(),
                owned_by: "google-vertex".to_string(),
            });
        }
    }

    entries
}

pub fn generic_model_entries(members: &[std::sync::Arc<AccountMember>]) -> Vec<ModelEntry> {
    let mut entries = Vec::new();
    for member in members {
        let Some((provider, models)) = member.generic_models() else {
            continue;
        };
        for id in models {
            if !entries.iter().any(|entry: &ModelEntry| entry.id == id) {
                entries.push(ModelEntry {
                    id,
                    owned_by: provider.clone(),
                });
            }
        }
    }
    entries
}

pub fn models_payload(entries: &[ModelEntry], created_unix: i64) -> Value {
    // Static per-provider entries and per-account entries overlap by design
    // (several accounts may own the same model), and OpenAI clients treat a
    // repeated id as a catalog bug — first owner wins.
    let mut seen = std::collections::HashSet::new();
    let data: Vec<Value> = entries
        .iter()
        .filter(|entry| seen.insert(entry.id.as_str()))
        .map(|entry| {
            json!({
                "id": entry.id,
                "object": "model",
                "created": created_unix,
                "owned_by": entry.owned_by,
            })
        })
        .collect();

    json!({
        "object": "list",
        "data": data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{AccountMember, GenericAccount, ProviderAccount};
    use crate::management::settings::Settings;
    use std::sync::Arc;

    #[test]
    #[allow(deprecated)]
    fn characterization_provider_presence_exposure_all_providers() {
        // 1. Empty providers -> empty entries
        assert!(model_entries(&[], None).is_empty());

        // 2. Codex only -> DEFAULT_MODELS (7 models) owned by "openai"
        let codex = model_entries(&[ProviderKind::Codex], None);
        assert_eq!(codex.len(), 7);
        assert!(codex.iter().all(|e| e.owned_by == "openai"));
        let codex_ids: Vec<&str> = codex.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            codex_ids,
            [
                "gpt-5.6-sol",
                "gpt-5.6-luna",
                "gpt-5.6-terra",
                "gpt-5.5",
                "gpt-5.4",
                "gpt-5.4-mini",
                "gpt-5.3-codex-spark",
            ]
        );

        // 3. Antigravity only -> 14 ANTIGRAVITY_MODELS owned by "google"
        let ag = model_entries(&[ProviderKind::Antigravity], None);
        assert_eq!(ag.len(), 14);
        assert!(ag.iter().all(|e| e.owned_by == "google"));
        for model in mahoquot_providers::ANTIGRAVITY_MODELS {
            assert!(
                ag.iter().any(|e| e.id == model),
                "missing antigravity model {model}"
            );
        }

        // 4. Claude only -> 12 CLAUDE_MODELS owned by "anthropic"
        let claude = model_entries(&[ProviderKind::Claude], None);
        assert_eq!(claude.len(), 12);
        assert!(claude.iter().all(|e| e.owned_by == "anthropic"));
        for model in mahoquot_providers::CLAUDE_MODELS {
            assert!(
                claude.iter().any(|e| e.id == *model),
                "missing claude model {model}"
            );
        }

        // 5. Zcode only -> 5 ZCODE_MODELS owned by "z-ai"
        let zcode = model_entries(&[ProviderKind::Zcode], None);
        assert_eq!(zcode.len(), 5);
        assert!(zcode.iter().all(|e| e.owned_by == "z-ai"));
        for model in mahoquot_providers::ZCODE_MODELS {
            assert!(
                zcode.iter().any(|e| e.id == *model),
                "missing zcode model {model}"
            );
        }

        // 6. Kiro only -> 8 KIRO_MODELS prefixed with "kiro/" owned by "kiro"
        let kiro = model_entries(&[ProviderKind::Kiro], None);
        assert_eq!(kiro.len(), 8);
        assert!(kiro.iter().all(|e| e.owned_by == "kiro"));
        for model in mahoquot_providers::KIRO_MODELS {
            let expected_id = format!("kiro/{model}");
            assert!(
                kiro.iter().any(|e| e.id == expected_id),
                "missing kiro model {expected_id}"
            );
        }

        // 7. Cursor only -> 4 cursor/auto models owned by "cursor"
        let cursor = model_entries(&[ProviderKind::Cursor], None);
        assert_eq!(cursor.len(), 4);
        assert!(cursor.iter().all(|e| e.owned_by == "cursor"));
        let cursor_ids: Vec<&str> = cursor.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            cursor_ids,
            [
                "cursor/auto",
                "cursor/auto-cost",
                "cursor/auto-balance",
                "cursor/auto-intelligence",
            ]
        );

        // 8. Vertex only -> 21 VERTEX_MODELS owned by "google-vertex"
        let vertex = model_entries(&[ProviderKind::Vertex], None);
        assert_eq!(vertex.len(), 21);
        assert!(vertex.iter().all(|e| e.owned_by == "google-vertex"));
        for model in mahoquot_providers::VERTEX_MODELS {
            assert!(
                vertex.iter().any(|e| e.id == *model),
                "missing vertex model {model}"
            );
        }

        // 9. All 7 providers present -> 7 + 14 + 12 + 5 + 8 + 4 + 21 = 71 entries
        let all = model_entries(
            &[
                ProviderKind::Codex,
                ProviderKind::Antigravity,
                ProviderKind::Claude,
                ProviderKind::Zcode,
                ProviderKind::Kiro,
                ProviderKind::Cursor,
                ProviderKind::Vertex,
            ],
            None,
        );
        assert_eq!(all.len(), 71);

        // 10. Env override scopes Codex only
        let custom = model_entries(
            &[ProviderKind::Codex, ProviderKind::Claude],
            Some("custom-gpt, extra-gpt"),
        );
        let custom_codex: Vec<_> = custom.iter().filter(|e| e.owned_by == "openai").collect();
        assert_eq!(custom_codex.len(), 2);
        assert_eq!(custom_codex[0].id, "custom-gpt");
        assert_eq!(custom_codex[1].id, "extra-gpt");
        assert_eq!(
            custom.iter().filter(|e| e.owned_by == "anthropic").count(),
            12
        );
    }

    #[test]
    fn characterization_duplicate_owner_ordering_first_wins() {
        // Antigravity and Claude both define "claude-sonnet-4-6".
        // In model_entries, Antigravity is evaluated before Claude.
        let entries = model_entries(&[ProviderKind::Antigravity, ProviderKind::Claude], None);
        let sonnet_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.id == "claude-sonnet-4-6")
            .collect();
        assert_eq!(sonnet_entries.len(), 2);
        assert_eq!(sonnet_entries[0].owned_by, "google");
        assert_eq!(sonnet_entries[1].owned_by, "anthropic");

        // models_payload dedupes entries keeping the FIRST owner encountered
        let payload = models_payload(&entries, 0);
        let sonnet_payload = payload["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == "claude-sonnet-4-6")
            .expect("must contain claude-sonnet-4-6");
        assert_eq!(sonnet_payload["owned_by"], "google");

        // Synthetic duplicate test confirming strict first-owner-wins deduplication
        let synthetic = vec![
            ModelEntry {
                id: "shared-model".to_string(),
                owned_by: "first-provider".to_string(),
            },
            ModelEntry {
                id: "other-model".to_string(),
                owned_by: "second-provider".to_string(),
            },
            ModelEntry {
                id: "shared-model".to_string(),
                owned_by: "third-provider".to_string(),
            },
        ];
        let synthetic_payload = models_payload(&synthetic, 100);
        let data = synthetic_payload["data"].as_array().unwrap();
        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["id"], "shared-model");
        assert_eq!(data[0]["owned_by"], "first-provider");
        assert_eq!(data[1]["id"], "other-model");
        assert_eq!(data[1]["owned_by"], "second-provider");
    }

    #[test]
    fn characterization_generic_model_entries_empty_and_non_empty() {
        // Generic account with empty models produces NO model entries
        let empty_generic = Arc::new(AccountMember::for_test(ProviderAccount::Generic(
            GenericAccount {
                identity_slug: "slug-empty".to_string(),
                provider: "custom-empty".to_string(),
                label: "Custom Empty".to_string(),
                adapter: "openai-chat".to_string(),
                base_url: "https://api.empty.com/v1".to_string(),
                api_key: "k1".to_string(),
                auth_mode: "key".to_string(),
                refresh_token: String::new(),
                expired: String::new(),
                token_url: String::new(),
                client_id: String::new(),
                project_id: String::new(),
                models: vec![],
                static_headers: Default::default(),
                disabled: false,
            },
        )));
        assert!(generic_model_entries(std::slice::from_ref(&empty_generic)).is_empty());

        // Generic account with declared models exposes each model with account's provider
        let populated_generic = Arc::new(AccountMember::for_test(ProviderAccount::Generic(
            GenericAccount {
                identity_slug: "slug-pop".to_string(),
                provider: "custom-provider-a".to_string(),
                label: "Custom A".to_string(),
                adapter: "openai-chat".to_string(),
                base_url: "https://api.a.com/v1".to_string(),
                api_key: "k2".to_string(),
                auth_mode: "key".to_string(),
                refresh_token: String::new(),
                expired: String::new(),
                token_url: String::new(),
                client_id: String::new(),
                project_id: String::new(),
                models: vec!["model-alpha".to_string(), "model-beta".to_string()],
                static_headers: Default::default(),
                disabled: false,
            },
        )));
        let entries = generic_model_entries(std::slice::from_ref(&populated_generic));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "model-alpha");
        assert_eq!(entries[0].owned_by, "custom-provider-a");
        assert_eq!(entries[1].id, "model-beta");
        assert_eq!(entries[1].owned_by, "custom-provider-a");

        // Multiple generic accounts deduplicate across accounts (first account wins)
        let overlapping_generic = Arc::new(AccountMember::for_test(ProviderAccount::Generic(
            GenericAccount {
                identity_slug: "slug-overlap".to_string(),
                provider: "custom-provider-b".to_string(),
                label: "Custom B".to_string(),
                adapter: "openai-chat".to_string(),
                base_url: "https://api.b.com/v1".to_string(),
                api_key: "k3".to_string(),
                auth_mode: "key".to_string(),
                refresh_token: String::new(),
                expired: String::new(),
                token_url: String::new(),
                client_id: String::new(),
                project_id: String::new(),
                models: vec!["model-alpha".to_string(), "model-gamma".to_string()],
                static_headers: Default::default(),
                disabled: false,
            },
        )));
        let multi_entries =
            generic_model_entries(&[empty_generic, populated_generic, overlapping_generic]);
        assert_eq!(multi_entries.len(), 3);
        assert_eq!(multi_entries[0].id, "model-alpha");
        assert_eq!(multi_entries[0].owned_by, "custom-provider-a");
        assert_eq!(multi_entries[1].id, "model-beta");
        assert_eq!(multi_entries[1].owned_by, "custom-provider-a");
        assert_eq!(multi_entries[2].id, "model-gamma");
        assert_eq!(multi_entries[2].owned_by, "custom-provider-b");
    }

    #[test]
    fn characterization_v1_and_v1beta_models_schemas() {
        let entries = vec![
            ModelEntry {
                id: "gpt-5.5".to_string(),
                owned_by: "openai".to_string(),
            },
            ModelEntry {
                id: "gemini-3.1-flash-image".to_string(),
                owned_by: "google".to_string(),
            },
        ];

        // /v1/models schema
        let v1_payload = models_payload(&entries, 1725000000);
        assert_eq!(v1_payload["object"], "list");
        let v1_data = v1_payload["data"].as_array().expect("data must be array");
        assert_eq!(v1_data.len(), 2);
        assert_eq!(v1_data[0]["id"], "gpt-5.5");
        assert_eq!(v1_data[0]["object"], "model");
        assert_eq!(v1_data[0]["created"], 1725000000);
        assert_eq!(v1_data[0]["owned_by"], "openai");

        assert_eq!(v1_data[1]["id"], "gemini-3.1-flash-image");
        assert_eq!(v1_data[1]["object"], "model");
        assert_eq!(v1_data[1]["created"], 1725000000);
        assert_eq!(v1_data[1]["owned_by"], "google");

        // /v1beta/models schema
        let v1beta_payload = crate::v1beta::models_payload(&entries);
        let beta_models = v1beta_payload["models"].as_array().expect("models array");
        assert_eq!(beta_models.len(), 2);

        // Non-image model modalities
        let m0 = &beta_models[0];
        assert_eq!(m0["name"], "models/gpt-5.5");
        assert_eq!(m0["displayName"], "gpt-5.5");
        assert_eq!(m0["description"], "gpt-5.5");
        assert_eq!(
            m0["supportedInputModalities"],
            json!(["text", "image", "audio", "video"])
        );
        assert_eq!(m0["supportedOutputModalities"], json!(["text"]));
        assert_eq!(m0["supportedGenerationMethods"], json!(["generateContent"]));

        // Image model output modalities include "image"
        let m1 = &beta_models[1];
        assert_eq!(m1["name"], "models/gemini-3.1-flash-image");
        assert_eq!(m1["supportedOutputModalities"], json!(["text", "image"]));
        assert_eq!(m1["supportedGenerationMethods"], json!(["generateContent"]));

        // /v1beta/models/{model} single model payload drops supportedGenerationMethods
        let single = crate::v1beta::single_model_payload(&entries[0]);
        assert_eq!(single["name"], "models/gpt-5.5");
        assert!(single.get("supportedGenerationMethods").is_none());
        assert_eq!(single["supportedOutputModalities"], json!(["text"]));
    }

    #[test]
    fn characterization_aliases_and_exclusions_roundtrip() {
        // Defaults in Settings
        let default_settings = Settings::default();
        assert!(default_settings.oauth_excluded_models.is_empty());
        assert_eq!(default_settings.oauth_model_alias, Value::Null);

        // Populate aliases and exclusions
        let mut custom_settings = Settings::default();
        custom_settings
            .oauth_excluded_models
            .insert("openai".to_string(), vec!["gpt-4-old".to_string()]);
        custom_settings.oauth_excluded_models.insert(
            "anthropic".to_string(),
            vec!["claude-2.1".to_string(), "claude-2.0".to_string()],
        );
        custom_settings.oauth_model_alias = json!({
            "gpt-4": "gpt-5.6-sol",
            "claude": "claude-sonnet-4-6"
        });

        // Roundtrip through YAML
        let yaml = custom_settings.to_yaml().expect("must serialize");
        assert!(
            yaml.contains("oauth-excluded-models:"),
            "YAML must contain excluded models key"
        );
        assert!(yaml.contains("gpt-4-old"));
        assert!(yaml.contains("claude-2.1"));
        assert!(
            yaml.contains("oauth-model-alias:"),
            "YAML must contain model alias key"
        );

        let loaded = Settings::from_yaml(&yaml).expect("must deserialize");
        assert_eq!(
            loaded.oauth_excluded_models,
            custom_settings.oauth_excluded_models
        );
        assert_eq!(loaded.oauth_model_alias, custom_settings.oauth_model_alias);
    }

    #[test]
    fn characterization_device_oauth_defaults() {
        // Kimi device defaults
        let kimi = crate::management::oauth::device_provider("kimi").expect("kimi provider");
        assert_eq!(kimi.client_id, "17e5f671-d194-4dfb-9706-5516cb48c098");
        assert_eq!(
            kimi.device_url,
            "https://auth.kimi.com/api/oauth/device_authorization"
        );
        assert_eq!(kimi.token_url, "https://auth.kimi.com/api/oauth/token");
        assert_eq!(kimi.base_url, "https://api.kimi.com/coding/v1");
        assert_eq!(
            kimi.models,
            ["k3", "k3[1m]", "kimi-k2.7-code", "kimi-k2.6", "kimi-k2.5"]
        );
        assert!(!kimi.camel_case_poll);
        assert_eq!(kimi.scope, None);

        // Qwen device defaults
        let qwen = crate::management::oauth::device_provider("qwen").expect("qwen provider");
        assert_eq!(qwen.client_id, "e883ade2-e6e3-4d6d-adf7-f92ceff5fdcb");
        assert_eq!(
            qwen.device_url,
            "https://openapi.qoder.sh/api/v1/deviceToken/register"
        );
        assert_eq!(
            qwen.token_url,
            "https://openapi.qoder.sh/api/v1/deviceToken/poll"
        );
        assert_eq!(qwen.base_url, "https://openapi.qoder.sh/api/v1");
        assert_eq!(
            qwen.models,
            [
                "qwen3.8-max",
                "qwen3.7-max",
                "qwen3.7-plus",
                "qwen3.6-flash"
            ]
        );
        assert!(qwen.camel_case_poll);
        assert_eq!(qwen.scope, None);

        // Nous device defaults
        let nous = crate::management::oauth::device_provider("nous").expect("nous provider");
        assert_eq!(nous.client_id, "hermes-cli");
        assert_eq!(
            nous.device_url,
            "https://portal.nousresearch.com/api/oauth/device/code"
        );
        assert_eq!(
            nous.token_url,
            "https://portal.nousresearch.com/api/oauth/token"
        );
        assert_eq!(nous.base_url, "https://inference-api.nousresearch.com/v1");
        assert_eq!(
            nous.models,
            [
                "tencent/hy3:free",
                "poolside/laguna-s-2.1:free",
                "stepfun/step-3.7-flash:free",
                "poolside/laguna-xs-2.1:free"
            ]
        );
        assert!(!nous.camel_case_poll);
        assert_eq!(nous.scope, Some("inference:invoke"));

        // GitHub Copilot device defaults
        let copilot =
            crate::management::oauth::device_provider("github-copilot").expect("copilot provider");
        assert_eq!(copilot.client_id, "Iv1.b507a08c87ecfe98");
        assert_eq!(copilot.device_url, "https://github.com/login/device/code");
        assert_eq!(
            copilot.token_url,
            "https://github.com/login/oauth/access_token"
        );
        assert_eq!(
            copilot.base_url,
            "https://api.github.com/copilot_internal/v2/token"
        );
        assert_eq!(
            copilot.models,
            ["gpt-4o", "gpt-4.1", "gpt-5.3-codex", "gpt-5.4", "gpt-5.5"]
        );
        assert!(!copilot.camel_case_poll);
        assert_eq!(copilot.scope, Some("read:user"));

        // Unknown provider returns None
        assert!(crate::management::oauth::device_provider("unknown-device-provider").is_none());
    }

    #[test]
    fn payload_dedupes_ids_across_owners() {
        let entries = vec![
            ModelEntry {
                id: "claude-sonnet-4-6".to_string(),
                owned_by: "claude".to_string(),
            },
            ModelEntry {
                id: "gpt-5.6-sol".to_string(),
                owned_by: "openai".to_string(),
            },
            ModelEntry {
                id: "claude-sonnet-4-6".to_string(),
                owned_by: "antigravity".to_string(),
            },
        ];
        let payload = models_payload(&entries, 0);
        let ids: Vec<&str> = payload["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["claude-sonnet-4-6", "gpt-5.6-sol"]);
    }

    #[test]
    fn entries_track_loaded_providers() {
        let codex_only = model_entries(&[ProviderKind::Codex], None);
        assert_eq!(codex_only.len(), 7);
        assert!(codex_only.iter().all(|e| e.owned_by == "openai"));

        let ag_only = model_entries(&[ProviderKind::Antigravity], None);
        assert_eq!(ag_only.len(), 14);
        assert!(ag_only.iter().any(|e| e.id == "gemini-3.7-flash-high"));
        assert!(ag_only.iter().any(|e| e.id == "gemini-3.8-flash-high"));
        assert!(ag_only.iter().all(|e| e.owned_by == "google"));

        let both = model_entries(&[ProviderKind::Codex, ProviderKind::Antigravity], None);
        assert_eq!(both.len(), 21);

        assert!(model_entries(&[], None).is_empty());
    }

    #[test]
    fn env_override_scopes_codex_only() {
        let entries = model_entries(
            &[ProviderKind::Codex, ProviderKind::Antigravity],
            Some("gpt-x, gpt-y"),
        );
        let codex: Vec<_> = entries.iter().filter(|e| e.owned_by == "openai").collect();
        assert_eq!(codex.len(), 2);
        assert_eq!(codex[0].id, "gpt-x");
        assert_eq!(
            entries.iter().filter(|e| e.owned_by == "google").count(),
            14
        );
    }

    fn test_antigravity_account(id: &str, disabled: bool) -> Arc<AccountMember> {
        let acc: mahoquot_providers::antigravity::AntigravityAccount =
            serde_json::from_value(json!({
                "identity_slug": id,
                "access_token": "tok",
                "refresh_token": "rt",
                "project_id": "proj",
                "email": "ag@test.com",
                "expired": "2099-01-01T00:00:00Z",
                "disabled": disabled,
                "type": "antigravity",
            }))
            .unwrap();
        Arc::new(AccountMember::for_test_with_id(
            id,
            ProviderAccount::Antigravity(acc),
        ))
    }

    fn test_claude_account(id: &str, disabled: bool) -> Arc<AccountMember> {
        let acc: mahoquot_providers::claude::ClaudeAccount = serde_json::from_value(json!({
            "identity_slug": id,
            "access_token": "tok",
            "email": "c@test.com",
            "expired": "2099-01-01T00:00:00Z",
            "disabled": disabled,
            "type": "claude",
        }))
        .unwrap();
        Arc::new(AccountMember::for_test_with_id(
            id,
            ProviderAccount::Claude(acc),
        ))
    }

    #[test]
    fn test_project_model_entries_fixture_model_and_absent_accounts() {
        use mahoquot_registry::{
            CatalogSource, CatalogVersion, ModelCapability, ModelDescriptor, ModelId,
            ProviderBinding, ProviderId, ProviderPolicy, RegistryBuilder,
        };
        let mut builder = RegistryBuilder::new(CatalogVersion(1), CatalogSource::LocalOverride);
        builder.register_provider(ProviderId::codex(), ProviderPolicy::Open);
        builder.register_provider(ProviderId::antigravity(), ProviderPolicy::Closed);
        builder.register_provider(ProviderId::claude(), ProviderPolicy::Closed);

        let codex_id = ModelId::new("model-codex").unwrap();
        let mut codex_desc = ModelDescriptor::new(codex_id.clone(), "openai");
        codex_desc.bindings.insert(
            ProviderId::codex(),
            ProviderBinding::new(
                ProviderId::codex(),
                ProviderPolicy::Open,
                CatalogSource::LocalOverride,
            )
            .with_capabilities([ModelCapability::Chat]),
        );
        builder.add_model(codex_desc).unwrap();

        let ag_id = ModelId::new("gemini-next-flash-high").unwrap();
        let mut ag_desc = ModelDescriptor::new(ag_id.clone(), "google");
        ag_desc.bindings.insert(
            ProviderId::antigravity(),
            ProviderBinding::new(
                ProviderId::antigravity(),
                ProviderPolicy::Closed,
                CatalogSource::LocalOverride,
            )
            .with_capabilities([ModelCapability::Chat]),
        );
        builder.add_model(ag_desc).unwrap();

        let claude_id = ModelId::new("model-claude").unwrap();
        let mut claude_desc = ModelDescriptor::new(claude_id.clone(), "anthropic");
        claude_desc.bindings.insert(
            ProviderId::claude(),
            ProviderBinding::new(
                ProviderId::claude(),
                ProviderPolicy::Closed,
                CatalogSource::LocalOverride,
            )
            .with_capabilities([ModelCapability::Chat]),
        );
        builder.add_model(claude_desc).unwrap();

        let reg = builder.build().unwrap();

        // Only Antigravity member loaded
        let ag_member = test_antigravity_account("ag-1", false);

        let projected = project_model_entries(&reg, &[ag_member]);
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].id, "gemini-next-flash-high");
        assert_eq!(projected[0].owned_by, "google");
    }

    #[test]
    fn test_project_model_entries_stable_owner_from_metadata() {
        use mahoquot_registry::{
            CatalogSource, CatalogVersion, ModelCapability, ModelDescriptor, ModelId,
            ProviderBinding, ProviderId, ProviderPolicy, RegistryBuilder,
        };
        let mut builder = RegistryBuilder::new(CatalogVersion(1), CatalogSource::LocalOverride);
        builder.register_provider(ProviderId::antigravity(), ProviderPolicy::Closed);
        builder.register_provider(ProviderId::claude(), ProviderPolicy::Closed);

        let shared_id = ModelId::new("claude-sonnet-4-6").unwrap();
        let mut shared_desc = ModelDescriptor::new(shared_id.clone(), "google");
        shared_desc.bindings.insert(
            ProviderId::claude(),
            ProviderBinding::new(
                ProviderId::claude(),
                ProviderPolicy::Closed,
                CatalogSource::LocalOverride,
            )
            .with_capabilities([ModelCapability::Chat]),
        );
        shared_desc.bindings.insert(
            ProviderId::antigravity(),
            ProviderBinding::new(
                ProviderId::antigravity(),
                ProviderPolicy::Closed,
                CatalogSource::LocalOverride,
            )
            .with_capabilities([ModelCapability::Chat]),
        );
        builder.add_model(shared_desc).unwrap();
        let reg = builder.build().unwrap();

        let claude_member = test_claude_account("claude-1", false);
        let ag_member = test_antigravity_account("ag-1", false);

        // Load Claude first, then Antigravity
        let projected = project_model_entries(&reg, &[claude_member, ag_member]);
        assert_eq!(projected.len(), 1, "duplicates must appear only once");
        assert_eq!(projected[0].id, "claude-sonnet-4-6");
        assert_eq!(
            projected[0].owned_by, "google",
            "owner must be stable from descriptor metadata, not account load order"
        );
    }

    #[test]
    fn test_project_model_entries_respects_manual_disabled() {
        use mahoquot_registry::{
            CatalogSource, CatalogVersion, ModelCapability, ModelDescriptor, ModelId,
            ProviderBinding, ProviderId, ProviderPolicy, RegistryBuilder,
        };
        let mut builder = RegistryBuilder::new(CatalogVersion(1), CatalogSource::LocalOverride);
        builder.register_provider(ProviderId::antigravity(), ProviderPolicy::Closed);

        let ag_id = ModelId::new("gemini-next-flash-high").unwrap();
        let mut ag_desc = ModelDescriptor::new(ag_id.clone(), "google");
        ag_desc.bindings.insert(
            ProviderId::antigravity(),
            ProviderBinding::new(
                ProviderId::antigravity(),
                ProviderPolicy::Closed,
                CatalogSource::LocalOverride,
            )
            .with_capabilities([ModelCapability::Chat]),
        );
        builder.add_model(ag_desc).unwrap();
        let reg = builder.build().unwrap();

        // Account with inner disabled = true
        let disabled_inner = test_antigravity_account("ag-disabled", true);
        assert!(project_model_entries(&reg, &[disabled_inner]).is_empty());

        // Account with Health::Disabled
        let health_disabled = test_antigravity_account("ag-health-disabled", false);
        health_disabled.set_health(mahoquot_types::Health::Disabled);
        assert!(project_model_entries(&reg, &[health_disabled]).is_empty());
    }

    #[test]
    fn test_project_model_entries_cooldown_and_unsupported_feedback_do_not_churn() {
        use mahoquot_registry::{
            CatalogSource, CatalogVersion, ModelCapability, ModelDescriptor, ModelId,
            ProviderBinding, ProviderId, ProviderPolicy, RegistryBuilder,
        };
        let mut builder = RegistryBuilder::new(CatalogVersion(1), CatalogSource::LocalOverride);
        builder.register_provider(ProviderId::antigravity(), ProviderPolicy::Closed);

        let ag_id = ModelId::new("gemini-next-flash-high").unwrap();
        let mut ag_desc = ModelDescriptor::new(ag_id.clone(), "google");
        ag_desc.bindings.insert(
            ProviderId::antigravity(),
            ProviderBinding::new(
                ProviderId::antigravity(),
                ProviderPolicy::Closed,
                CatalogSource::LocalOverride,
            )
            .with_capabilities([ModelCapability::Chat]),
        );
        builder.add_model(ag_desc).unwrap();
        let reg = builder.build().unwrap();

        // Account in cooldown
        let cooldown_member = test_antigravity_account("ag-cooldown", false);
        cooldown_member.set_health(mahoquot_types::Health::Cooldown {
            until_unix_ms: 9999999999999,
        });

        // Temporary cooldown must NOT churn the public catalog
        let projected = project_model_entries(&reg, &[cooldown_member]);
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].id, "gemini-next-flash-high");

        // Transient unsupported feedback must NOT churn the public catalog
        let unsupported_member = test_antigravity_account("ag-unsupported", false);
        unsupported_member.mark_model_unsupported("gemini-next-flash-high");
        let projected = project_model_entries(&reg, &[unsupported_member]);
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].id, "gemini-next-flash-high");
    }

    #[test]
    fn test_project_model_entries_all_advertised_resolve_to_binding() {
        use mahoquot_registry::{
            CatalogSource, CatalogVersion, ModelCapability, ModelDescriptor, ModelId,
            ProviderBinding, ProviderId, ProviderPolicy, RegistryBuilder,
        };
        let mut builder = RegistryBuilder::new(CatalogVersion(1), CatalogSource::LocalOverride);
        builder.register_provider(ProviderId::antigravity(), ProviderPolicy::Closed);

        let ag_id = ModelId::new("gemini-next-flash-high").unwrap();
        let mut ag_desc = ModelDescriptor::new(ag_id.clone(), "google");
        ag_desc.bindings.insert(
            ProviderId::antigravity(),
            ProviderBinding::new(
                ProviderId::antigravity(),
                ProviderPolicy::Closed,
                CatalogSource::LocalOverride,
            )
            .with_capabilities([ModelCapability::Chat]),
        );
        builder.add_model(ag_desc).unwrap();
        let reg = builder.build().unwrap();

        let ag_member = test_antigravity_account("ag-1", false);

        let projected = project_model_entries(&reg, &[ag_member]);
        for entry in &projected {
            let resolved = reg.resolve(&entry.id).expect("must resolve in registry");
            assert!(!resolved.eligible_bindings.is_empty());
        }
    }
}
