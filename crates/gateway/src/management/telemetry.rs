use crate::state::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::sync::Arc;

pub(crate) async fn get_logs(State(state): State<Arc<AppState>>) -> Response {
    crate::management::observability::get_logs(
        State(state),
        Query(crate::management::observability::LogsQuery::default()),
    )
    .await
}

pub(crate) async fn delete_logs(State(state): State<Arc<AppState>>) -> Response {
    crate::management::observability::delete_logs(State(state)).await
}

pub(crate) async fn request_errors(State(state): State<Arc<AppState>>) -> Response {
    crate::management::observability::request_error_logs(State(state)).await
}

pub(crate) async fn download_request_error(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Response {
    crate::management::observability::request_error_log_by_name(State(state), Path(name)).await
}

pub(crate) async fn request_log(
    Path(id): Path<String>,
    State(_state): State<Arc<AppState>>,
) -> Response {
    crate::management::observability::request_log_by_id(Path(id)).await
}

pub(crate) async fn api_key_usage(State(state): State<Arc<AppState>>) -> Response {
    (
        StatusCode::OK,
        Json(json!({ "api-key-usage": state.get_stats() })),
    )
        .into_response()
}

pub(crate) async fn usage_queue(
    State(_state): State<Arc<AppState>>,
    Query(_query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    (StatusCode::OK, Json(json!([]))).into_response()
}

#[cfg(test)]
pub(crate) fn not_found_payload() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"error":"not_found"}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_not_found_is_deterministic() {
        let resp = not_found_payload();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
