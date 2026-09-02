use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::scheduler::SchedulerSettings;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
struct SettingsPatch {
    enabled: Option<bool>,
    priorities: Option<std::collections::BTreeMap<String, u32>>,
}

#[derive(Debug, Deserialize)]
struct OrderPatch {
    order: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ReservationRequest {
    instance_id: String,
    account_id: String,
}

async fn list_reservations(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({ "reservations": state.scheduler.reservations() }))
}

async fn reserve_account(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ReservationRequest>,
) -> Response {
    if state.find_member(&request.account_id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "account not found" })),
        )
            .into_response();
    }
    match state
        .scheduler
        .reserve(&request.instance_id, &request.account_id)
    {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "reserved" }))).into_response(),
        Err(error) => (StatusCode::CONFLICT, Json(json!({ "error": error }))).into_response(),
    }
}

async fn release_account(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(instance_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    Json(json!({ "released": state.scheduler.release(&instance_id) }))
}

fn save_error(error: impl std::fmt::Display) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": format!("failed to save scheduler sidecar: {error}") })),
    )
        .into_response()
}

async fn get_settings(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json((*state.scheduler.settings()).clone())
}

async fn put_settings(
    State(state): State<Arc<AppState>>,
    Json(patch): Json<SettingsPatch>,
) -> Response {
    let mut next: SchedulerSettings = (*state.scheduler.settings()).clone();
    if let Some(enabled) = patch.enabled {
        next.enabled = enabled;
    }
    if let Some(priorities) = patch.priorities {
        next.priorities = priorities;
    }
    match state
        .scheduler
        .update_settings(next, &state.pool.load().members)
    {
        Ok(snapshot) => (
            StatusCode::OK,
            Json(json!({ "status": "ok", "scheduler": &*snapshot })),
        )
            .into_response(),
        Err(error) => save_error(error),
    }
}

async fn get_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    state.scheduler.reconcile(&state.pool.load().members);
    Json((*state.scheduler.snapshot()).clone())
}

async fn get_order(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    state.scheduler.reconcile(&state.pool.load().members);
    Json(json!({ "order": state.scheduler.snapshot().order }))
}

async fn put_order(State(state): State<Arc<AppState>>, Json(patch): Json<OrderPatch>) -> Response {
    match state
        .scheduler
        .set_order(&patch.order, &state.pool.load().members)
    {
        Ok(snapshot) => (
            StatusCode::OK,
            Json(json!({ "status": "ok", "order": snapshot.order })),
        )
            .into_response(),
        Err(error) => save_error(error),
    }
}

pub fn scheduler_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/scheduler/settings", get(get_settings).put(put_settings))
        .route("/scheduler/status", get(get_status))
        .route("/scheduler/order", get(get_order).put(put_order))
        .route(
            "/scheduler/reservations",
            get(list_reservations).post(reserve_account),
        )
        .route(
            "/scheduler/reservations/{instance_id}",
            delete(release_account),
        )
}
