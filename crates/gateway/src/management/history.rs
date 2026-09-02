use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::request_history::{
    GroupDimension, HistoryError, HistoryQuery, ModelPrice, OutcomeFilter, TimeBucket,
};
use crate::state::AppState;

fn error_response(status: StatusCode, code: &str, message: impl ToString) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "code": code,
                "message": message.to_string(),
                "retryable": status == StatusCode::SERVICE_UNAVAILABLE
            }
        })),
    )
        .into_response()
}

fn history_error(error: HistoryError) -> Response {
    match error {
        HistoryError::InvalidTimeRange { .. }
        | HistoryError::InvalidTimeBucket(_)
        | HistoryError::InvalidEvent(_)
        | HistoryError::InvalidPrice(_)
        | HistoryError::ValueOutOfRange(_) => {
            error_response(StatusCode::BAD_REQUEST, "history_query_invalid", error)
        }
        HistoryError::Database(_) | HistoryError::WorkerUnavailable => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "history_unavailable",
            error,
        ),
    }
}

type ResponseResult<T> = Result<T, Box<Response>>;

fn boxed_error_response(status: StatusCode, code: &str, message: impl ToString) -> Box<Response> {
    Box::new(error_response(status, code, message))
}

fn list(params: &HashMap<String, String>, name: &str) -> Vec<String> {
    params
        .get(name)
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_i64(params: &HashMap<String, String>, name: &str) -> ResponseResult<Option<i64>> {
    params
        .get(name)
        .map(|value| {
            value.parse::<i64>().map_err(|_| {
                boxed_error_response(
                    StatusCode::BAD_REQUEST,
                    "history_query_invalid",
                    format!("{name} must be an integer"),
                )
            })
        })
        .transpose()
}

fn parse_query(params: &HashMap<String, String>) -> ResponseResult<HistoryQuery> {
    let start_ms = parse_i64(params, "start-ms")?;
    let end_ms = parse_i64(params, "end-ms")?;
    if let (Some(start_ms), Some(end_ms)) = (start_ms, end_ms) {
        if start_ms >= end_ms {
            return Err(boxed_error_response(
                StatusCode::BAD_REQUEST,
                "history_range_invalid",
                format!("start-ms {start_ms} must be before end-ms {end_ms}"),
            ));
        }
    }
    let status_codes = list(params, "status")
        .into_iter()
        .map(|value| {
            value.parse::<u16>().map_err(|_| {
                boxed_error_response(
                    StatusCode::BAD_REQUEST,
                    "history_query_invalid",
                    "status must contain HTTP status codes",
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let outcomes = list(params, "outcome")
        .into_iter()
        .map(|value| match value.as_str() {
            "succeeded" | "success" => Ok(OutcomeFilter::Succeeded),
            "failed" | "failure" => Ok(OutcomeFilter::Failed),
            _ => Err(boxed_error_response(
                StatusCode::BAD_REQUEST,
                "history_query_invalid",
                "outcome must be succeeded or failed",
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let time_bucket = params
        .get("time-bucket")
        .map(|value| TimeBucket::parse(value).map_err(|error| Box::new(history_error(error))))
        .transpose()?;
    let group_by = list(params, "group-by")
        .into_iter()
        .map(|value| match value.as_str() {
            "account" => Ok(GroupDimension::Account),
            "provider" => Ok(GroupDimension::Provider),
            "model" => Ok(GroupDimension::Model),
            "key" | "key-label" => Ok(GroupDimension::Key),
            "status" => Ok(GroupDimension::Status),
            _ => Err(boxed_error_response(
                StatusCode::BAD_REQUEST,
                "history_query_invalid",
                "group-by contains an unsupported dimension",
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(HistoryQuery {
        start_ms,
        end_ms,
        accounts: list(params, "account"),
        providers: list(params, "provider"),
        models: list(params, "model"),
        key_identifiers: list(params, "key-label"),
        status_codes,
        outcomes,
        search: params
            .get("text")
            .or_else(|| params.get("search"))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        time_bucket,
        group_by,
    })
}

fn totals_json(totals: &crate::request_history::HistoryTotals) -> Value {
    json!({
        "requests": totals.requests,
        "successful-requests": totals.successful_requests,
        "failed-requests": totals.failed_requests,
        "input-tokens": totals.input_tokens,
        "output-tokens": totals.output_tokens,
        "cached-input-tokens": totals.cached_input_tokens,
        "reasoning-tokens": totals.reasoning_tokens,
        "total-tokens": totals.total_tokens,
        "total-latency-ms": totals.total_latency_ms,
        "average-latency-ms": totals.average_latency_ms,
        "estimated-cost-usd": totals.estimated_cost_usd
    })
}

fn event_json(event: &crate::request_history::HistoryEventRow) -> Value {
    json!({
        "event-id": event.event_id,
        "occurred-at-ms": event.occurred_at_ms,
        "account": event.account_identifier,
        "provider": event.provider,
        "model": event.model,
        "key-label": event.key_identifier,
        "status": event.status_code,
        "succeeded": event.succeeded,
        "input-tokens": event.input_tokens,
        "output-tokens": event.output_tokens,
        "cached-input-tokens": event.cached_input_tokens,
        "reasoning-tokens": event.reasoning_tokens,
        "total-tokens": event.total_tokens,
        "latency-ms": event.latency_ms,
        "estimated-cost-usd": event.estimated_cost_usd,
        "price-version": event.price_version
    })
}

async fn stats(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let query = match parse_query(&params) {
        Ok(query) => query,
        Err(response) => return *response,
    };
    if let Err(error) = state.history.flush() {
        return history_error(error);
    }
    match state.history.store().and_then(|store| store.query(&query)) {
        Ok(result) => Json(json!({
            "totals": totals_json(&result.totals),
            "groups": result.groups.into_iter().map(|group| json!({
                "bucket-start-ms": group.key.bucket_start_ms,
                "account": group.key.account,
                "provider": group.key.provider,
                "model": group.key.model,
                "key-label": group.key.key_identifier,
                "status": group.key.status_code,
                "totals": totals_json(&group.totals)
            })).collect::<Vec<_>>()
        }))
        .into_response(),
        Err(error) => history_error(error),
    }
}

async fn count(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let query = match parse_query(&params) {
        Ok(query) => query,
        Err(response) => return *response,
    };
    if let Err(error) = state.history.flush() {
        return history_error(error);
    }
    match state.history.store().and_then(|store| store.count(&query)) {
        Ok(count) => Json(json!({ "count": count })).into_response(),
        Err(error) => history_error(error),
    }
}

async fn events(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let query = match parse_query(&params) {
        Ok(query) => query,
        Err(response) => return *response,
    };
    let limit = match params.get("limit") {
        Some(value) => match value.parse::<usize>() {
            Ok(value @ 1..=1000) => value,
            _ => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "history_query_invalid",
                    "limit must be between 1 and 1000",
                )
            }
        },
        None => 100,
    };
    let cursor = match parse_i64(&params, "cursor") {
        Ok(cursor) => cursor,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "history_cursor_invalid",
                "cursor must be an integer",
            )
        }
    };
    if let Err(error) = state.history.flush() {
        return history_error(error);
    }
    match state
        .history
        .store()
        .and_then(|store| store.page(&query, cursor, limit))
    {
        Ok(page) => match state.history.store().and_then(|store| store.query(&query)) {
            Ok(result) => Json(json!({
                "events": page.events.iter().map(event_json).collect::<Vec<_>>(),
                "next-cursor": page.next_cursor,
                "totals": totals_json(&result.totals)
            }))
            .into_response(),
            Err(error) => history_error(error),
        },
        Err(error) => history_error(error),
    }
}

async fn clear(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let confirmed = matches!(
        params.get("confirm").map(String::as_str),
        Some("true" | "delete-history")
    );
    if !confirmed {
        return error_response(
            StatusCode::BAD_REQUEST,
            "history_clear_confirmation_required",
            "confirm=true is required to clear request history",
        );
    }
    let query = match parse_query(&params) {
        Ok(query) => query,
        Err(response) => return *response,
    };
    if let Err(error) = state.history.flush() {
        return history_error(error);
    }
    let mut telemetry_query = query.clone();
    telemetry_query.time_bucket = Some(TimeBucket::Minute);
    telemetry_query.group_by = vec![GroupDimension::Provider, GroupDimension::Account];
    let dashboard_groups = match state
        .history
        .store()
        .and_then(|store| store.query(&telemetry_query))
    {
        Ok(result) => result.groups,
        Err(error) => return history_error(error),
    };
    match state.history.store().and_then(|store| store.clear(&query)) {
        Ok(deleted) => {
            let dashboard_buckets = state.telemetry.remove_history_groups(&dashboard_groups);
            let _ = state.telemetry.flush();
            Json(json!({
                "deleted": deleted,
                "dashboard-history-removed": true,
                "dashboard-buckets-removed": dashboard_buckets,
                "proxy-file-logs-removed": false
            }))
            .into_response()
        }
        Err(error) => history_error(error),
    }
}

async fn clear_compat(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if !matches!(
        params.get("confirm").map(String::as_str),
        Some("true" | "delete-history")
    ) {
        return error_response(
            StatusCode::CONFLICT,
            "history_clear_confirmation_required",
            "confirm=delete-history is required to clear request history",
        );
    }
    clear(State(state), Query(params)).await
}

async fn detail(State(state): State<Arc<AppState>>, Path(event_id): Path<String>) -> Response {
    if let Err(error) = state.history.flush() {
        return history_error(error);
    }
    match state
        .history
        .store()
        .and_then(|store| store.detail(&event_id))
    {
        Ok(Some(event)) => Json(json!({ "event": event_json(&event) })).into_response(),
        Ok(None) => error_response(
            StatusCode::NOT_FOUND,
            "history_event_not_found",
            "request history event not found",
        ),
        Err(error) => history_error(error),
    }
}

async fn health(State(state): State<Arc<AppState>>) -> Response {
    let health = state.history.health();
    Json(json!({
        "ready": health.ready,
        "degraded": health.degraded,
        "queue-capacity": health.queue_capacity,
        "queue-depth": health.queue_depth,
        "enqueued-events": health.enqueued_events,
        "written-events": health.written_events,
        "dropped-events": health.dropped_events,
        "database-failures": health.database_failures,
        "last-error": health.last_error
    }))
    .into_response()
}

fn csv_field(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

async fn export(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let export_guard_configured = !state
        .settings
        .current()
        .remote_management
        .secret_key
        .is_empty();
    if export_guard_configured && !super::accounts::export_authorized(&state, &headers) {
        return super::accounts::export_refusal();
    }
    let query = match parse_query(&params) {
        Ok(query) => query,
        Err(response) => return *response,
    };
    let format = params.get("format").map(String::as_str).unwrap_or("json");
    if !matches!(format, "json" | "csv") {
        return error_response(
            StatusCode::BAD_REQUEST,
            "history_export_format_invalid",
            "format must be json or csv",
        );
    }
    if let Err(error) = state.history.flush() {
        return history_error(error);
    }
    let events = match state.history.store().and_then(|store| store.export(&query)) {
        Ok(events) => events,
        Err(error) => return history_error(error),
    };
    if format == "json" {
        return Json(json!({
            "count": events.len(),
            "events": events.iter().map(event_json).collect::<Vec<_>>()
        }))
        .into_response();
    }
    let mut body = String::from("event_id,occurred_at_ms,account,provider,model,key_label,status,succeeded,input_tokens,output_tokens,cached_input_tokens,reasoning_tokens,total_tokens,latency_ms,estimated_cost_usd\n");
    for event in events {
        body.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            csv_field(&event.event_id),
            event.occurred_at_ms,
            csv_field(&event.account_identifier),
            csv_field(&event.provider),
            csv_field(&event.model),
            csv_field(event.key_identifier.as_deref().unwrap_or("")),
            event.status_code,
            event.succeeded,
            event.input_tokens,
            event.output_tokens,
            event.cached_input_tokens,
            event.reasoning_tokens,
            event.total_tokens,
            event.latency_ms,
            event.estimated_cost_usd
        ));
    }
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=mahoquot-request-history.csv",
            ),
        ],
        Body::from(body),
    )
        .into_response()
}

fn price_json(price: &ModelPrice) -> Value {
    json!({
        "model": price.model,
        "version": price.version,
        "input-per-million": price.input_per_million,
        "output-per-million": price.output_per_million,
        "cached-input-per-million": price.cached_input_per_million,
        "effective-from-ms": price.effective_from_ms
    })
}

async fn prices(State(state): State<Arc<AppState>>) -> Response {
    match state.history.store().and_then(|store| store.model_prices()) {
        Ok(prices) => Json(json!({
            "prices": prices.iter().map(price_json).collect::<Vec<_>>()
        }))
        .into_response(),
        Err(error) => history_error(error),
    }
}

async fn put_price(
    State(state): State<Arc<AppState>>,
    Path(model): Path<String>,
    raw: bytes::Bytes,
) -> Response {
    let body: Value = match serde_json::from_slice(&raw) {
        Ok(body) => body,
        Err(error) => {
            return error_response(StatusCode::BAD_REQUEST, "history_price_invalid", error)
        }
    };
    let number = |name: &str| body.get(name).and_then(Value::as_f64);
    let price = ModelPrice {
        model,
        version: body
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("default")
            .to_string(),
        input_per_million: number("input-per-million").unwrap_or(-1.0),
        output_per_million: number("output-per-million").unwrap_or(-1.0),
        cached_input_per_million: number("cached-input-per-million").unwrap_or(0.0),
        effective_from_ms: body
            .get("effective-from-ms")
            .and_then(Value::as_i64)
            .unwrap_or(0),
    };
    match state
        .history
        .store()
        .and_then(|store| store.set_model_price(&price))
    {
        Ok(()) => (StatusCode::OK, Json(price_json(&price))).into_response(),
        Err(error) => history_error(error),
    }
}

async fn delete_price(
    State(state): State<Arc<AppState>>,
    Path(model): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    match state.history.store().and_then(|store| {
        store.delete_model_price(&model, params.get("version").map(String::as_str))
    }) {
        Ok(deleted) => Json(json!({ "deleted": deleted })).into_response(),
        Err(error) => history_error(error),
    }
}

pub fn history_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/history", delete(clear_compat))
        .route("/history/stats", get(stats))
        .route("/history/count", get(count))
        .route("/history/events", get(events).delete(clear))
        .route("/history/events/{event_id}", get(detail))
        .route("/history/health", get(health))
        .route("/history/export", get(export).post(export))
        .route("/prices", get(prices))
        .route(
            "/prices/{model}",
            axum::routing::put(put_price).delete(delete_price),
        )
}
