//! OpenCode provider adapter.
//!
//! Provides account loading, header generation for OpenCode free models.

pub const OPENCODE_USER_AGENT: &str = "opencode/2.2.2 (aidev_client;)";
pub const OPENCODE_UPSTREAM_BASE: &str = "https://opencode.ai/inference/openai";

#[derive(Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct OpenCodeAccount {
    pub api_key: String,
    pub identity_slug: String,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub expired: String,
}

impl std::fmt::Debug for OpenCodeAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenCodeAccount")
            .field("identity_slug", &self.identity_slug)
            .field("api_key", &"[REDACTED]")
            .field("disabled", &self.disabled)
            .finish()
    }
}

impl OpenCodeAccount {
    pub fn build_upstream_headers(&self) -> Vec<(String, String)> {
        vec![
            (
                "Authorization".to_string(),
                format!("Bearer {}", self.api_key),
            ),
            ("User-Agent".to_string(), OPENCODE_USER_AGENT.to_string()),
        ]
    }

    pub fn is_expired(&self, _now_unix: i64) -> bool {
        false
    }
}

use mahoquot_registry::{embedded_snapshot, ProviderContribution, ProviderId, RegistrySnapshot};

pub fn provider_id() -> ProviderId {
    ProviderId::new("opencode").expect("valid provider id")
}

pub fn contribution(snapshot: &RegistrySnapshot) -> ProviderContribution {
    snapshot.contribution_for_provider(&provider_id())
}

pub fn default_contribution() -> ProviderContribution {
    contribution(embedded_snapshot())
}

pub fn supported_models(snapshot: &RegistrySnapshot) -> Vec<String> {
    contribution(snapshot).supported_model_ids()
}

pub fn is_opencode_model_in_snapshot(snapshot: &RegistrySnapshot, model: &str) -> bool {
    contribution(snapshot).supports_model(model)
}

pub fn is_opencode_model(model: &str) -> bool {
    is_opencode_model_in_snapshot(embedded_snapshot(), model)
}
