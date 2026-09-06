use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::management::settings::ScopedApiKey;
use crate::request_history::stable_key_identifier;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct CreateScopedKeyRequest {
    pub name: String,
    #[serde(default)]
    pub allowed_providers: Vec<String>,
    #[serde(default)]
    pub allowed_accounts: Vec<String>,
    #[serde(default)]
    pub allowed_models: Vec<String>,
    #[serde(default)]
    pub token_limit: u64,
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct PatchScopedKeyRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub raw_key: Option<String>,
    #[serde(default)]
    pub allowed_providers: Option<Vec<String>>,
    #[serde(default)]
    pub allowed_accounts: Option<Vec<String>>,
    #[serde(default)]
    pub allowed_models: Option<Vec<String>>,
    #[serde(default)]
    pub token_limit: Option<u64>,
    #[serde(default)]
    pub is_active: Option<bool>,
    #[serde(default)]
    pub expires_at_ms: Option<Option<i64>>,
}

#[derive(Debug, Serialize)]
pub struct ScopedKeyView {
    pub id: String,
    pub name: String,
    pub key_prefix: String,
    pub key_identifier: String,
    pub allowed_providers: Vec<String>,
    pub allowed_accounts: Vec<String>,
    pub allowed_models: Vec<String>,
    pub token_limit: u64,
    pub token_used: u64,
    pub is_active: bool,
    pub is_exhausted: bool,
    pub created_at_ms: i64,
    pub expires_at_ms: Option<i64>,
}

impl ScopedKeyView {
    fn from_key(key: &ScopedApiKey, live_used: Option<u64>) -> Self {
        let token_used = live_used.unwrap_or(key.token_used);
        let is_exhausted = key.token_limit > 0 && token_used >= key.token_limit;
        Self {
            id: key.id.clone(),
            name: key.name.clone(),
            key_prefix: key.key_prefix.clone(),
            key_identifier: key.key_identifier.clone(),
            allowed_providers: key.allowed_providers.clone(),
            allowed_accounts: key.allowed_accounts.clone(),
            allowed_models: key.allowed_models.clone(),
            token_limit: key.token_limit,
            token_used,
            is_active: key.is_active,
            is_exhausted,
            created_at_ms: key.created_at_ms,
            expires_at_ms: key.expires_at_ms,
        }
    }
}

async fn list_scoped_keys(State(state): State<Arc<AppState>>) -> Response {
    let settings = state.settings.current();
    let keys = &settings.scoped_api_keys;
    let list: Vec<ScopedKeyView> = keys
        .iter()
        .map(|k| {
            let live = state
                .scoped_keys
                .get(&k.key_identifier)
                .map(|e| e.token_used());
            ScopedKeyView::from_key(k, live)
        })
        .collect();
    (StatusCode::OK, Json(json!({ "scoped_keys": list }))).into_response()
}

async fn create_scoped_key(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CreateScopedKeyRequest>,
) -> Response {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let id = format!("shk_{}", uuid::Uuid::new_v4().simple());
    let raw_secret = format!("mq-sh-{}", uuid::Uuid::new_v4().simple());
    let key_identifier = stable_key_identifier(&raw_secret);
    let key_prefix = format!("{}...", key_prefix_source(&raw_secret));

    let new_key = ScopedApiKey {
        id: id.clone(),
        name: payload.name.trim().to_string(),
        key_identifier: key_identifier.clone(),
        key_prefix: key_prefix.clone(),
        raw_key: Some(raw_secret.clone()),
        allowed_providers: payload.allowed_providers,
        allowed_accounts: payload.allowed_accounts,
        allowed_models: payload.allowed_models,
        token_limit: payload.token_limit,
        token_used: 0,
        is_active: true,
        created_at_ms: now_ms,
        expires_at_ms: payload.expires_at_ms,
    };

    // The scoped-key write persists YAML under the mutate lock; keep it off
    // the executor threads.
    let settings = Arc::clone(&state.settings);
    let stored = new_key.clone();
    let result = match tokio::task::spawn_blocking(move || {
        settings.mutate(|s| {
            s.scoped_api_keys.push(stored.clone());
        })
    })
    .await
    {
        Ok(result) => result,
        Err(err) => {
            tracing::error!("scoped key write task failed: {err}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "scoped key write task failed" })),
            )
                .into_response();
        }
    };

    if let Err(err) = result {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to save scoped key: {err}") })),
        )
            .into_response();
    }

    let view = ScopedKeyView::from_key(&new_key, Some(0));
    (
        StatusCode::CREATED,
        Json(json!({
            "api_key": raw_secret,
            "key": view,
        })),
    )
        .into_response()
}

async fn patch_scoped_key(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<PatchScopedKeyRequest>,
) -> Response {
    let settings = Arc::clone(&state.settings);
    let tracker = Arc::clone(&state.scoped_keys);
    let task = tokio::task::spawn_blocking(move || {
      let mut updated_view: Option<ScopedKeyView> = None;
      let result = settings.mutate(|s| {
        if let Some(key) = s.scoped_api_keys.iter_mut().find(|k| k.id == id) {
            if let Some(entry) = tracker.get(&key.key_identifier) {
                key.token_used = key.token_used.max(entry.token_used());
            }
            if let Some(name) = payload.name {
                key.name = name.trim().to_string();
            }
            if let Some(raw) = payload.raw_key {
                let trimmed = raw.trim().to_string();
                if !trimmed.is_empty() {
                    key.key_identifier = stable_key_identifier(&trimmed);
                    key.key_prefix = format!("{}...", key_prefix_source(&trimmed));
                    key.raw_key = Some(trimmed);
                }
            }
            if let Some(providers) = payload.allowed_providers {
                key.allowed_providers = providers;
            }
            if let Some(accounts) = payload.allowed_accounts {
                key.allowed_accounts = accounts;
            }
            if let Some(models) = payload.allowed_models {
                key.allowed_models = models;
            }
            if let Some(limit) = payload.token_limit {
                key.token_limit = limit;
            }
            if let Some(active) = payload.is_active {
                key.is_active = active;
            }
            if let Some(expires) = payload.expires_at_ms {
                key.expires_at_ms = expires;
            }
            updated_view = Some(ScopedKeyView::from_key(key, None));
        }
    });

      (result, updated_view)
    }).await;
    let (result, updated_view) = match task {
        Ok(result) => result,
        Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("scoped key write task failed: {err}")}))).into_response(),
    };
    match result {
        Ok(_) => match updated_view {
            Some(mut view) => {
                if let Some(entry) = state.scoped_keys.get(&view.key_identifier) {
                    view.token_used = entry.token_used();
                    view.is_exhausted = view.token_limit > 0 && view.token_used >= view.token_limit;
                }
                (StatusCode::OK, Json(json!({ "key": view }))).into_response()
            }
            None => (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "scoped key not found" })),
            )
                .into_response(),
        },
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to update scoped key: {err}") })),
        )
            .into_response(),
    }
}

async fn delete_scoped_key(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let settings = Arc::clone(&state.settings);
    let task = tokio::task::spawn_blocking(move || {
      let mut found = false;
      let result = settings.mutate(|s| {
        let initial_len = s.scoped_api_keys.len();
        s.scoped_api_keys.retain(|k| k.id != id);
        found = s.scoped_api_keys.len() < initial_len;
    });

      (result, found)
    }).await;
    let (result, found) = match task {
        Ok(result) => result,
        Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("scoped key write task failed: {err}")}))).into_response(),
    };
    match result {
        Ok(_) => {
            if found {
                (StatusCode::OK, Json(json!({ "status": "deleted" }))).into_response()
            } else {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "scoped key not found" })),
                )
                    .into_response()
            }
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to delete scoped key: {err}") })),
        )
            .into_response(),
    }
}

/// Leading display slice of a secret, cut on a UTF-8 character boundary.
/// Slicing raw bytes panics when a multi-byte character straddles the cut.
fn key_prefix_source(secret: &str) -> &str {
    match secret.char_indices().nth(10) {
        Some((boundary, _)) => &secret[..boundary],
        None => secret,
    }
}

pub fn scoped_keys_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/scoped-keys",
            get(list_scoped_keys).post(create_scoped_key),
        )
        .route(
            "/scoped-keys/{id}",
            axum::routing::patch(patch_scoped_key).delete(delete_scoped_key),
        )
}
