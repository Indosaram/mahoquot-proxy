use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use mahoquot_providers::credential_file::write_credential_atomically;
use serde_json::{json, Value};

use crate::state::AppState;

fn json_status(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

const ACCOUNT_ORDER_FILE: &str = ".mahoquot-account-order.json";

fn is_credential_filename(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    lowered.ends_with(".json") && lowered != ACCOUNT_ORDER_FILE
        && lowered != "account-order.json" && lowered != "telemetry.json"
}

fn is_credential_document(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some(
            "codex"
                | "antigravity"
                | "claude"
                | "anthropic"
                | "cursor"
                | "kiro"
                | "zcode"
                | "generic"
                | "vertex"
                | "google-vertex"
                | "devin"
                | "monitor"
        )
    )
}

fn ordered_names(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(dir.join(ACCOUNT_ORDER_FILE))
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
        .unwrap_or_default()
}

fn sort_described_files(files: &mut [Value], order: &[String]) {
    files.sort_by(|a, b| {
        let a_name = a["name"].as_str().unwrap_or_default();
        let b_name = b["name"].as_str().unwrap_or_default();
        let a_index = order
            .iter()
            .position(|name| name == a_name)
            .unwrap_or(usize::MAX);
        let b_index = order
            .iter()
            .position(|name| name == b_name)
            .unwrap_or(usize::MAX);
        a_index.cmp(&b_index).then_with(|| a_name.cmp(b_name))
    });
}

/// Describe one credential file the way upstream does: filesystem metadata
/// plus the `type`/`email` fields read out of the JSON itself, so the desktop
/// app can list accounts without opening every file.
fn describe(dir: &std::path::Path, name: &str) -> Option<Value> {
    let full = dir.join(name);
    let meta = std::fs::metadata(&full).ok()?;
    let mut entry = json!({
        "name": name,
        "size": meta.len(),
        "auth_index": auth_index(name),
        "path": full.to_string_lossy(),
        "label": name.trim_end_matches(".json"),
        "disabled": false,
        "unavailable": false,
        "runtime_only": false,
    });
    if let Ok(modified) = meta.modified() {
        if let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH) {
            entry["modtime"] = json!(since.as_secs());
        }
    }
    if let Ok(raw) = std::fs::read_to_string(&full) {
        if let Ok(parsed) = serde_json::from_str::<Value>(&raw) {
            let kind = parsed
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let email = parsed
                .get("email")
                .and_then(Value::as_str)
                .unwrap_or_default();
            entry["type"] = json!(kind);
            entry["email"] = json!(email);
            entry["provider"] = parsed
                .get("provider")
                .cloned()
                .unwrap_or_else(|| json!(kind));
            entry["account"] = json!(email);
            entry["account_type"] = json!("oauth");
            if let Some(slug) = parsed.get("identity_slug").and_then(Value::as_str) {
                if !slug.trim().is_empty() {
                    entry["identity_slug"] = json!(slug.trim());
                    entry["identity"] = json!(slug.trim());
                }
            }
            if kind == "devin" {
                entry["account_type"] = json!("token");
                let slug = parsed
                    .get("identity_slug")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        name.strip_prefix("devin-")
                            .unwrap_or(name)
                            .strip_suffix(".json")
                            .unwrap_or(name)
                            .to_string()
                    });
                entry["identity_slug"] = json!(&slug);
                entry["identity"] = json!(&slug);
                entry["account"] = json!(&slug);
                if entry["label"] == name.trim_end_matches(".json") {
                    entry["label"] = json!(&slug);
                }
            }
            if let Some(lbl) = parsed.get("label").and_then(Value::as_str) {
                if !lbl.trim().is_empty() && lbl != "generic" {
                    entry["label"] = json!(lbl);
                }
            } else if !email.is_empty() {
                entry["label"] = json!(email);
            }
            let disabled = parsed
                .get("disabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            entry["disabled"] = json!(disabled);
            entry["status"] = json!(if disabled { "disabled" } else { "active" });
            if let Some(project) = parsed.get("project_id").and_then(Value::as_str) {
                if !project.trim().is_empty() {
                    entry["project_id"] = json!(project);
                }
            }
        }
    }
    Some(entry)
}

async fn list_auth_files(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let dir = state.settings.current().auth_dir.clone();
    let dir = std::path::PathBuf::from(dir);

    if params
        .get("auth_index")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        return json_status(StatusCode::OK, json!({ "files": [] }));
    }
    let name_filter = params.get("name").map(|v| v.trim().to_string());

    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return json_status(StatusCode::OK, json!({ "files": [] }))
        }
        Err(err) => {
            return json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": format!("failed to read auth dir: {err}") }),
            )
        }
    };

    let mut files = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !is_credential_filename(&name) {
            continue;
        }
        if name_filter
            .as_deref()
            .is_some_and(|f| !f.is_empty() && f != name)
        {
            continue;
        }
        let is_credential = std::fs::read_to_string(dir.join(&name))
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .is_some_and(|value| is_credential_document(&value));
        if is_credential {
            if let Some(described) = describe(&dir, &name) {
                files.push(described);
            }
        }
    }
    sort_described_files(&mut files, &ordered_names(&dir));
    json_status(StatusCode::OK, json!({ "files": files }))
}

async fn save_auth_file_order(State(state): State<Arc<AppState>>, raw: bytes::Bytes) -> Response {
    let Ok(body) = serde_json::from_slice::<Value>(&raw) else {
        return json_status(StatusCode::BAD_REQUEST, json!({ "error": "invalid body" }));
    };
    let Some(names) = body.get("names").and_then(Value::as_array) else {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({ "error": "names is required" }),
        );
    };
    let names: Vec<String> = names
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty() && !name.contains('/') && !name.contains(".."))
        .map(str::to_string)
        .collect();
    if names.len() != body["names"].as_array().map_or(0, Vec::len) {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({ "error": "invalid credential name" }),
        );
    }
    let dir = std::path::PathBuf::from(state.settings.current().auth_dir.clone());
    if let Err(err) = std::fs::create_dir_all(&dir) {
        return json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": err.to_string() }),
        );
    }
    let rendered = match serde_json::to_string_pretty(&names) {
        Ok(rendered) => rendered,
        Err(err) => {
            return json_status(StatusCode::BAD_REQUEST, json!({ "error": err.to_string() }))
        }
    };
    match write_credential_atomically(&dir.join(ACCOUNT_ORDER_FILE), rendered.as_bytes()) {
        Ok(()) => {
            if let Err(err) = state.rescan_pool() {
                tracing::warn!(error = %err, "failed to rescan pool after saving account order");
            }
            json_status(StatusCode::OK, json!({ "status": "ok", "names": names }))
        }
        Err(err) => json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": err.to_string() }),
        ),
    }
}

async fn create_auth_file(State(state): State<Arc<AppState>>, raw: bytes::Bytes) -> Response {
    let Ok(body) = serde_json::from_slice::<Value>(&raw) else {
        return json_status(StatusCode::BAD_REQUEST, json!({ "error": "invalid body" }));
    };
    let Some(name) = body.get("name").and_then(Value::as_str).map(str::trim) else {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({ "error": "name is required" }),
        );
    };
    if name.is_empty() || name.contains('/') || name.contains("..") {
        return json_status(StatusCode::BAD_REQUEST, json!({ "error": "invalid name" }));
    }
    let mut content = body.get("content").cloned().unwrap_or(Value::Null);
    if let Err(error) = validate_provider_credential(&content) {
        return json_status(StatusCode::BAD_REQUEST, json!({ "error": error }));
    }
    if content.get("type").and_then(Value::as_str) == Some("devin") {
        let devin_account: mahoquot_providers::DevinAccount = match serde_json::from_value(content.clone()) {
            Ok(acct) => acct,
            Err(_) => return json_status(StatusCode::BAD_REQUEST, json!({ "error": "invalid devin credential format" })),
        };
        let validated = match devin_account.validate() {
            Ok(acct) => acct,
            Err(e) => return json_status(StatusCode::BAD_REQUEST, json!({ "error": format!("invalid devin credential: {e}") })),
        };
        content = match serde_json::to_value(&validated) {
            Ok(v) => v,
            Err(e) => return json_status(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": format!("failed to serialize normalized devin credential: {e}") })),
        };
    }
    let dir = std::path::PathBuf::from(state.settings.current().auth_dir.clone());
    if let Err(err) = std::fs::create_dir_all(&dir) {
        return json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": format!("failed to write auth file: {err}") }),
        );
    }
    let rendered = match serde_json::to_string_pretty(&content) {
        Ok(rendered) => rendered,
        Err(err) => {
            return json_status(
                StatusCode::BAD_REQUEST,
                json!({ "error": format!("invalid content: {err}") }),
            )
        }
    };
    match write_credential_atomically(&dir.join(name), rendered.as_bytes()) {
        Ok(()) => {
            if let Err(error) = state.rescan_pool() {
                eprintln!("pool rescan failed after credential write: {error}");
            }
            json_status(StatusCode::OK, json!({ "status": "ok", "name": name }))
        }
        Err(err) => json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": format!("failed to write auth file: {err}") }),
        ),
    }
}

fn required_string<'a>(content: &'a Value, field: &str) -> Result<&'a str, String> {
    content
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("credential field {field} is required"))
}

pub(crate) fn validate_provider_credential(content: &Value) -> Result<(), String> {
    let kind = required_string(content, "type")?;
    match kind {
        // A claude static-key relay deployment (api_key + upstream_override)
        // has inert refresh/expiry, so demanding the OAuth quartet would reject
        // the only shape the add-screen can produce for relay targets.
        "claude" | "anthropic" => {
            required_string(content, "email")?;
            if content.get("api_key").and_then(Value::as_str).is_some() {
                required_string(content, "upstream_override")?;
            } else {
                required_string(content, "access_token")?;
                required_string(content, "refresh_token")?;
                required_string(content, "expired")?;
            }
        }
        "cursor" => {
            required_string(content, "access_token")?;
            required_string(content, "refresh_token")?;
            required_string(content, "email")?;
            required_string(content, "expired")?;
        }
        "kiro" => {
            required_string(content, "access_token")?;
            required_string(content, "refresh_token")?;
            required_string(content, "email")?;
            required_string(content, "expired")?;
            if content.get("auth_mode").and_then(Value::as_str) == Some("idc") {
                required_string(content, "client_id")?;
                required_string(content, "client_secret")?;
            }
        }
        "zcode" => {
            let key = required_string(content, "access_token")?;
            if !mahoquot_providers::zcode::is_provisioned_api_key(key) {
                return Err("zcode access_token must be a provisioned {id}.{secret} key".into());
            }
            required_string(content, "email")?;
            // A provisioned key never expires and has nothing to refresh from,
            // so demanding those two fields would reject the only credential
            // shape an operator can actually paste.
        }
        "generic" => {
            for field in ["provider", "adapter", "base_url"] {
                required_string(content, field)?;
            }
        }
        "vertex" | "google-vertex" => {
            required_string(content, "project_id")?;
        }
        "devin" => {
            let account: mahoquot_providers::DevinAccount = serde_json::from_value(content.clone())
                .map_err(|_| "invalid devin credential format".to_string())?;
            account
                .validate()
                .map_err(|e| format!("invalid devin credential: {e}"))?;
        }
        "monitor" => {
            required_string(content, "provider")?;
            required_string(content, "label")?;
        }
        "codex" | "antigravity" => {}
        _ => return Err(format!("unsupported credential type {kind}")),
    }
    Ok(())
}

/// Stable duplicate-detection identity for imports. Provider plus email keeps
/// separate providers distinct; account/project/identity ids cover credentials
/// that do not carry an email. Secret token material is never part of the key.
pub(crate) fn credential_identity(content: &Value) -> Option<String> {
    let provider = content
        .get("provider")
        .or_else(|| content.get("type"))
        .and_then(Value::as_str)?
        .trim()
        .to_ascii_lowercase();
    if provider.is_empty() {
        return None;
    }
    let identity = ["email", "account_id", "project_id", "identity_slug"]
        .into_iter()
        .find_map(|field| {
            content
                .get(field)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_ascii_lowercase)
        })?;
    Some(format!("{provider}|{identity}"))
}

/// Upstream addresses a credential by a stable opaque handle rather than its
/// filename, and `POST /reset-quota` takes that handle. It is derived from the
/// name so it survives restarts and stays identical for the same account.
fn auth_index(name: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Map an opaque handle back to the credential FILENAME that produced it. The
/// handle is not stored anywhere, so the directory is scanned and each name
/// re-hashed; the pool is small enough that this stays cheap. Callers match the
/// result against a member's `file_path`, which is the only identifier shared
/// between the directory listing and the loaded pool.
pub fn resolve_auth_index(state: &AppState, wanted: &str) -> Option<String> {
    let dir = std::path::PathBuf::from(state.settings.current().auth_dir.clone());
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.to_ascii_lowercase().ends_with(".json") {
            continue;
        }
        if auth_index(&name) == wanted || name == wanted {
            return Some(name);
        }
    }
    None
}

async fn delete_auth_file(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(name) = params.get("name").map(|v| v.trim()) else {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({ "error": "name is required" }),
        );
    };
    if name.is_empty() || name.contains('/') || name.contains("..") {
        return json_status(StatusCode::BAD_REQUEST, json!({ "error": "invalid name" }));
    }
    let dir = std::path::PathBuf::from(state.settings.current().auth_dir.clone());
    match std::fs::remove_file(dir.join(name)) {
        Ok(()) => {
            if let Err(error) = state.rescan_pool() {
                eprintln!("pool rescan failed after credential delete: {error}");
            }
            json_status(StatusCode::OK, json!({ "status": "ok" }))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            json_status(StatusCode::NOT_FOUND, json!({ "error": "auth not found" }))
        }
        Err(err) => json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": format!("failed to delete auth file: {err}") }),
        ),
    }
}

async fn auth_file_models(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if params
        .get("name")
        .map(|v| v.trim())
        .unwrap_or("")
        .is_empty()
    {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({ "error": "name is required" }),
        );
    }
    json_status(
        StatusCode::OK,
        json!({ "models": crate::models_route::models_payload(&state.pool.load().models, 0) }),
    )
}

async fn model_definitions(Path(channel): Path<String>) -> Response {
    json_status(
        StatusCode::OK,
        json!({ "channel": channel, "models": Value::Array(vec![]) }),
    )
}

async fn download_auth_file(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if !super::accounts::export_authorized(&state, &headers) {
        return super::accounts::export_refusal();
    }
    let Some(name) = params.get("name").map(|v| v.trim()) else {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({ "error": "name is required" }),
        );
    };
    if name.is_empty() || name.contains('/') || name.contains("..") {
        return json_status(StatusCode::BAD_REQUEST, json!({ "error": "invalid name" }));
    }
    let dir = std::path::PathBuf::from(state.settings.current().auth_dir.clone());
    match std::fs::read(dir.join(name)) {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/json"),
                (
                    header::CONTENT_DISPOSITION,
                    &format!("attachment; filename=\"{name}\""),
                ),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => json_status(StatusCode::NOT_FOUND, json!({ "error": "auth not found" })),
    }
}

async fn patch_auth_file_status(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let Some(name) = body.get("name").and_then(Value::as_str) else {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({ "error": "name is required" }),
        );
    };
    let Some(disabled) = body.get("disabled").and_then(Value::as_bool) else {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({ "error": "disabled is required" }),
        );
    };
    if name.is_empty() || name.contains('/') || name.contains("..") || !is_credential_filename(name) {
        return json_status(StatusCode::BAD_REQUEST, json!({ "error": "invalid name" }));
    }
    let dir = std::path::PathBuf::from(state.settings.current().auth_dir.clone());
    let path = dir.join(name);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return json_status(StatusCode::NOT_FOUND, json!({ "error": "auth not found" }))
        }
        Err(error) => {
            return json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": error.to_string() }),
            )
        }
    };
    let mut value: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(error) => {
            return json_status(
                StatusCode::BAD_REQUEST,
                json!({ "error": error.to_string() }),
            )
        }
    };
    if !value.is_object() {
        return json_status(StatusCode::BAD_REQUEST, json!({ "error": "auth file must be a JSON object" }));
    }
    let legacy_codex = name.starts_with("codex-")
        && serde_json::from_value::<mahoquot_providers::CodexAccount>(value.clone()).is_ok();
    if !is_credential_document(&value) && !legacy_codex {
        return json_status(StatusCode::BAD_REQUEST, json!({ "error": "invalid credential document" }));
    }
    if let Value::Object(fields) = &mut value {
        fields.insert("disabled".into(), json!(disabled));
    }
    let rendered = match serde_json::to_string_pretty(&value) {
        Ok(rendered) => rendered,
        Err(error) => {
            return json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": error.to_string() }),
            )
        }
    };
    if let Err(error) = write_credential_atomically(&path, rendered.as_bytes()) {
        return json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": error.to_string() }),
        );
    }
    if let Err(error) = state.rescan_pool() {
        return json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": error.to_string() }),
        );
    }
    if !disabled {
        let state_clone = Arc::clone(&state);
        tokio::spawn(async move {
            crate::quota::refresh_all_usage(&state_clone).await;
        });
    }
    json_status(
        StatusCode::OK,
        json!({ "status": "ok", "name": name, "disabled": disabled }),
    )
}

async fn patch_auth_file_fields(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let Some(obj) = body.as_object() else {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({ "error": "invalid request body" }),
        );
    };
    let Some(name) = obj.get("name").and_then(Value::as_str) else {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({ "error": "name is required" }),
        );
    };
    let name = name.trim();
    if name.is_empty() || name.contains('/') || name.contains("..") {
        return json_status(StatusCode::BAD_REQUEST, json!({ "error": "invalid name" }));
    }
    let dir = std::path::PathBuf::from(state.settings.current().auth_dir.clone());
    let path = dir.join(name);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return json_status(StatusCode::NOT_FOUND, json!({ "error": "auth not found" }))
        }
        Err(error) => {
            return json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": error.to_string() }),
            )
        }
    };
    let mut file_value: Value = match serde_json::from_str(&raw) {
        Ok(val) => val,
        Err(error) => {
            return json_status(
                StatusCode::BAD_REQUEST,
                json!({ "error": error.to_string() }),
            )
        }
    };
    let Some(target_map) = file_value.as_object_mut() else {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({ "error": "auth file is not a json object" }),
        );
    };

    for (key, val) in obj {
        if key == "name" {
            continue;
        }
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        if key.contains('.') {
            let parts: Vec<&str> = key
                .split('.')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if parts.is_empty() {
                continue;
            }
            let mut curr = &mut *target_map;
            for p in parts.iter().take(parts.len().saturating_sub(1)) {
                if !curr.contains_key(*p) || !curr[*p].is_object() {
                    curr.insert((*p).to_string(), json!({}));
                }
                curr = curr.get_mut(*p).unwrap().as_object_mut().unwrap();
            }
            let last = parts[parts.len() - 1];
            if val.is_null() {
                curr.remove(last);
            } else {
                curr.insert(last.to_string(), val.clone());
            }
        } else if val.is_null() {
            target_map.remove(key);
        } else if let Some(sub_obj) = val.as_object() {
            let entry = target_map
                .entry(key.to_string())
                .or_insert_with(|| json!({}));
            if let Some(entry_map) = entry.as_object_mut() {
                for (sub_k, sub_v) in sub_obj {
                    if sub_v.is_null()
                        || (sub_v.is_string() && sub_v.as_str().unwrap().trim().is_empty())
                    {
                        entry_map.remove(sub_k);
                    } else {
                        entry_map.insert(sub_k.clone(), sub_v.clone());
                    }
                }
            } else {
                target_map.insert(key.to_string(), val.clone());
            }
        } else {
            target_map.insert(key.to_string(), val.clone());
        }
    }

    let rendered = match serde_json::to_string_pretty(&file_value) {
        Ok(rendered) => rendered,
        Err(error) => {
            return json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": error.to_string() }),
            )
        }
    };
    if let Err(error) = write_credential_atomically(&path, rendered.as_bytes()) {
        return json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": error.to_string() }),
        );
    }
    if let Err(error) = state.rescan_pool() {
        return json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": error.to_string() }),
        );
    }
    json_status(StatusCode::OK, json!({ "status": "ok" }))
}

#[derive(serde::Serialize)]
struct VertexClaims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: u64,
    exp: u64,
}

async fn vertex_import(State(state): State<Arc<AppState>>, raw: bytes::Bytes) -> Response {
    let parsed = serde_json::from_slice::<Value>(&raw).unwrap_or(Value::Null);
    let Some(file) = parsed
        .get("file")
        .and_then(Value::as_str)
        .filter(|file| !file.trim().is_empty())
    else {
        return json_status(StatusCode::BAD_REQUEST, json!({ "error": "file required" }));
    };
    let service: Value = match serde_json::from_str(file) {
        Ok(value) => value,
        Err(error) => {
            return json_status(
                StatusCode::BAD_REQUEST,
                json!({ "error": error.to_string() }),
            )
        }
    };
    let required = |field: &str| {
        service
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let (Some(project_id), Some(private_key), Some(client_email)) = (
        required("project_id"),
        required("private_key"),
        required("client_email"),
    ) else {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({ "error": "service account missing project_id, private_key, or client_email" }),
        );
    };
    let token_uri = required("token_uri").unwrap_or("https://oauth2.googleapis.com/token");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let key = match jsonwebtoken::EncodingKey::from_rsa_pem(private_key.as_bytes()) {
        Ok(key) => key,
        Err(error) => {
            return json_status(
                StatusCode::BAD_REQUEST,
                json!({ "error": error.to_string() }),
            )
        }
    };
    let assertion = match jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &VertexClaims {
            iss: client_email,
            scope: "https://www.googleapis.com/auth/cloud-platform",
            aud: token_uri,
            iat: now,
            exp: now + 3600,
        },
        &key,
    ) {
        Ok(assertion) => assertion,
        Err(error) => {
            return json_status(
                StatusCode::BAD_REQUEST,
                json!({ "error": error.to_string() }),
            )
        }
    };
    let response = match state
        .http_client
        .post(token_uri)
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", assertion.as_str()),
        ])
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return json_status(
                StatusCode::BAD_GATEWAY,
                json!({ "error": error.to_string() }),
            )
        }
    };
    let status = response.status();
    let body: Value = match response.json().await {
        Ok(body) => body,
        Err(error) => {
            return json_status(
                StatusCode::BAD_GATEWAY,
                json!({ "error": error.to_string() }),
            )
        }
    };
    if !status.is_success() {
        return json_status(
            StatusCode::BAD_GATEWAY,
            json!({ "error": format!("token exchange failed ({status}): {body}") }),
        );
    }
    let Some(access_token) = body.get("access_token").and_then(Value::as_str) else {
        return json_status(
            StatusCode::BAD_GATEWAY,
            json!({ "error": "token response missing access_token" }),
        );
    };
    let location = parsed
        .get("location")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|loc| !loc.is_empty())
        .unwrap_or("us-central1");
    let project_slug = project_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let credential = json!({
        "type": "vertex",
        "identity_slug": format!("vertex-{project_slug}"),
        "provider": "google-vertex",
        "label": client_email,
        "project_id": project_id,
        "location": location,
        "email": client_email,
        "private_key": private_key,
        "private_key_id": service.get("private_key_id").and_then(Value::as_str),
        "token_url": token_uri,
        "access_token": access_token,
        "expired": mahoquot_providers::format_expired_rfc3339((now + 3600) as i64),
        "last_refresh": mahoquot_providers::format_expired_rfc3339(now as i64),
        "disabled": false,
        "service_account": service,
    });
    let dir = std::path::PathBuf::from(state.settings.current().auth_dir.clone());
    let path = dir.join(format!("vertex-{project_slug}.json"));
    let rendered = match serde_json::to_string_pretty(&credential) {
        Ok(rendered) => rendered,
        Err(error) => {
            return json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": error.to_string() }),
            )
        }
    };
    if let Err(error) = write_credential_atomically(&path, rendered.as_bytes()) {
        return json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": error.to_string() }),
        );
    }
    if let Err(error) = state.rescan_pool() {
        return json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": error.to_string() }),
        );
    }
    json_status(
        StatusCode::OK,
        json!({ "status":"ok", "name":path.file_name().and_then(|name|name.to_str()) }),
    )
}

/// Cline CLI login import. Reads the WorkOS OAuth session the `cline`
/// CLI wrote to `~/.cline/data/settings/providers.json` (`tokenSource:
/// "oauth"`) and stores it as an `auth_mode: "oauth"` generic account:
/// the short-lived `workos:` JWT goes in `api_key`, the rotated
/// `refreshToken` in `refresh_token`, expiry in `expired`. The account
/// refreshes server-side via `POST /api/v1/auth/refresh` (see
/// `execute_cline_refresh`), like the CLI itself. Verified live 2026-09-10:
/// the OAuth token calls free models (`z-ai/glm-5.3-flash`) that a static
/// API key cannot touch (403 ENTITLEMENT_ERROR).
async fn cline_import(State(state): State<Arc<AppState>>) -> Response {
    let path = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join(".cline/data/settings/providers.json");
    let raw = match std::fs::read_to_string(&path) {
        Ok(value) => value,
        Err(error) => {
            return json_status(
                StatusCode::NOT_FOUND,
                json!({"error":format!("cline CLI login not found at {}: {error} (run `cline auth` first)", path.display())}),
            )
        }
    };
    let providers: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(error) => {
            return json_status(StatusCode::BAD_REQUEST, json!({"error":error.to_string()}))
        }
    };
    let cline = providers
        .get("providers")
        .and_then(|p| p.get("cline"))
        .unwrap_or(&Value::Null);
    if cline.get("tokenSource").and_then(Value::as_str) != Some("oauth") {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({"error":"cline CLI is not OAuth-logged-in (tokenSource != oauth); run `cline auth` first"}),
        );
    }
    let settings = cline.get("settings").unwrap_or(&Value::Null);
    let auth = settings.get("auth").unwrap_or(&Value::Null);
    let (Some(access), Some(refresh)) = (
        auth.get("accessToken").and_then(Value::as_str).filter(|v| !v.is_empty()),
        auth.get("refreshToken").and_then(Value::as_str).filter(|v| !v.is_empty()),
    ) else {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({"error":"cline OAuth tokens missing in providers.json"}),
        );
    };
    let expired_ms = auth.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
    let expired = if expired_ms > 0 {
        mahoquot_providers::format_expired_rfc3339(expired_ms.div_euclid(1000))
    } else {
        String::new()
    };
    let account_id = auth.get("accountId").and_then(Value::as_str).unwrap_or("");
    let email = auth
        .get("metadata")
        .and_then(|m| m.get("userInfo"))
        .and_then(|u| u.get("email"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let label = if !email.is_empty() { email } else { "Cline" };
    // Probe the token once before it becomes a stored account file, and take the
    // advertised catalogue from that same response. The model list belongs to the
    // provider: hardcoding it here silently invents capabilities the account may
    // not have and goes stale the moment upstream changes its lineup.
    let probe = state
        .http_client
        .get("https://api.cline.bot/api/v1/models")
        .header(header::AUTHORIZATION, format!("Bearer {access}"))
        .send()
        .await;
    let models: Vec<String> = match probe {
        Ok(response) if response.status().is_success() => {
            match models_from_catalog("cline", response).await {
                Ok(ids) => ids,
                Err(rejection) => return rejection,
            }
        }
        Ok(response) => {
            return json_status(
                StatusCode::UNAUTHORIZED,
                json!({"error": format!("cline OAuth token rejected by upstream: {}", response.status()), "status": "error"}),
            )
        }
        Err(error) => {
            return json_status(
                StatusCode::BAD_GATEWAY,
                json!({"error": format!("cline upstream unreachable: {error}"), "status": "error"}),
            )
        }
    };
    let credential = json!({"type":"generic","provider":"cline","label":label,"email":email,
        "adapter":"openai-chat","auth_mode":"oauth",
        "base_url":"https://api.cline.bot/api/v1",
        "api_key":access,"refresh_token":refresh,"expired":expired,
        "token_url":"https://api.cline.bot/api/v1/auth/refresh",
        "account_id":account_id,
        "models":models,
        "disabled":false});
    let dir = std::path::PathBuf::from(state.settings.current().auth_dir.clone());
    // Filename prefix must be `generic-`: `classify_credential` dispatches
    // on the `generic-` name prefix (ProviderKind::Generic.as_str), and a
    // `generic-cline-` name would miss every prefix and fall through to
    // `from_type_str("generic")` only by luck of the declared type.
    let path = dir.join(format!(
        "generic-cline-oauth-{}.json",
        crate::request_history::stable_key_identifier(account_id),
    ));
    let rendered = match serde_json::to_string_pretty(&credential) {
        Ok(value) => value,
        Err(error) => {
            return json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error":error.to_string()}),
            )
        }
    };
    if let Err(error) = write_credential_atomically(&path, rendered.as_bytes()) {
        return json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":error.to_string()}),
        );
    }
    if let Err(error) = state.rescan_pool() {
        return json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":error.to_string()}),
        );
    }
    json_status(
        StatusCode::OK,
        json!({"status":"ok","name":path.file_name().unwrap_or_default()}),
    )
}

/// Devin CLI login import. Reads the session token written by Devin CLI
/// (`credentials.toml` / `windsurf_api_key`) and stores it as a normalized
/// `devin` account in the gateway's auth directory.
///
/// Security boundary:
/// - Rejects arbitrary file paths in the request body; resolution is strictly
///   governed on the proxy host via DEVIN_CREDENTIALS_PATH -> XDG_DATA_HOME -> ~/.local/share.
/// - The source TOML file is never modified or removed.
/// - Persistence is atomic; blocking file I/O is offloaded via `spawn_blocking`.
/// - Secret tokens are never echoed back in success responses or error diagnostics.
async fn devin_import_cli(State(state): State<Arc<AppState>>, raw: bytes::Bytes) -> Response {
    let mut identity: Option<String> = None;
    let mut label: Option<String> = None;

    if !raw.is_empty() {
        let body: Value = match serde_json::from_slice(&raw) {
            Ok(val) => val,
            Err(err) => {
                return json_status(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": format!("invalid request json: {err}") }),
                );
            }
        };

        let Some(obj) = body.as_object() else {
            return json_status(
                StatusCode::BAD_REQUEST,
                json!({ "error": "request body must be a JSON object" }),
            );
        };

        // Reject arbitrary file path fields strictly with dedicated message
        for key in ["path", "file_path", "file", "filepath", "credentials_path", "source_path"] {
            if obj.contains_key(key) {
                return json_status(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": "arbitrary file paths are not accepted; CLI credentials are resolved on the proxy host" }),
                );
            }
        }

        // Strict allowlist: only identity, identity_slug, and label are permitted
        for key in obj.keys() {
            if key != "identity" && key != "identity_slug" && key != "label" {
                return json_status(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": format!("unknown field `{key}`; only `identity` and `label` (with `identity_slug` alias) are accepted") }),
                );
            }
        }

        let explicit_identity = match obj.get("identity") {
            Some(Value::String(s)) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    return json_status(
                        StatusCode::BAD_REQUEST,
                        json!({ "error": "explicit empty `identity` is not allowed" }),
                    );
                }
                Some(trimmed.to_string())
            }
            Some(_) => {
                return json_status(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": "`identity` must be a string" }),
                );
            }
            None => None,
        };

        let explicit_slug = match obj.get("identity_slug") {
            Some(Value::String(s)) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    return json_status(
                        StatusCode::BAD_REQUEST,
                        json!({ "error": "explicit empty `identity_slug` is not allowed" }),
                    );
                }
                Some(trimmed.to_string())
            }
            Some(_) => {
                return json_status(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": "`identity_slug` must be a string" }),
                );
            }
            None => None,
        };

        match (explicit_identity, explicit_slug) {
            (Some(id), Some(slug)) => {
                if id != slug {
                    return json_status(
                        StatusCode::BAD_REQUEST,
                        json!({ "error": format!("conflicting alias values: identity `{id}` != identity_slug `{slug}`") }),
                    );
                }
                identity = Some(id);
            }
            (Some(id), None) => identity = Some(id),
            (None, Some(slug)) => identity = Some(slug),
            (None, None) => identity = None,
        }

        if let Some(lbl_val) = obj.get("label") {
            match lbl_val {
                Value::String(s) => {
                    let trimmed = s.trim();
                    if !trimmed.is_empty() {
                        label = Some(trimmed.to_string());
                    }
                }
                _ => {
                    return json_status(
                        StatusCode::BAD_REQUEST,
                        json!({ "error": "`label` must be a string" }),
                    );
                }
            }
        }
    }

    let slug = match identity {
        Some(explicit) => explicit,
        None => "devin".to_string(),
    };

    // Validate identity slug strictly BEFORE reading CLI or touching filesystem
    if let Err(err) = mahoquot_providers::devin::validate_identity_slug(&slug) {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({ "error": format!("invalid identity: {err}") }),
        );
    }

    let auth_dir = std::path::PathBuf::from(state.settings.current().auth_dir.clone());
    let filename = format!("devin-{slug}.json");
    let target_path = auth_dir.join(&filename);

    if target_path.parent() != Some(auth_dir.as_path()) {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({ "error": "invalid target path" }),
        );
    }

    let import_task = tokio::task::spawn_blocking(move || {
        let explicit = std::env::var_os(mahoquot_providers::devin::DEVIN_CREDENTIALS_PATH_ENV)
            .map(std::path::PathBuf::from);
        let xdg = std::env::var_os("XDG_DATA_HOME").map(std::path::PathBuf::from);
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let cli_path = mahoquot_providers::devin::resolve_credentials_path(
            explicit.as_deref(),
            xdg.as_deref(),
            home.as_deref(),
        );

        let bytes = match std::fs::read(&cli_path) {
            Ok(b) => b,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err((
                    StatusCode::NOT_FOUND,
                    format!(
                        "Devin CLI credentials not found on proxy host at {} (run `devin auth login` first)",
                        cli_path.display()
                    ),
                ));
            }
            Err(err) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed reading Devin CLI credentials at {}: {err}", cli_path.display()),
                ));
            }
        };

        let cli = mahoquot_providers::devin::parse_cli_credentials(&bytes, &cli_path)
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("failed parsing Devin CLI credentials: {e}")))?;

        let existing_disabled = std::fs::read_to_string(&target_path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.get("disabled").and_then(Value::as_bool))
            .unwrap_or(false);

        let existing_label = std::fs::read_to_string(&target_path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.get("label").and_then(Value::as_str).map(str::to_string));

        let effective_label = label.or(existing_label);

        let mut account = mahoquot_providers::devin::DevinAccount::from_cli(
            cli,
            slug.clone(),
            effective_label,
        )
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid Devin account: {e}")))?;

        if existing_disabled {
            account.disabled = true;
        }

        let validated = account
            .validate()
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid Devin account: {e}")))?;

        let rendered = serde_json::to_string_pretty(&validated)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("failed serializing Devin account: {e}")))?;

        mahoquot_providers::credential_file::write_credential_atomically(&target_path, rendered.as_bytes())
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("failed saving Devin account: {e}")))?;

        Ok((filename, slug))
    })
    .await;

    let (filename, slug) = match import_task {
        Ok(Ok(result)) => result,
        Ok(Err((status, err_msg))) => return json_status(status, json!({ "error": err_msg })),
        Err(join_err) => {
            return json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": format!("import task failed: {join_err}") }),
            );
        }
    };

    if let Err(error) = state.rescan_pool() {
        return json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": format!("pool rescan failed: {error}") }),
        );
    }

    json_status(
        StatusCode::OK,
        json!({ "status": "ok", "name": filename, "identity_slug": slug }),
    )
}

/// Reads an OpenAI-style `{"data":[{"id":..}]}` catalogue out of a successful
/// `/models` probe.
///
/// The model list belongs to the provider. Hardcoding one here would invent
/// capabilities an account may not actually have and would rot silently the
/// moment upstream changes its lineup, so an unreadable or empty catalogue is
/// surfaced as an error instead of being papered over with a static default.
/// Extracts model ids from an OpenAI-style `{"data":[{"id":..}]}` catalogue.
///
/// Shared with the OAuth device flows so every provider reads its catalogue the
/// same way, whatever error idiom the caller uses.
pub(crate) fn model_ids_from_catalog(parsed: &Value) -> Vec<String> {
    parsed
        .get("data")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("id").and_then(Value::as_str))
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

async fn models_from_catalog(
    provider: &str,
    response: reqwest::Response,
) -> Result<Vec<String>, Response> {
    let body = response.bytes().await.map_err(|error| {
        json_status(
            StatusCode::BAD_GATEWAY,
            json!({"error": format!("{provider} model catalog unreadable: {error}"), "status": "error"}),
        )
    })?;
    let parsed: Value = serde_json::from_slice(&body).map_err(|error| {
        json_status(
            StatusCode::BAD_GATEWAY,
            json!({"error": format!("{provider} model catalog is not JSON: {error}"), "status": "error"}),
        )
    })?;
    let ids = model_ids_from_catalog(&parsed);
    if ids.is_empty() {
        return Err(json_status(
            StatusCode::BAD_GATEWAY,
            json!({"error": format!("{provider} returned an empty model catalog"), "status": "error"}),
        ));
    }
    Ok(ids)
}

async fn command_code_import(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let Some(api_key) = body
        .get("api_key")
        .or_else(|| body.get("apiKey"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return json_status(StatusCode::BAD_REQUEST, json!({"error":"api_key required"}));
    };
    let label = body
        .get("label")
        .or_else(|| body.get("userName"))
        .and_then(Value::as_str)
        .unwrap_or("Command Code");
    let base_url = std::env::var("MAHOQUOT_COMMAND_CODE_BASE_URL")
        .unwrap_or_else(|_| "https://api.commandcode.ai/provider/v1".to_string());
    // Plan D9 (AUTH-7): verify the key against the upstream once before it
    // can become a stored account file.
    let verification = state
        .http_client
        .get(format!("{base_url}/models"))
        .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
        .send()
        .await;
    let models: Vec<String> = match verification {
        Ok(response) if response.status().is_success() => {
            match models_from_catalog("command-code", response).await {
                Ok(ids) => ids,
                Err(rejection) => return rejection,
            }
        }
        Ok(response) => {
            return json_status(
                StatusCode::UNAUTHORIZED,
                json!({"error": format!("credential rejected by upstream: {}", response.status()), "status": "error"}),
            )
        }
        Err(error) => {
            return json_status(
                StatusCode::BAD_GATEWAY,
                json!({"error": format!("credential verification unreachable: {error}"), "status": "error"}),
            )
        }
    };
    let credential = json!({"type":"generic","provider":"command-code","label":label,"adapter":"openai-chat",
        "base_url":base_url,"api_key":api_key,
        "models":models,"disabled":false});
    let dir = std::path::PathBuf::from(state.settings.current().auth_dir.clone());
    // Name the file after the credential, not the process: a PID-derived name
    // makes a second import in the same process overwrite the first account.
    let path = dir.join(format!(
        "generic-command-code-{}.json",
        crate::request_history::stable_key_identifier(api_key)
    ));
    let rendered = match serde_json::to_string_pretty(&credential) {
        Ok(value) => value,
        Err(error) => {
            return json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error":error.to_string()}),
            )
        }
    };
    if let Err(error) = write_credential_atomically(&path, rendered.as_bytes()) {
        return json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":error.to_string()}),
        );
    }
    if let Err(error) = state.rescan_pool() {
        return json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":error.to_string()}),
        );
    }
    json_status(
        StatusCode::OK,
        json!({"status":"ok","name":path.file_name().and_then(|name|name.to_str())}),
    )
}

async fn trae_import(State(state): State<Arc<AppState>>, raw: bytes::Bytes) -> Response {
    // The desktop app posts this endpoint without a body or content-type;
    // a JSON body only ever carried an optional storage-path override, so
    // parse it leniently instead of letting the extractor 415 the request.
    let body: Value = if raw.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice(&raw) {
            Ok(value) => value,
            Err(error) => {
                return json_status(StatusCode::BAD_REQUEST, json!({"error": error.to_string()}))
            }
        }
    };
    let path = body
        .get("path")
        .and_then(Value::as_str)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default()
                .join("Library/Application Support/Trae/User/globalStorage/storage.json")
        });
    let raw = match std::fs::read_to_string(&path) {
        Ok(value) => value,
        Err(error) => {
            return json_status(StatusCode::NOT_FOUND, json!({"error":error.to_string()}))
        }
    };
    let storage: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(error) => {
            return json_status(StatusCode::BAD_REQUEST, json!({"error":error.to_string()}))
        }
    };
    let auth = storage
        .get("iCubeAuthInfo://icube.cloudide")
        .and_then(|value| {
            if value.is_string() {
                value
                    .as_str()
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            } else {
                Some(value.clone())
            }
        });
    let Some(auth) = auth else {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({"error":"Trae auth record not found"}),
        );
    };
    let Some(token) = auth
        .get("token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({"error":"Trae token missing"}),
        );
    };
    let label = auth
        .get("account")
        .and_then(|v| v.get("email"))
        .and_then(Value::as_str)
        .unwrap_or("Trae");
    let credential = json!({"type":"monitor","provider":"trae","label":label,"token":token,
        "base_url":auth.get("host").and_then(Value::as_str).unwrap_or("https://api-sg-central.trae.ai"),"disabled":false});
    let dir = std::path::PathBuf::from(state.settings.current().auth_dir.clone());
    let target = dir.join("monitor-trae.json");
    let rendered = serde_json::to_string_pretty(&credential).unwrap_or_default();
    if let Err(error) = write_credential_atomically(&target, rendered.as_bytes()) {
        return json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":error.to_string()}),
        );
    }
    json_status(
        StatusCode::OK,
        json!({"status":"ok","name":"monitor-trae.json"}),
    )
}

async fn discover_provider_models(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let base_url = match body.get("base_url").and_then(Value::as_str) {
        Some(u) if !u.trim().is_empty() => u.trim().trim_end_matches('/'),
        _ => return json_status(StatusCode::BAD_REQUEST, json!({"error": "base_url is required"})),
    };
    let api_key = body.get("api_key").and_then(Value::as_str).unwrap_or("").trim();
    let target_url = if base_url.ends_with("/v1") {
        format!("{base_url}/models")
    } else {
        format!("{base_url}/v1/models")
    };
    let mut req = state.http_client.get(&target_url);
    if !api_key.is_empty() {
        req = req.header(header::AUTHORIZATION, format!("Bearer {api_key}"));
    }
    if let Some(headers) = body.get("static_headers").and_then(Value::as_object) {
        for (k, v) in headers {
            if let Some(v_str) = v.as_str() {
                if let (Ok(name), Ok(val)) = (
                    header::HeaderName::from_bytes(k.as_bytes()),
                    header::HeaderValue::from_str(v_str),
                ) {
                    req = req.header(name, val);
                }
            }
        }
    }
    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            let Ok(json) = resp.json::<Value>().await else {
                return json_status(
                    StatusCode::BAD_GATEWAY,
                    json!({"error": "invalid json from upstream models endpoint"}),
                );
            };
            let mut models = Vec::new();
            if let Some(data) = json.get("data").and_then(Value::as_array) {
                for item in data {
                    if let Some(id) = item.get("id").and_then(Value::as_str) {
                        models.push(id.to_string());
                    }
                }
            } else if let Some(items) = json.get("models").and_then(Value::as_array) {
                for item in items {
                    if let Some(id) = item.as_str() {
                        models.push(id.to_string());
                    } else if let Some(id) = item.get("id").and_then(Value::as_str) {
                        models.push(id.to_string());
                    }
                }
            }
            models.sort();
            models.dedup();
            json_status(StatusCode::OK, json!({ "models": models }))
        }
        Ok(resp) => json_status(
            StatusCode::BAD_GATEWAY,
            json!({ "error": format!("upstream returned status {}", resp.status()) }),
        ),
        Err(err) => json_status(
            StatusCode::BAD_GATEWAY,
            json!({ "error": format!("failed to reach upstream: {err}") }),
        ),
    }
}

pub fn creds_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/auth-files",
            get(list_auth_files)
                .post(create_auth_file)
                .delete(delete_auth_file),
        )
        .route("/auth-files/order", put(save_auth_file_order))
        .route("/auth-files/models", get(auth_file_models))
        .route("/auth-files/download", get(download_auth_file))
        .route(
            "/auth-files/status",
            axum::routing::patch(patch_auth_file_status),
        )
        .route(
            "/auth-files/fields",
            axum::routing::patch(patch_auth_file_fields),
        )
        .route("/model-definitions/{channel}", get(model_definitions))
        .route("/vertex/import", post(vertex_import))
        .route("/cline/import", post(cline_import))
        .route("/devin/import-cli", post(devin_import_cli))
        .route("/command-code/import", post(command_code_import))
        .route("/trae/import-local", post(trae_import))
        .route("/provider-models/discover", post(discover_provider_models))
        .route(
            "/auth-files/delete",
            post(delete_auth_file).delete(delete_auth_file),
        )
        .merge(super::oauth::oauth_routes())
        .route("/oauth-session", delete(super::oauth::cancel_session))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_account_order_precedes_unlisted_files() {
        let mut files = vec![
            json!({"name":"b.json"}),
            json!({"name":"a.json"}),
            json!({"name":"c.json"}),
        ];
        sort_described_files(&mut files, &["c.json".into(), "a.json".into()]);
        assert_eq!(
            files
                .iter()
                .map(|file| file["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["c.json", "a.json", "b.json"]
        );
    }

    #[test]
    fn account_order_manifest_is_not_a_credential() {
        assert!(!is_credential_filename(ACCOUNT_ORDER_FILE));
        assert!(is_credential_filename("claude-local.json"));
    }

    #[test]
    fn a_credential_listing_reports_type_and_email_from_the_file() {
        // given a credential file on disk
        let dir = std::env::temp_dir().join(format!("mahoquot-creds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("acct.json"),
            r#"{"type":"codex","email":"a@b.c","project_id":"p1"}"#,
        )
        .expect("write");
        // when described
        let described = describe(&dir, "acct.json").expect("described");
        // then the loader-visible fields are surfaced
        assert_eq!(described["type"], "codex");
        assert_eq!(described["email"], "a@b.c");
        assert_eq!(described["project_id"], "p1");
        assert_eq!(described["name"], "acct.json");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn writing_a_credential_leaves_no_temp_file() {
        // given a credential written atomically
        let dir = std::env::temp_dir().join(format!("mahoquot-creds-w-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        write_credential_atomically(&dir.join("x.json"), b"{}").expect("writes");
        // when the directory is listed
        let names: Vec<_> = std::fs::read_dir(&dir)
            .expect("readable")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        // then only the final file exists, so the loader never sees a partial
        assert_eq!(names, vec!["x.json".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn provider_imports_reject_credentials_the_loader_cannot_use() {
        let invalid_kiro = json!({
            "type": "kiro",
            "auth_mode": "idc",
            "access_token": "a",
            "refresh_token": "r",
            "email": "u@example.com",
            "expired": "2099-01-01T00:00:00Z",
            "client_id": "client"
        });
        assert_eq!(
            validate_provider_credential(&invalid_kiro),
            Err("credential field client_secret is required".to_string())
        );

        let invalid_zcode = json!({
            "type": "zcode",
            "access_token": "oauth-token-not-api-key",
            "refresh_token": "r",
            "email": "u@example.com",
            "expired": "2099-01-01T00:00:00Z"
        });
        assert_eq!(
            validate_provider_credential(&invalid_zcode),
            Err("zcode access_token must be a provisioned {id}.{secret} key".to_string())
        );
    }

    #[test]
    fn claude_relay_documents_skip_the_oauth_quartet_but_stay_pinned() {
        // given the claude-type document the add-screen builds for relay targets
        let relay_doc = json!({
            "type": "claude",
            "email": "claude-nekos",
            "identity_slug": "claude-nekos",
            "api_key": "sk-clb-secret",
            "upstream_override": "https://claude.nekos.me",
            "plan": "standard"
        });
        // then it is accepted without access_token/refresh_token/expired
        assert_eq!(validate_provider_credential(&relay_doc), Ok(()));

        // and a relay doc without a pinned target API is still rejected
        let unpinned = json!({
            "type": "claude",
            "email": "claude-nekos",
            "api_key": "sk-clb-secret"
        });
        assert_eq!(
            validate_provider_credential(&unpinned),
            Err("credential field upstream_override is required".to_string())
        );

        // and an oauth claude doc without tokens is still rejected as before
        let oauth_missing_tokens = json!({
            "type": "claude",
            "email": "u@example.com",
            "expired": "2099-01-01T00:00:00Z"
        });
        assert_eq!(
            validate_provider_credential(&oauth_missing_tokens),
            Err("credential field access_token is required".to_string())
        );
    }

    #[test]
    fn zcode_accepts_a_pasted_provisioned_key_without_oauth_fields() {
        // The console can only ever supply these two fields for Z.ai, because a
        // provisioned key has no refresh token and no expiry.
        assert_eq!(
            validate_provider_credential(&json!({
                "type": "zcode",
                "access_token": "keyid.keysecret",
                "email": "u@example.com"
            })),
            Ok(())
        );
    }

    #[test]
    fn provider_imports_accept_reference_credential_shapes() {
        for credential in [
            json!({
                "type": "anthropic", "access_token": "a", "refresh_token": "r",
                "email": "u@example.com", "expired": "2099-01-01T00:00:00Z"
            }),
            json!({
                "type": "claude", "access_token": "a", "refresh_token": "r",
                "email": "u@example.com", "expired": "2099-01-01T00:00:00Z"
            }),
            json!({
                "type": "cursor", "access_token": "a", "refresh_token": "r",
                "email": "u@example.com", "expired": "2099-01-01T00:00:00Z"
            }),
            json!({
                "type": "kiro", "auth_mode": "social", "access_token": "a",
                "refresh_token": "r", "email": "u@example.com",
                "expired": "2099-01-01T00:00:00Z"
            }),
            json!({
                "type": "kiro", "auth_mode": "idc", "access_token": "a",
                "refresh_token": "r", "email": "u@example.com",
                "expired": "2099-01-01T00:00:00Z", "client_id": "c",
                "client_secret": "s"
            }),
            json!({
                "type": "zcode", "access_token": "id.secret", "refresh_token": "r",
                "email": "u@example.com", "expired": "2099-01-01T00:00:00Z"
            }),
        ] {
            validate_provider_credential(&credential).expect("reference shape accepted");
        }
    }
}

#[cfg(test)]
mod reserved_file_tests {
    use super::*;

    #[tokio::test]
    async fn discover_provider_models_requires_base_url() {
        let root = std::env::temp_dir().join(format!("qgw-test-disc-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let config = crate::config::GatewayConfig {
            auth_dir: root.clone(),
            config_path: root.join("config.yaml"),
            ..Default::default()
        };
        let state = Arc::new(AppState::new(&config).unwrap());
        let resp = discover_provider_models(State(state), Json(json!({}))).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn reserved_data_files_are_not_listed_as_credentials() {
        assert!(!is_credential_filename("telemetry.json"));
        assert!(!is_credential_filename("TELEMETRY.JSON"));
        assert!(!is_credential_filename("config.yaml"));
        assert!(is_credential_filename("codex-1.json"));
        assert!(!is_credential_filename(".mahoquot-account-order.json"));
    }

    #[test]
    fn catalog_ids_are_read_from_the_provider_payload() {
        let payload = serde_json::json!({
            "object": "list",
            "data": [
                {"id": "z-ai/glm-5.3-flash", "object": "model"},
                {"id": "  deepseek/deepseek-v4.1-flash  ", "object": "model"},
                {"id": "upstage/solar-pro4"}
            ]
        });
        assert_eq!(
            model_ids_from_catalog(&payload),
            vec![
                "z-ai/glm-5.3-flash".to_string(),
                "deepseek/deepseek-v4.1-flash".to_string(),
                "upstage/solar-pro4".to_string(),
            ]
        );
    }

    #[test]
    fn catalog_parsing_drops_unusable_entries() {
        // Blank and non-string ids carry no routing meaning, and an entry
        // without an `id` is not a model at all.
        let payload = serde_json::json!({
            "data": [
                {"id": ""},
                {"id": "   "},
                {"id": 42},
                {"object": "model"},
                {"id": "kept/model"}
            ]
        });
        assert_eq!(
            model_ids_from_catalog(&payload),
            vec!["kept/model".to_string()]
        );
    }

    #[test]
    fn catalog_parsing_yields_nothing_for_malformed_payloads() {
        // An empty result is what makes the callers reject the credential
        // instead of silently storing a guessed model list, so the shapes that
        // must produce it are pinned here.
        for payload in [
            serde_json::json!({}),
            serde_json::json!({"data": []}),
            serde_json::json!({"data": "not-an-array"}),
            serde_json::json!({"models": [{"id": "wrong-key"}]}),
        ] {
            assert!(
                model_ids_from_catalog(&payload).is_empty(),
                "expected no ids from {payload}"
            );
        }
    }
}
