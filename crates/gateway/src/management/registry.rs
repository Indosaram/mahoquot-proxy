use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::registry::{CatalogStatus, RefreshEnqueue};
use crate::state::AppState;

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
struct LastRefreshResponse {
    outcome: &'static str,
    attempted_at: Option<u64>,
    duration_ms: Option<u64>,
    rejection_reason: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
struct ModelRegistryStatusResponse {
    source: String,
    catalog_version: u64,
    generation: u64,
    generated_at: Option<u64>,
    loaded_at: u64,
    stale: bool,
    last_refresh: LastRefreshResponse,
    provider_count: usize,
    model_count: usize,
    refresh_in_flight: bool,
}

#[derive(Serialize)]
struct RefreshResponse {
    accepted: bool,
    coalesced: bool,
    state: ModelRegistryStatusResponse,
}

fn safe_status(state: &AppState) -> ModelRegistryStatusResponse {
    let status: CatalogStatus = state.catalog.status();
    let outcome = match status.last_refresh_at {
        None => "never",
        Some(_) if status.last_refresh_success => "success",
        Some(_) => "error",
    };
    ModelRegistryStatusResponse {
        source: status.active_source.to_string(),
        catalog_version: status.active_version.as_u64(),
        generation: state.runtime.generation(),
        generated_at: status.generated_at,
        loaded_at: status.loaded_at,
        stale: status.stale,
        last_refresh: LastRefreshResponse {
            outcome,
            attempted_at: status.last_refresh_at,
            duration_ms: status.last_refresh_duration_ms,
            rejection_reason: status.last_rejection_reason,
        },
        provider_count: status.provider_count,
        model_count: status.model_count,
        refresh_in_flight: state.catalog.refresh_in_flight(),
    }
}

async fn get_registry(State(state): State<Arc<AppState>>) -> Json<ModelRegistryStatusResponse> {
    Json(safe_status(&state))
}

async fn refresh_registry(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<RefreshResponse>) {
    let enqueue = state.catalog.enqueue_refresh();
    let accepted = enqueue == RefreshEnqueue::Accepted;
    (
        StatusCode::ACCEPTED,
        Json(RefreshResponse {
            accepted,
            coalesced: !accepted,
            state: safe_status(&state),
        }),
    )
}

pub fn registry_routes() -> Router<Arc<AppState>> {
    Router::new().route("/model-registry", get(get_registry).post(refresh_registry))
}
