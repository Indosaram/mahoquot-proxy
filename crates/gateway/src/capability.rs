//! Model-capability gating for protocol-specific surfaces.
//!
//! Surface eligibility comes from the binding resolved in the request's loaded
//! [`PoolSnapshot`]. Capabilities are deliberately read from provider bindings,
//! not inferred from model names or pooled descriptor capabilities: two
//! providers may expose the same public model ID with different protocols.
//! Error strings remain verbatim because clients match on them; they were
//! captured from CLIProxyAPI v7.2.140 sharing this credential pool.

use mahoquot_registry::{ModelCapability, ProviderId, ResolvedModel};
use serde_json::{json, Value};

use crate::account::{AccountMember, ProviderKind};
use crate::state::PoolSnapshot;

/// The model `/openai/v1/videos` resolves against regardless of the request body.
pub const OPENAI_VIDEO_MODEL: &str = "grok-imagine-video";

const IMAGE_HINT: &str =
    "Use gpt-image-1.5, gpt-image-2, or a configured openai-compatibility image model.";

const VIDEO_HINT: &str = "No reference-backed video model is configured.";

/// `{"error":{"message":..,"type":"invalid_request_error"}}` - no code/param.
pub fn unsupported_on_surface(model: &str, surface: &str, hint: &str) -> Value {
    json!({"error": {
        "message": format!("Model {model} is not supported on {surface}. {hint}"),
        "type": "invalid_request_error",
    }})
}

/// `{"error":{..,"code":"model_not_found","param":"model"}}`.
pub fn unknown_provider(model: &str) -> Value {
    json!({"error": {
        "message": format!("unknown provider for model {model}"),
        "type": "invalid_request_error",
        "code": "model_not_found",
        "param": "model",
    }})
}

fn member_matches_provider(member: &AccountMember, provider: &ProviderId) -> bool {
    match provider.as_str() {
        "antigravity" => member.kind() == ProviderKind::Antigravity,
        "claude" => member.kind() == ProviderKind::Claude,
        "cursor" => member.kind() == ProviderKind::Cursor,
        "kiro" => member.kind() == ProviderKind::Kiro,
        "vertex" => member.kind() == ProviderKind::Vertex,
        "zcode" => member.kind() == ProviderKind::Zcode,
        "codex" => member.kind() == ProviderKind::Codex,
        other => member.kind() == ProviderKind::Generic && member.provider_name() == other,
    }
}

fn generic_account_allows(member: &AccountMember, requested: &str, canonical: &str) -> bool {
    let Some((_, models)) = member.generic_models() else {
        return true;
    };
    models.is_empty()
        || models
            .iter()
            .any(|model| model == requested || model == canonical)
}

fn member_is_dynamically_allowed(member: &AccountMember, requested: &str, canonical: &str) -> bool {
    if !generic_account_allows(member, requested, canonical) {
        return false;
    }
    let unsupported = member
        .unsupported_models
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    !unsupported
        .iter()
        .any(|model| model == requested || model == canonical)
}

/// Resolve a model and retain only bindings that explicitly declare the
/// requested protocol. Sparse discovery therefore contributes an ID but cannot
/// invent image/video/realtime/count-token support.
pub fn resolve_registry_capability(
    pool: &PoolSnapshot,
    requested: &str,
    capability: ModelCapability,
) -> Option<ResolvedModel> {
    let mut resolved = pool.registry.resolve(requested).ok()?;
    resolved
        .eligible_bindings
        .retain(|binding| binding.capabilities.contains(&capability));
    if resolved.eligible_bindings.is_empty() {
        return None;
    }
    resolved.effective_capabilities = resolved
        .eligible_bindings
        .iter()
        .flat_map(|binding| binding.capabilities.iter().copied())
        .collect();
    Some(resolved)
}

/// Resolve a capable binding that also has a matching loaded account. Media
/// routes use this form because the guard and the relay must choose the same
/// provider-specific capability for duplicate public IDs.
pub fn resolve_for_capability(
    pool: &PoolSnapshot,
    requested: &str,
    capability: ModelCapability,
) -> Option<ResolvedModel> {
    let mut resolved = resolve_registry_capability(pool, requested, capability)?;
    let canonical = resolved.canonical_id.as_str();
    resolved.eligible_bindings.retain(|binding| {
        pool.members.iter().any(|member| {
            member_matches_provider(member, &binding.provider_id)
                && member_is_dynamically_allowed(member, requested, canonical)
        })
    });
    (!resolved.eligible_bindings.is_empty()).then_some(resolved)
}

pub fn member_supports_capability(
    pool: &PoolSnapshot,
    member: &AccountMember,
    requested: &str,
    capability: ModelCapability,
) -> bool {
    let Some(resolved) = resolve_for_capability(pool, requested, capability) else {
        return false;
    };
    resolved.eligible_bindings.iter().any(|binding| {
        member_matches_provider(member, &binding.provider_id)
            && member_is_dynamically_allowed(member, requested, resolved.canonical_id.as_str())
    })
}

pub fn check_image(pool: &PoolSnapshot, model: &str) -> Option<Value> {
    if resolve_for_capability(pool, model, ModelCapability::Image).is_some() {
        return None;
    }
    Some(unsupported_on_surface(
        model,
        "/v1/images/generations or /v1/images/edits",
        IMAGE_HINT,
    ))
}

pub fn check_video(pool: &PoolSnapshot, model: &str) -> Option<Value> {
    if resolve_for_capability(pool, model, ModelCapability::Video).is_some() {
        return None;
    }
    Some(unsupported_on_surface(
        model,
        "/v1/videos/generations, /v1/videos/edits, or /v1/videos/extensions",
        VIDEO_HINT,
    ))
}

pub fn count_tokens_error(model: &str) -> Value {
    json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": format!("Model {model} does not support token counting")
        }
    })
}

/// Model named in a request body, falling back to the empty string like the
/// upstream does when the field is absent.
pub fn model_of(body: &Value) -> &str {
    body.get("model").and_then(Value::as_str).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mahoquot_providers::CodexAccount;

    use super::*;
    use crate::account::{AccountMember, ProviderAccount};
    use crate::runtime_state::compute_candidate_composition;

    fn codex_pool() -> PoolSnapshot {
        let registry = Arc::new(mahoquot_registry::embedded_registry_snapshot().unwrap());
        let member = Arc::new(AccountMember::for_test(ProviderAccount::Codex(
            CodexAccount::default(),
        )));
        compute_candidate_composition(1, vec![member], registry, None).unwrap()
    }

    #[test]
    fn characterization_image_and_video_rejection_text() {
        let pool = codex_pool();
        // Supported image models pass surface gate (return None)
        assert!(check_image(&pool, "gpt-image-1.5").is_none());
        assert!(check_image(&pool, "gpt-image-2").is_none());

        // Unsupported image model returns exact upstream message shape
        let img_err =
            check_image(&pool, "dall-e-3").expect("dall-e-3 is unsupported on direct surface");
        assert_eq!(
            img_err["error"]["message"],
            "Model dall-e-3 is not supported on /v1/images/generations or /v1/images/edits. Use gpt-image-1.5, gpt-image-2, or a configured openai-compatibility image model."
        );
        assert_eq!(img_err["error"]["type"], "invalid_request_error");
        assert!(img_err["error"].get("code").is_none());

        // Unsupported video model returns exact upstream message shape
        let vid_err = check_video(&pool, "sora-1.0").expect("sora-1.0 is unsupported");
        assert_eq!(
            vid_err["error"]["message"],
            "Model sora-1.0 is not supported on /v1/videos/generations, /v1/videos/edits, or /v1/videos/extensions. No reference-backed video model is configured."
        );
        assert_eq!(vid_err["error"]["type"], "invalid_request_error");
        assert!(vid_err["error"].get("code").is_none());

        let unk_err = unknown_provider("unknown-custom-model");
        assert_eq!(
            unk_err["error"]["message"],
            "unknown provider for model unknown-custom-model"
        );
        assert_eq!(unk_err["error"]["type"], "invalid_request_error");
        assert_eq!(unk_err["error"]["code"], "model_not_found");
        assert_eq!(unk_err["error"]["param"], "model");
    }

    #[test]
    fn image_error_matches_captured_upstream_text() {
        let pool = codex_pool();
        let v = check_image(&pool, "gemini-3-pro-image-preview").expect("unsupported");
        assert_eq!(
            v["error"]["message"].as_str().unwrap(),
            "Model gemini-3-pro-image-preview is not supported on \
             /v1/images/generations or /v1/images/edits. Use gpt-image-1.5, \
             gpt-image-2, or a configured openai-compatibility image model."
        );
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert!(v["error"].get("code").is_none());
    }

    #[test]
    fn video_error_matches_captured_upstream_text() {
        let pool = codex_pool();
        let v = check_video(&pool, "veo-3.1").expect("unsupported");
        assert_eq!(
            v["error"]["message"].as_str().unwrap(),
            "Model veo-3.1 is not supported on /v1/videos/generations, \
             /v1/videos/edits, or /v1/videos/extensions. No reference-backed \
             video model is configured."
        );
    }

    #[test]
    fn unknown_provider_carries_code_and_param() {
        let v = unknown_provider("grok-imagine-video");
        assert_eq!(
            v["error"]["message"],
            "unknown provider for model grok-imagine-video"
        );
        assert_eq!(v["error"]["code"], "model_not_found");
        assert_eq!(v["error"]["param"], "model");
    }

    #[test]
    fn reference_backed_image_models_pass_surface_gate() {
        assert!(check_image(&codex_pool(), "gpt-image-2").is_none());
    }

    #[test]
    fn unimplemented_media_models_are_not_advertised_as_routable() {
        let pool = codex_pool();
        assert!(check_image(&pool, "grok-imagine-image").is_some());
        assert!(check_image(&pool, "grok-imagine-image-quality").is_some());
        assert!(check_image(&pool, "grok-imagine-image-2.0").is_some());
        assert!(check_video(&pool, "grok-imagine-video").is_some());
    }

    #[test]
    fn open_bindings_do_not_gain_non_chat_capabilities_implicitly() {
        let pool = codex_pool();
        let resolved = pool.registry.resolve("uncatalogued-open-model").unwrap();
        assert!(resolved
            .eligible_bindings
            .iter()
            .all(|binding| binding.policy == mahoquot_registry::ProviderPolicy::Open));
        assert!(
            resolve_for_capability(&pool, "uncatalogued-open-model", ModelCapability::Image)
                .is_none()
        );
    }
}
