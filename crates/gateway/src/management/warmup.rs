use crate::{
    management::settings::{WarmupAccountPolicy, WarmupProviderPolicy},
    state::AppState,
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, put},
    Json, Router,
};
use std::sync::Arc;

pub fn warmup_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/warmup/settings", get(settings))
        .route("/warmup/status", get(status))
        .route("/warmup/settings/provider/{provider}", put(provider))
        .route("/warmup/settings/account/{id}", put(account))
}
async fn settings(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let settings = state.settings.current().warmup.clone();
    Json(settings)
}
async fn status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(crate::warmup::status(&state))
}
async fn provider(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(policy): Json<WarmupProviderPolicy>,
) -> Response {
    if !state
        .pool
        .load()
        .registry
        .models()
        .values()
        .any(|m| m.bindings.keys().any(|p| p.as_str() == name))
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let response = policy.clone();
    match tokio::task::spawn_blocking(move || {
        state.settings.mutate(|s| {
            s.warmup.providers.insert(name, policy);
        })
    })
    .await
    {
        Ok(Ok(_)) => Json(response).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
async fn account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(policy): Json<WarmupAccountPolicy>,
) -> Response {
    if state.find_member(&id).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let response = policy.clone();
    match tokio::task::spawn_blocking(move || {
        state.settings.mutate(|s| {
            s.warmup.accounts.insert(id, policy);
        })
    })
    .await
    {
        Ok(Ok(_)) => Json(response).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
