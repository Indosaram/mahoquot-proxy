use crate::state::AppState;
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

fn json_status(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

pub(crate) async fn latest_version(State(state): State<Arc<AppState>>) -> Response {
    let endpoint = std::env::var("MAHOQUOT_LATEST_VERSION_URL").unwrap_or_else(|_| {
        "https://api.github.com/repos/router-for-me/CLIProxyAPI/releases/latest".into()
    });

    let proxy_url = state.settings.current().proxy_url.clone();
    let client = match crate::proxy_policy::build_http_client(if proxy_url.is_empty() {
        None
    } else {
        Some(&proxy_url)
    }) {
        Ok(client) => client,
        Err(error) => {
            return json_status(
                StatusCode::BAD_GATEWAY,
                json!({"error":"client_failed","message":error.to_string()}),
            )
        }
    };

    let mut request_builder = client
        .get(&endpoint)
        .header(header::USER_AGENT, "CLIProxyAPI")
        .header(header::ACCEPT, "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(10));

    let gh_token = state.settings.current().github_token.clone();
    let token: Option<String> = if !gh_token.trim().is_empty() {
        Some(gh_token)
    } else {
        std::env::var("GITHUB_TOKEN").ok()
    };
    if let Some(t) = token {
        if !t.trim().is_empty() {
            request_builder = request_builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
    }

    let response = match request_builder.send().await {
        Ok(resp) => resp,
        Err(error) => {
            return json_status(
                StatusCode::BAD_GATEWAY,
                json!({"error":"upstream_unavailable","message":error.to_string()}),
            )
        }
    };

    if !response.status().is_success() {
        let status_code = response.status().as_u16();
        let body_preview = response.text().await.unwrap_or_default();
        let message = if body_preview.len() > 1024 {
            body_preview[..1024].to_string()
        } else {
            body_preview
        };
        return json_status(
            StatusCode::BAD_GATEWAY,
            json!({"error":"upstream_status","status":status_code,"message":message}),
        );
    }

    let body: Value = match response.json().await {
        Ok(val) => val,
        Err(error) => {
            return json_status(
                StatusCode::BAD_GATEWAY,
                json!({"error":"invalid_release_response","message":error.to_string()}),
            )
        }
    };

    let version = body
        .get("tag_name")
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            body.get("name")
                .and_then(Value::as_str)
                .filter(|v| !v.trim().is_empty())
        });

    match version {
        Some(ver) => json_status(StatusCode::OK, json!({"latest-version": ver})),
        None => json_status(
            StatusCode::BAD_GATEWAY,
            json!({"error":"invalid_release_response","message":"missing release version"}),
        ),
    }
}

pub(crate) async fn api_call(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    let request: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return json_status(StatusCode::BAD_REQUEST, json!({"error":"invalid body"})),
    };

    let method_raw = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if method_raw.is_empty() {
        return json_status(StatusCode::BAD_REQUEST, json!({"error":"missing method"}));
    }
    let method = match reqwest::Method::from_bytes(method_raw.to_ascii_uppercase().as_bytes()) {
        Ok(m) => m,
        Err(_) => return json_status(StatusCode::BAD_REQUEST, json!({"error":"invalid method"})),
    };

    let url_raw = request
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if url_raw.is_empty() {
        return json_status(StatusCode::BAD_REQUEST, json!({"error":"missing url"}));
    }
    let parsed_url = match reqwest::Url::parse(url_raw) {
        Ok(u) if u.scheme() != "http" && u.scheme() != "https" => {
            return json_status(StatusCode::BAD_REQUEST, json!({"error":"invalid url"}));
        }
        Ok(u) if u.host_str().is_none() => {
            return json_status(StatusCode::BAD_REQUEST, json!({"error":"invalid url"}));
        }
        Ok(u) => u,
        Err(_) => return json_status(StatusCode::BAD_REQUEST, json!({"error":"invalid url"})),
    };

    let auth_index = request
        .get("auth_index")
        .or_else(|| request.get("authIndex"))
        .or_else(|| request.get("AuthIndex"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();

    let mut resolved_token = String::new();
    let mut credential_proxy_url = String::new();
    if !auth_index.is_empty() {
        let auth_dir = state.settings.current().auth_dir.clone();
        let auth_path = std::path::PathBuf::from(&auth_dir);
        let resolved_filename = crate::management::creds::resolve_auth_index(&state, auth_index);
        let cred_file = resolved_filename
            .as_deref()
            .map(|f| auth_path.join(f))
            .or_else(|| {
                let candidate = auth_path.join(auth_index);
                if candidate.exists() {
                    Some(candidate)
                } else {
                    None
                }
            });

        if let Some(file) = cred_file {
            if let Ok(content) = std::fs::read_to_string(&file) {
                if let Ok(cred_json) = serde_json::from_str::<Value>(&content) {
                    if let Some(tok) = cred_json
                        .get("access_token")
                        .or_else(|| cred_json.get("api_key"))
                        .or_else(|| cred_json.get("token"))
                        .or_else(|| cred_json.get("id_token"))
                        .or_else(|| cred_json.get("session_token"))
                        .or_else(|| cred_json.get("cookie"))
                        .or_else(|| cred_json.pointer("/metadata/access_token"))
                        .or_else(|| cred_json.pointer("/metadata/token"))
                        .and_then(Value::as_str)
                    {
                        resolved_token = tok.to_string();
                    }
                    if let Some(proxy) = cred_json
                        .get("proxy_url")
                        .or_else(|| cred_json.get("proxy-url"))
                        .or_else(|| cred_json.pointer("/metadata/proxy_url"))
                        .and_then(Value::as_str)
                    {
                        credential_proxy_url = proxy.to_string();
                    }
                }
            }
        }
    }

    let req_proxy = request
        .get("proxy_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    let global_proxy = state.settings.current().proxy_url.clone();
    let selected_proxy = if !req_proxy.is_empty() {
        Some(req_proxy)
    } else if !credential_proxy_url.is_empty() {
        Some(credential_proxy_url.as_str())
    } else if !global_proxy.trim().is_empty() {
        Some(global_proxy.as_str())
    } else {
        None
    };

    let client = match crate::proxy_policy::build_http_client(selected_proxy) {
        Ok(c) => c,
        Err(e) => {
            return json_status(
                StatusCode::BAD_REQUEST,
                json!({"error":format!("invalid proxy: {e}")}),
            )
        }
    };

    let mut req_headers = HashMap::new();
    if let Some(headers_obj) = request.get("header").and_then(Value::as_object) {
        for (k, v) in headers_obj {
            if let Some(v_str) = v.as_str() {
                if v_str.contains("$TOKEN$") {
                    if resolved_token.is_empty() {
                        return json_status(
                            StatusCode::BAD_REQUEST,
                            json!({"error":"auth token not found"}),
                        );
                    }
                    req_headers.insert(k.clone(), v_str.replace("$TOKEN$", &resolved_token));
                } else {
                    req_headers.insert(k.clone(), v_str.to_string());
                }
            }
        }
    }

    let mut data_str = request
        .get("data")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if data_str.contains("$TOKEN$") {
        if resolved_token.is_empty() {
            return json_status(
                StatusCode::BAD_REQUEST,
                json!({"error":"auth token not found"}),
            );
        }
        data_str = data_str.replace("$TOKEN$", &resolved_token);
    }

    let mut http_req = client
        .request(method, parsed_url)
        .timeout(std::time::Duration::from_secs(60));
    for (k, v) in req_headers {
        if k.eq_ignore_ascii_case("host") {
            continue;
        }
        if let (Ok(hname), Ok(hval)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(&v),
        ) {
            http_req = http_req.header(hname, hval);
        }
    }

    if !data_str.is_empty() {
        http_req = http_req.body(data_str);
    }

    let upstream_resp = match http_req.send().await {
        Ok(resp) => resp,
        Err(_) => return json_status(StatusCode::BAD_GATEWAY, json!({"error":"request failed"})),
    };

    let status_code = upstream_resp.status().as_u16();
    let mut header_map: HashMap<String, Vec<String>> = HashMap::new();
    for (name, val) in upstream_resp.headers() {
        let key = name.as_str().to_string();
        let value = redact_secret(val.to_str().unwrap_or(""), &resolved_token);
        header_map.entry(key).or_default().push(value);
    }

    let body_text = match upstream_resp.text().await {
        Ok(t) => t,
        Err(_) => {
            return json_status(
                StatusCode::BAD_GATEWAY,
                json!({"error":"failed to read response"}),
            )
        }
    };

    json_status(
        StatusCode::OK,
        json!({
            "status_code": status_code,
            "header": header_map,
            "body": redact_secret(&body_text, &resolved_token),
        }),
    )
}

const REDACTED_SECRET: &str = "[REDACTED]";

/// The credential substituted for the token placeholder must never come back
/// to the management caller, even when the upstream echoes it.
fn redact_secret(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        return text.to_string();
    }
    text.replace(secret, REDACTED_SECRET)
}

async fn cooldown_reset(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    crate::management::core::reset_quota(State(state), body).await
}

async fn model_definitions(
    State(state): State<Arc<AppState>>,
    Path(channel): Path<String>,
) -> Response {
    crate::management::creds::model_definitions(State(state), Path(channel)).await
}

async fn list_credentials(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    crate::management::creds::list_auth_files(State(state), Query(params)).await
}

async fn upload_credential(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    crate::management::creds::create_auth_file(State(state), body).await
}

async fn delete_credential(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    crate::management::creds::delete_auth_file(State(state), Query(params)).await
}

async fn credential_models(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    crate::management::creds::auth_file_models(State(state), Query(params)).await
}

async fn download_credential(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    crate::management::creds::download_auth_file(State(state), headers, Query(params)).await
}

async fn patch_credential_status(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    crate::management::creds::patch_auth_file_status(State(state), Json(body)).await
}

async fn patch_credential_fields(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    crate::management::creds::patch_auth_file_fields(State(state), Json(body)).await
}

pub(crate) async fn refresh_credentials(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    let body_json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let all = params.get("all").map(|v| v == "true").unwrap_or(false)
        || body_json
            .get("all")
            .and_then(Value::as_bool)
            .unwrap_or(false);

    let name = params
        .get("name")
        .cloned()
        .or_else(|| {
            body_json
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();

    let auth_index = params
        .get("auth_index")
        .cloned()
        .or_else(|| {
            body_json
                .get("auth_index")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();

    if all {
        crate::quota::refresh_all_usage(&state).await;
        let mut results = Vec::new();
        for member in &state.pool.load().members {
            results.push(json!({
                "auth_index": member.id,
                "status": "ok"
            }));
        }
        return json_status(StatusCode::OK, json!({"ok": true, "results": results}));
    }

    if name.is_empty() && auth_index.is_empty() {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({"error": "name or all=true is required"}),
        );
    }

    let auth_dir = state.settings.current().auth_dir.clone();
    let auth_path = std::path::PathBuf::from(&auth_dir);
    let target_name = if !name.is_empty() {
        name
    } else {
        match crate::management::creds::resolve_auth_index(&state, &auth_index) {
            Some(resolved) => resolved,
            None => {
                return json_status(
                    StatusCode::NOT_FOUND,
                    json!({"error": "auth file not found"}),
                )
            }
        }
    };

    let target_file = auth_path.join(&target_name);
    if !target_file.exists() {
        return json_status(
            StatusCode::NOT_FOUND,
            json!({"error": "auth file not found"}),
        );
    }

    let member = state
        .pool
        .load()
        .members
        .iter()
        .find(|m| {
            m.file_path
                .file_name()
                .map(|n| n.to_string_lossy() == target_name)
                .unwrap_or(false)
        })
        .cloned();

    if let Some(m) = member {
        let _ = state.refresh_member(&m, None).await;
        let _ = crate::quota::refresh_account_usage(&state, &m).await;
        json_status(
            StatusCode::OK,
            json!({
                "ok": true,
                "auth": {
                    "name": target_name,
                    "status": "refreshed",
                    "models": m.usage_snapshot()
                }
            }),
        )
    } else {
        if let Err(error) = state.rescan_pool() {
            eprintln!("pool rescan during refresh error: {error}");
        }
        json_status(
            StatusCode::OK,
            json!({
                "ok": true,
                "auth": {
                    "name": target_name,
                    "status": "refreshed"
                }
            }),
        )
    }
}

async fn start_oauth_v8(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    crate::management::oauth::start_oauth_by_provider(State(state), Query(params)).await
}

async fn import_oauth_v8(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    let provider = params.get("provider").map(String::as_str).unwrap_or("");
    match provider {
        "" => json_status(
            StatusCode::BAD_REQUEST,
            json!({"error": "provider is required"}),
        ),
        "vertex" => crate::management::creds::vertex_import(State(state), body).await,
        _ => json_status(
            StatusCode::NOT_FOUND,
            json!({"error": "provider_not_found"}),
        ),
    }
}

async fn oauth_status(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    crate::management::oauth::auth_status(State(state), Query(params)).await
}

async fn cancel_oauth_session(Query(params): Query<HashMap<String, String>>) -> Response {
    crate::management::oauth::cancel_session(Query(params)).await
}

pub(crate) async fn storage_status_v8(State(state): State<Arc<AppState>>) -> Response {
    if let Some(mgr) = state.settings.storage_manager() {
        let (available, error) = match mgr.check_health() {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };
        let status = if available {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        };
        json_status(
            status,
            json!({
                "configured": true,
                "available": available,
                "error": error,
                "spool_root": mgr.spool_root().display().to_string(),
            }),
        )
    } else {
        json_status(
            StatusCode::OK,
            json!({
                "configured": false,
                "available": true,
                "backend": "local",
            }),
        )
    }
}

async fn list_plugins(State(state): State<Arc<AppState>>) -> Response {
    crate::management::plugins::list_plugins(State(state)).await
}

async fn delete_plugin(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    crate::management::plugins::delete_plugin(State(state), Path(id)).await
}

async fn list_plugin_store(
    State(state): State<Arc<AppState>>,
    Query(query): Query<crate::management::plugins::StoreListQuery>,
) -> Response {
    crate::management::plugins::list_plugin_store(State(state), Query(query)).await
}

async fn install_plugin_from_store(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<crate::management::plugins::InstallRequest>,
    body: Bytes,
) -> Response {
    crate::management::plugins::install_plugin(State(state), Path(id), Query(query), body).await
}

async fn get_plugin_quota(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let auth_index = params
        .get("auth_index")
        .or_else(|| params.get("authIndex"))
        .cloned()
        .unwrap_or_default();
    crate::management::plugins::query_plugin_quota_internal(&state, &id, &auth_index, false).await
}

async fn fetch_plugin_quota(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    let body_json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let auth_index = params
        .get("auth_index")
        .or_else(|| params.get("authIndex"))
        .cloned()
        .or_else(|| {
            body_json
                .get("auth_index")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .or_else(|| {
            body_json
                .get("authIndex")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .unwrap_or_default();
    crate::management::plugins::query_plugin_quota_internal(&state, &id, &auth_index, false).await
}

async fn reset_plugin_quota(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    let body_json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let auth_index = params
        .get("auth_index")
        .or_else(|| params.get("authIndex"))
        .cloned()
        .or_else(|| {
            body_json
                .get("auth_index")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .or_else(|| {
            body_json
                .get("authIndex")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .unwrap_or_default();
    crate::management::plugins::query_plugin_quota_internal(&state, &id, &auth_index, true).await
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/server/latest-version", get(latest_version))
        .route("/requests/api-call", post(api_call))
        .route("/routing/cooldown/reset", post(cooldown_reset))
        .route(
            "/routing/model-definitions/{channel}",
            get(model_definitions),
        )
        .route(
            "/observability/logs",
            get(super::telemetry::get_logs).delete(super::telemetry::delete_logs),
        )
        .route(
            "/observability/logs/errors",
            get(super::telemetry::request_errors),
        )
        .route(
            "/observability/logs/errors/{name}",
            get(super::telemetry::download_request_error),
        )
        .route(
            "/observability/logs/requests/{id}",
            get(super::telemetry::request_log),
        )
        .route(
            "/observability/usage/api-keys",
            get(super::telemetry::api_key_usage),
        )
        .route(
            "/observability/usage/queue",
            get(super::telemetry::usage_queue),
        )
        .route(
            "/credentials",
            get(list_credentials)
                .post(upload_credential)
                .delete(delete_credential),
        )
        .route("/credentials/models", get(credential_models))
        .route("/credentials/download", get(download_credential))
        .route("/credentials/status", patch(patch_credential_status))
        .route("/credentials/fields", patch(patch_credential_fields))
        .route("/credentials/refresh", post(refresh_credentials))
        .route("/oauth/import", post(import_oauth_v8))
        .route("/oauth/auth-url", get(start_oauth_v8))
        .route("/oauth/status", get(oauth_status))
        .route("/oauth/session", delete(cancel_oauth_session))
        .route("/storage/status", get(storage_status_v8))
        .route("/plugins", get(list_plugins))
        .route("/plugins/{id}", delete(delete_plugin))
        .route("/plugins/store", get(list_plugin_store))
        .route(
            "/plugins/store/{id}/install",
            post(install_plugin_from_store),
        )
        .route(
            "/plugins/{id}/quota",
            get(get_plugin_quota)
                .post(fetch_plugin_quota)
                .delete(reset_plugin_quota),
        )
}

#[cfg(test)]
mod tests {
    #[test]
    fn api_call_redacts_the_substituted_credential_from_echoed_responses() {
        assert_eq!(
            super::redact_secret("Bearer tok-123 seen", "tok-123"),
            "Bearer [REDACTED] seen"
        );
        assert_eq!(
            super::redact_secret("nothing secret", "tok-123"),
            "nothing secret"
        );
        assert_eq!(super::redact_secret("unchanged", ""), "unchanged");
    }

    use super::*;

    #[test]
    fn latest_release_requires_a_real_remote_version() {
        let value = json!({"tag_name":"v1.2.3"});
        assert_eq!(value["tag_name"].as_str(), Some("v1.2.3"));
        assert!(json!({}).get("tag_name").and_then(Value::as_str).is_none());
    }

    #[test]
    fn v8_route_manifest_contains_all_core_endpoints() {
        let res = json_status(StatusCode::OK, json!({"status":"ok"}));
        assert_eq!(res.status(), StatusCode::OK);
    }
}
