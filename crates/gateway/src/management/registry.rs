use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::account::AccountMember;
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

#[derive(Deserialize, Default)]
pub struct DevinRefreshQuery {
    pub identity_slug: Option<String>,
    pub identity: Option<String>,
}

#[derive(Serialize)]
pub struct DevinAccountRefreshResult {
    pub identity_slug: String,
    pub status: &'static str,
    pub models: Vec<String>,
    pub stale: bool,
    pub last_refresh_at: Option<u64>,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct DevinRefreshResponse {
    pub status: &'static str,
    pub outcome: &'static str,
    pub models: Vec<String>,
    pub error: Option<String>,
    pub accounts: Vec<DevinAccountRefreshResult>,
    pub generation: u64,
}

async fn refresh_devin_models(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DevinRefreshQuery>,
    body: axum::body::Bytes,
) -> (StatusCode, Json<DevinRefreshResponse>) {
    let mut target_identity = query.identity_slug.or(query.identity);
    if !body.is_empty() {
        match serde_json::from_slice::<serde_json::Value>(&body) {
            Ok(v) => {
                if !v.is_object() {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(DevinRefreshResponse {
                            status: "error",
                            outcome: "error",
                            models: Vec::new(),
                            error: Some("request body must be a JSON object".to_string()),
                            accounts: Vec::new(),
                            generation: state.runtime.generation(),
                        }),
                    );
                }
                if let Some(slug) = v.get("identity_slug").or_else(|| v.get("identity")) {
                    if let Some(s) = slug.as_str() {
                        target_identity = Some(s.to_string());
                    } else if !slug.is_null() {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(DevinRefreshResponse {
                                status: "error",
                                outcome: "error",
                                models: Vec::new(),
                                error: Some("identity must be a string".to_string()),
                                accounts: Vec::new(),
                                generation: state.runtime.generation(),
                            }),
                        );
                    }
                }
            }
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(DevinRefreshResponse {
                        status: "error",
                        outcome: "error",
                        models: Vec::new(),
                        error: Some("invalid JSON body".to_string()),
                        accounts: Vec::new(),
                        generation: state.runtime.generation(),
                    }),
                );
            }
        }
    }

    let pool = state.pool.load();
    let devin_members: Vec<Arc<AccountMember>> = pool
        .members
        .iter()
        .filter(|m| m.kind() == crate::account::ProviderKind::Devin)
        .filter(|m| {
            if let Some(target) = &target_identity {
                m.id == *target
            } else {
                true
            }
        })
        .cloned()
        .collect();

    if let Some(target) = &target_identity {
        if devin_members.is_empty() {
            return (
                StatusCode::NOT_FOUND,
                Json(DevinRefreshResponse {
                    status: "error",
                    outcome: "error",
                    models: Vec::new(),
                    error: Some(format!("Devin account '{target}' not found")),
                    accounts: Vec::new(),
                    generation: state.runtime.generation(),
                }),
            );
        }
    }

    let mut account_results = Vec::new();
    let mut any_fresh_success = false;
    let mut last_error_msg = None;

    for member in &devin_members {
        let client = match state.devin_client_for_member(member) {
            Ok(c) => c,
            Err(err) => {
                let err_str = err.to_string();
                last_error_msg = Some(err_str.clone());
                state.monitor.record_error(&member.id, 502, &format!("model discovery failed: {err_str}"));
                let now_unix = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let key = crate::devin_catalog::DevinCacheKey::new(&member.id, &member.access_token(), member.effective_base_url());
                let init_fail = Arc::new(crate::devin_catalog::DevinAccountCatalogState::initial_failure(
                    key,
                    "failed to build upstream HTTP client",
                    now_unix,
                ));
                let rev = crate::devin_catalog::compute_credential_revision(&member.access_token());
                let _ = state.runtime.publish_devin_member_catalog(&member.id, &rev, init_fail, &state.devin_cache);

                account_results.push(DevinAccountRefreshResult {
                    identity_slug: member.id.clone(),
                    status: "error",
                    models: Vec::new(),
                    stale: true,
                    last_refresh_at: Some(now_unix),
                    error: Some(err_str),
                });
                continue;
            }
        };
        match crate::devin_catalog::refresh_account_models(member, &client, Some(&state.devin_cache)).await {
            Ok(catalog_state) => {
                match state.runtime.publish_devin_member_catalog(
                    &member.id,
                    &catalog_state.key.credential_revision,
                    Arc::clone(&catalog_state),
                    &state.devin_cache,
                ) {
                    Ok(new_comp) => {
                        if !catalog_state.stale {
                            any_fresh_success = true;
                        }
                        let accepted_member = new_comp
                            .members()
                            .iter()
                            .find(|m| m.id == member.id);
                        let public_ids = accepted_member
                            .and_then(|m| m.devin_models())
                            .unwrap_or_else(|| {
                                catalog_state
                                    .models
                                    .iter()
                                    .map(|m| m.public_id.clone())
                                    .collect()
                            });
                        account_results.push(DevinAccountRefreshResult {
                            identity_slug: member.id.clone(),
                            status: if catalog_state.stale { "error" } else { "success" },
                            models: public_ids,
                            stale: catalog_state.stale,
                            last_refresh_at: catalog_state.last_refresh_at,
                            error: catalog_state.last_error.clone(),
                        });
                        if let Some(ref err) = catalog_state.last_error {
                            last_error_msg = Some(err.clone());
                            state.monitor.record_error(&member.id, 502, &format!("model discovery: {err}"));
                        } else {
                            state.monitor.clear_error(&member.id);
                        }
                    }
                    Err(pub_err) => {
                        let err_str = pub_err.safe_message().to_string();
                        last_error_msg = Some(err_str.clone());
                        state.monitor.record_error(&member.id, 502, &format!("model discovery publication failed: {err_str}"));
                        account_results.push(DevinAccountRefreshResult {
                            identity_slug: member.id.clone(),
                            status: "error",
                            models: Vec::new(),
                            stale: true,
                            last_refresh_at: catalog_state.last_refresh_at,
                            error: Some(err_str),
                        });
                    }
                }
            }
            Err(err) => {
                let err_str = err.safe_message().to_string();
                last_error_msg = Some(err_str.clone());
                state.monitor.record_error(&member.id, 502, &format!("model discovery failed: {err_str}"));
                let now_unix = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let key = crate::devin_catalog::DevinCacheKey::new(&member.id, &member.access_token(), member.effective_base_url());
                let init_fail = Arc::new(crate::devin_catalog::DevinAccountCatalogState::initial_failure(
                    key,
                    err.safe_message(),
                    now_unix,
                ));
                let rev = crate::devin_catalog::compute_credential_revision(&member.access_token());
                let _ = state.runtime.publish_devin_member_catalog(&member.id, &rev, init_fail, &state.devin_cache);

                account_results.push(DevinAccountRefreshResult {
                    identity_slug: member.id.clone(),
                    status: "error",
                    models: Vec::new(),
                    stale: true,
                    last_refresh_at: Some(now_unix),
                    error: Some(err_str),
                });
            }
        }
    }

    // Read directly from the accepted candidate snapshot without extra trigger_coalesced_refresh
    let final_snapshot = state.pool.load();
    let generation = final_snapshot.generation();
    let all_models: Vec<String> = final_snapshot
        .models()
        .iter()
        .filter(|m| m.id.starts_with("devin/"))
        .map(|m| m.id.clone())
        .collect();

    let outcome = if devin_members.is_empty() {
        "never"
    } else if any_fresh_success {
        "success"
    } else {
        "error"
    };

    (
        StatusCode::OK,
        Json(DevinRefreshResponse {
            status: "ok",
            outcome,
            models: all_models,
            error: last_error_msg,
            accounts: account_results,
            generation,
        }),
    )
}

async fn get_devin_models_status(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let pool = state.pool.load();
    let devin_members: Vec<&Arc<AccountMember>> = pool
        .members
        .iter()
        .filter(|m| m.kind() == crate::account::ProviderKind::Devin)
        .collect();

    let mut accounts = Vec::new();
    let mut all_models = Vec::new();
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut stale_members_to_refresh = Vec::new();

    for member in devin_members {
        if let Some(cat) = member.devin_catalog_state() {
            let is_stale = cat.is_stale(now_unix);
            if is_stale && !member.is_manually_disabled() {
                stale_members_to_refresh.push(Arc::clone(member));
            }
            let public_ids: Vec<String> = cat.models.iter().map(|m| m.public_id.clone()).collect();
            for id in &public_ids {
                if !all_models.contains(id) {
                    all_models.push(id.clone());
                }
            }
            accounts.push(serde_json::json!({
                "identity_slug": member.id,
                "status": if cat.last_error.is_some() && !cat.has_succeeded {
                    "error"
                } else if is_stale {
                    "stale"
                } else {
                    "active"
                },
                "models": public_ids,
                "stale": is_stale,
                "last_refresh_at": cat.last_refresh_at,
                "error": cat.last_error,
                "disabled": member.is_manually_disabled(),
            }));
        } else {
            if !member.is_manually_disabled() {
                stale_members_to_refresh.push(Arc::clone(member));
            }
            accounts.push(serde_json::json!({
                "identity_slug": member.id,
                "status": "uninitialized",
                "models": [],
                "stale": true,
                "last_refresh_at": null,
                "error": null,
                "disabled": member.is_manually_disabled(),
            }));
        }
    }

    if !stale_members_to_refresh.is_empty() {
        let state_clone = Arc::clone(&state);
        tokio::spawn(async move {
            for member in stale_members_to_refresh {
                if let Ok(client) = state_clone.devin_client_for_member(&member) {
                    if let Ok(cat) = crate::devin_catalog::refresh_account_models(&member, &client, Some(&state_clone.devin_cache)).await {
                        let rev = cat.key.credential_revision.clone();
                        let _ = state_clone.runtime.publish_devin_member_catalog(
                            &member.id,
                            &rev,
                            cat,
                            &state_clone.devin_cache,
                        );
                    }
                }
                state_clone.notify_finalizer(Some(&member.id), "devin_catalog_refresh");
            }
        });
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "models": all_models,
            "accounts": accounts,
            "generation": state.runtime.generation(),
        })),
    )
}

pub fn registry_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/model-registry", get(get_registry).post(refresh_registry))
        .route("/devin/models/refresh", post(refresh_devin_models))
        .route("/devin/models/status", get(get_devin_models_status))
        .route("/devin/models", get(get_devin_models_status))
}
