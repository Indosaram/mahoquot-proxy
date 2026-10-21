//! The `/v0/management` surface.
//!
//! LANE SEAM: each route group lives in its own module and exposes
//! `pub fn <group>_routes() -> Router<Arc<AppState>>`. `management_router`
//! merges them and applies the availability + authentication gate exactly
//! once, so a group module never repeats the auth wiring and cannot be mounted
//! without it.

pub mod accounts;
pub mod apikeys;
pub mod core;
pub mod creds;
pub mod gate;
pub mod history;
pub mod lists;
pub mod oauth;
pub mod observability;
pub mod omo_profile;
pub mod plugins;
pub mod quotas;
pub mod registry;
pub mod scalar_table;
pub mod scalars;
pub mod scheduler;
pub mod scoped_keys;
pub mod settings;
pub mod store;
pub mod warmup;

use std::sync::Arc;

use axum::Router;

use crate::inbound::require_api_key;
use crate::state::AppState;

pub fn management_router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .merge(accounts::account_management_routes())
        .merge(apikeys::apikeys_routes())
        .merge(creds::creds_routes())
        .merge(history::history_routes())
        .merge(observability::observability_routes())
        .merge(plugins::plugins_routes())
        .merge(omo_profile::omo_profile_routes())
        .merge(registry::registry_routes())
        .merge(core::core_routes())
        .merge(scoped_keys::scoped_keys_routes())
        .merge(scalars::scalars_routes())
        .merge(scheduler::scheduler_routes())
        .merge(warmup::warmup_routes())
        // Task 33: the configured store's health is reported on v0 too.
        .route(
            "/storage/status",
            axum::routing::get(crate::management::store::v8::storage_status_v8),
        )
        // The gate reads the live settings document so a configured
        // `remote-management.secret-key` (and runtime key edits) take effect;
        // without it the management secret would always read as unset (404).
        .layer(axum::middleware::from_fn_with_state(
            // Cloning the state's keys keeps one shared management-attempt
            // (ban) ledger across both management mounts.
            Arc::new(crate::inbound::ApiKeys::with_live_settings(
                Arc::clone(&state.settings),
                (*state.api_keys).clone(),
            )),
            require_api_key,
        ))
        .layer(axum::middleware::from_fn(gate::stamp_management_response))
        // An unimplemented management path must answer 404 like upstream's
        // NoRoute. Without this fallback the request escapes the nest and hits
        // the relay's inbound-key layer, which answers 401 with the wrong
        // error body entirely. The fallback sits outside the gate because
        // upstream's group middleware never runs for an unmatched route.
        .fallback(|| async { axum::http::StatusCode::NOT_FOUND })
}
