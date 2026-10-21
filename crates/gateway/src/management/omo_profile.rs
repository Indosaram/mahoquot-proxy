//! `/v0/management/omo-profile`: the shared omo client profile.
//!
//! The profile is an opaque client document (`compat`, `models`, `remove`)
//! that the gateway stores in `config.yaml` beside every other setting. The
//! only thing it interprets is each model's `id`, to keep the list well formed.
//! Writes are optimistic: `ETag: "r<revision>"` plus a mandatory `If-Match`
//! make a PUT a compare-and-set, so two clients cannot silently overwrite each
//! other.

use std::collections::HashSet;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::settings::OmoProfile;
use crate::state::AppState;

const MAX_PROFILE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileBody {
    compat: Map<String, Value>,
    models: Vec<Map<String, Value>>,
    remove: Vec<String>,
}

fn etag_for(revision: u64) -> String {
    format!("\"r{revision}\"")
}

fn profile_body(profile: &OmoProfile) -> Value {
    json!({
        "revision": profile.revision,
        "updated_at": profile.updated_at,
        "compat": profile.compat,
        "models": profile.models,
        "remove": profile.remove,
    })
}

fn with_etag(status: StatusCode, profile: &OmoProfile) -> Response {
    let mut response = (status, Json(profile_body(profile))).into_response();
    set_etag(&mut response, profile.revision);
    response
}

fn set_etag(response: &mut Response, revision: u64) {
    if let Ok(value) = HeaderValue::from_str(&etag_for(revision)) {
        response.headers_mut().insert(header::ETAG, value);
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn validate(body: &ProfileBody) -> Result<(), String> {
    let mut ids = HashSet::new();
    for (index, model) in body.models.iter().enumerate() {
        let id = match model.get("id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => id,
            _ => return Err(format!("models[{index}] needs a non-empty string id")),
        };
        if !ids.insert(id) {
            return Err(format!("duplicate model id {id:?}"));
        }
    }
    let mut removed = HashSet::new();
    for entry in &body.remove {
        if entry.is_empty() {
            return Err("remove entries must be non-empty".to_string());
        }
        if !removed.insert(entry.as_str()) {
            return Err(format!("duplicate remove entry {entry:?}"));
        }
        if ids.contains(entry.as_str()) {
            return Err(format!("id {entry:?} is in both models and remove"));
        }
    }
    Ok(())
}

async fn get_profile(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let profile = state
        .settings
        .current()
        .omo_profile
        .clone()
        .unwrap_or_default();
    let etag = etag_for(profile.revision);
    let matches = header_str(&headers, header::IF_NONE_MATCH)
        .is_some_and(|value| value.split(',').any(|candidate| candidate.trim() == etag));
    if matches {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        set_etag(&mut response, profile.revision);
        return response;
    }
    with_etag(StatusCode::OK, &profile)
}

async fn put_profile(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(if_match) = header_str(&headers, header::IF_MATCH).map(str::to_string) else {
        return error(
            StatusCode::PRECONDITION_REQUIRED,
            "If-Match is required; send the ETag from GET /omo-profile",
        );
    };
    let parsed: ProfileBody = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(err) => return error(StatusCode::BAD_REQUEST, format!("invalid body: {err}")),
    };
    if let Err(message) = validate(&parsed) {
        return error(StatusCode::BAD_REQUEST, message);
    }

    let outcome = tokio::task::spawn_blocking(move || {
        state.settings.try_mutate(|settings| {
            let current = settings.omo_profile.clone().unwrap_or_default();
            if etag_for(current.revision) != if_match {
                return Err(current);
            }
            let updated_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or(0);
            settings.omo_profile = Some(OmoProfile {
                revision: current.revision + 1,
                updated_at,
                compat: parsed.compat,
                models: parsed.models,
                remove: parsed.remove,
            });
            Ok(())
        })
    })
    .await;

    match outcome {
        Ok(Err(current)) => with_etag(StatusCode::PRECONDITION_FAILED, &current),
        Ok(Ok(Ok(settings))) => {
            let profile = settings.omo_profile.clone().unwrap_or_default();
            with_etag(StatusCode::OK, &profile)
        }
        Ok(Ok(Err(err))) => error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

pub fn omo_profile_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/omo-profile", get(get_profile).put(put_profile))
        .layer(DefaultBodyLimit::max(MAX_PROFILE_BYTES))
}
