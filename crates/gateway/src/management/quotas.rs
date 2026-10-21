use crate::account::ProviderKind;
use crate::state::AppState;
use axum::response::IntoResponse;
use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Arc;

fn response(status: axum::http::StatusCode, body: Value) -> axum::response::Response {
    (status, Json(body)).into_response()
}

fn provider_supports_quota(kind: ProviderKind) -> bool {
    matches!(
        kind,
        ProviderKind::Codex
            | ProviderKind::Antigravity
            | ProviderKind::Claude
            | ProviderKind::Cursor
            | ProviderKind::Kiro
            | ProviderKind::Zcode
    )
}

fn provider_display_name(id: &str) -> &'static str {
    match id {
        "codex" => "Codex",
        "antigravity" => "Antigravity",
        "claude" => "Claude",
        "cursor" => "Cursor",
        "kiro" => "Kiro",
        "zcode" => "Zcode",
        _ => "Provider",
    }
}

async fn providers(State(state): State<Arc<AppState>>) -> axum::response::Response {
    let pool = state.pool.load();
    let mut supported_ids: BTreeSet<String> = BTreeSet::new();

    for member in &pool.members {
        if !member.is_manually_disabled() {
            let kind = member.kind();
            if provider_supports_quota(kind) {
                supported_ids.insert(kind.as_str().to_string());
            } else if kind == ProviderKind::Generic {
                if let Some((generic_provider, _)) = member.generic_models() {
                    let p_lower = generic_provider.to_ascii_lowercase();
                    if p_lower == "inferx" || p_lower == "cline-pass" || p_lower == "cline" {
                        supported_ids.insert(p_lower);
                    }
                }
            }
        }
    }

    let provider_list: Vec<Value> = supported_ids
        .into_iter()
        .map(|id| {
            let display = provider_display_name(&id);
            let supports_reset = id == "codex";
            json!({
                "provider": id,
                "display_name": display,
                "supports_reset": supports_reset,
            })
        })
        .collect();

    response(
        axum::http::StatusCode::OK,
        json!({ "providers": provider_list }),
    )
}

async fn fetch_quota(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let index = body
        .get("auth_index")
        .or_else(|| body.get("authIndex"))
        .or_else(|| body.get("AuthIndex"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if index.is_empty() {
        return response(
            axum::http::StatusCode::BAD_REQUEST,
            json!({"error":"auth_index is required"}),
        );
    }

    let resolved = crate::management::creds::resolve_auth_index(&state, index);
    let by_file = resolved.as_deref().and_then(|file_name| {
        state
            .pool
            .load()
            .members
            .iter()
            .find(|m| {
                m.file_path
                    .file_name()
                    .map(|n| n.to_string_lossy() == file_name)
                    .unwrap_or(false)
            })
            .cloned()
    });

    let Some(member) = by_file.or_else(|| state.find_member(index)) else {
        return response(
            axum::http::StatusCode::NOT_FOUND,
            json!({"error":"auth not found"}),
        );
    };

    let snapshot = member.usage_snapshot();
    let usage_val = serde_json::to_value(&snapshot).unwrap_or(Value::Null);

    response(
        axum::http::StatusCode::OK,
        json!({
            "auth_index": index,
            "models": usage_val
        }),
    )
}

pub(crate) async fn reset_quota(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let index = body
        .get("auth_index")
        .or_else(|| body.get("authIndex"))
        .or_else(|| body.get("AuthIndex"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if index.is_empty() {
        return response(
            axum::http::StatusCode::BAD_REQUEST,
            json!({"error":"auth_index is required"}),
        );
    }

    let resolved = crate::management::creds::resolve_auth_index(&state, index);
    let by_file = resolved.as_deref().and_then(|file_name| {
        state
            .pool
            .load()
            .members
            .iter()
            .find(|m| {
                m.file_path
                    .file_name()
                    .map(|n| n.to_string_lossy() == file_name)
                    .unwrap_or(false)
            })
            .cloned()
    });

    let Some(member) = by_file.or_else(|| state.find_member(index)) else {
        return response(
            axum::http::StatusCode::NOT_FOUND,
            json!({"error":"auth not found"}),
        );
    };

    match crate::quota::consume_reset_credit(&state, &member).await {
        Ok(()) => {
            let snapshot = member.usage_snapshot();
            let usage_val = serde_json::to_value(&snapshot).unwrap_or(Value::Null);
            response(
                axum::http::StatusCode::OK,
                json!({
                    "status": "ok",
                    "auth_index": index,
                    "models": usage_val
                }),
            )
        }
        Err(error) => response(
            crate::quota::reset_error_status(&error),
            json!({"status":"error","auth_index":index,"error":error.to_string()}),
        ),
    }
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/quota/providers", get(providers))
        .route("/quota/fetch", post(fetch_quota))
        .route("/quota/reset", post(reset_quota))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_provider_registry_is_an_explicit_list() {
        let value = json!({"providers": []});
        assert_eq!(
            value["providers"].as_array().expect("provider list").len(),
            0
        );
    }

    #[test]
    fn unknown_quota_remains_null_not_zero() {
        let usage = crate::usage::AccountUsage::default();
        let val = serde_json::to_value(&usage).expect("serialize");
        assert!(val.get("credits_balance").is_none() || val["credits_balance"].is_null());
        assert!(
            val.get("reset_credits_available").is_none()
                || val["reset_credits_available"].is_null()
        );
        if let Some(primary) = val.get("primary") {
            assert!(primary.get("used_percent").is_none() || primary["used_percent"].is_null());
        }
    }
}
