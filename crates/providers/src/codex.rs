//! Codex provider adapter.
//!
//! Provides account loading, header generation, and typed model registry contributions.

pub use crate::account::*;
use mahoquot_registry::{embedded_snapshot, ProviderContribution, ProviderId, RegistrySnapshot};

pub fn provider_id() -> ProviderId {
    ProviderId::codex()
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

pub fn is_codex_model_in_snapshot(snapshot: &RegistrySnapshot, model: &str) -> bool {
    contribution(snapshot).supports_model(model)
}

pub fn is_codex_model(model: &str) -> bool {
    is_codex_model_in_snapshot(embedded_snapshot(), model)
}
