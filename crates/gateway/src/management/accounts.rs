use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use mahoquot_providers::credential_file::write_credential_atomically;
use serde_json::{json, Value};

use crate::management::settings::ApiKeyBinding;
use crate::state::AppState;

use super::creds::{credential_identity, validate_provider_credential};

const EXPORT_AUTHORIZATION_HEADER: &str = "x-mahoquot-export-authorization";
const GENERIC_ADAPTERS: &[&str] = &[
    "openai-chat",
    "openai-responses",
    "azure-openai",
    "anthropic",
    "google",
    "mimo-free",
];

fn json_status(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

fn typed_error(status: StatusCode, code: &str, message: impl ToString) -> Response {
    json_status(
        status,
        json!({ "error": { "code": code, "message": message.to_string() } }),
    )
}

fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && !name.contains('/') && !name.contains("..")
}

fn auth_dir(state: &AppState) -> std::path::PathBuf {
    std::path::PathBuf::from(state.settings.current().auth_dir.clone())
}

type ResponseResult<T> = Result<T, Box<Response>>;

fn boxed_error(status: StatusCode, code: &str, message: impl ToString) -> Box<Response> {
    Box::new(typed_error(status, code, message))
}

fn read_json(path: &std::path::Path) -> ResponseResult<Value> {
    let raw = std::fs::read_to_string(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            boxed_error(StatusCode::NOT_FOUND, "not_found", "credential not found")
        } else {
            boxed_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "credential_read_failed",
                error,
            )
        }
    })?;
    serde_json::from_str(&raw)
        .map_err(|error| boxed_error(StatusCode::BAD_REQUEST, "credential_invalid", error))
}

fn write_json(path: &std::path::Path, value: &Value) -> ResponseResult<()> {
    let rendered = serde_json::to_vec_pretty(value).map_err(|error| {
        boxed_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "credential_encode_failed",
            error,
        )
    })?;
    write_credential_atomically(path, &rendered).map_err(|error| {
        boxed_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "credential_write_failed",
            error,
        )
    })
}

fn rescan(state: &AppState) -> ResponseResult<()> {
    state.rescan_pool().map(|_| ()).map_err(|error| {
        boxed_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "pool_rescan_failed",
            error,
        )
    })
}

fn provider_files(dir: &std::path::Path, provider_id: &str) -> Vec<(String, Value)> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.to_ascii_lowercase().ends_with(".json") {
            continue;
        }
        let path = entry.path();
        let Ok(raw) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("generic")
            && value.get("provider").and_then(Value::as_str) == Some(provider_id)
        {
            files.push((name, value));
        }
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

fn key_id(value: &Value, fallback: &str) -> String {
    value
        .get("management_key_id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .unwrap_or(fallback)
        .to_string()
}

fn provider_view(provider_id: &str, files: &[(String, Value)]) -> Value {
    let first = files.first().map(|(_, value)| value);
    let accounts: Vec<Value> = files
        .iter()
        .map(|(name, value)| {
            json!({
                "account": value
                    .get("identity_slug")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| name.trim_end_matches(".json")),
                "credential_name": name,
                "enabled": !value.get("disabled").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect();
    let keys: Vec<Value> = files
        .iter()
        .map(|(name, value)| {
            json!({
                "id": key_id(value, name),
                "label": value.get("management_key_label").and_then(Value::as_str).unwrap_or(""),
                "enabled": !value.get("disabled").and_then(Value::as_bool).unwrap_or(false),
                "account": value
                    .get("identity_slug")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| name.trim_end_matches(".json")),
            })
        })
        .collect();
    json!({
        "id": provider_id,
        "name": first
            .and_then(|value| value.get("management_provider_name"))
            .and_then(Value::as_str)
            .unwrap_or(provider_id),
        "base_url": first
            .and_then(|value| value.get("base_url"))
            .and_then(Value::as_str)
            .unwrap_or(""),
        "adapter": first
            .and_then(|value| value.get("adapter"))
            .and_then(Value::as_str)
            .unwrap_or("openai-chat"),
        "models": first
            .and_then(|value| value.get("models"))
            .cloned()
            .unwrap_or_else(|| json!([])),
        "accounts": accounts,
        "keys": keys,
    })
}

fn provider_ids(dir: &std::path::Path) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return ids;
    };
    for entry in entries.flatten() {
        let Ok(raw) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("generic") {
            if let Some(provider) = value.get("provider").and_then(Value::as_str) {
                ids.insert(provider.to_string());
            }
        }
    }
    ids
}

async fn list_providers(State(state): State<Arc<AppState>>) -> Response {
    let dir = auth_dir(&state);
    let providers: Vec<Value> = provider_ids(&dir)
        .into_iter()
        .map(|id| provider_view(&id, &provider_files(&dir, &id)))
        .collect();
    json_status(StatusCode::OK, json!({ "providers": providers }))
}

fn required_body_string<'a>(body: &'a Value, field: &str) -> ResponseResult<&'a str> {
    body.get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            boxed_error(
                StatusCode::BAD_REQUEST,
                "invalid_provider",
                format!("{field} is required"),
            )
        })
}

fn provider_fields(body: &Value) -> ResponseResult<(String, String, String, String, Vec<String>)> {
    let id = required_body_string(body, "id")?.to_string();
    let name = required_body_string(body, "name")?.to_string();
    let base_url = required_body_string(body, "base_url")?.to_string();
    let adapter = required_body_string(body, "adapter")?.to_string();
    if !GENERIC_ADAPTERS.contains(&adapter.as_str()) {
        return Err(boxed_error(
            StatusCode::BAD_REQUEST,
            "invalid_provider",
            "unsupported provider adapter",
        ));
    }
    let models = body
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            boxed_error(
                StatusCode::BAD_REQUEST,
                "invalid_provider",
                "models is required",
            )
        })?
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if !valid_name(&id) || models.is_empty() {
        return Err(boxed_error(
            StatusCode::BAD_REQUEST,
            "invalid_provider",
            "provider id and models must be valid",
        ));
    }
    Ok((id, name, base_url, adapter, models))
}

fn provider_credential(
    provider_id: &str,
    provider_name: &str,
    base_url: &str,
    adapter: &str,
    models: &[String],
    api_key: &str,
    label: &str,
) -> (String, Value) {
    let key_id = uuid::Uuid::new_v4().to_string();
    let account = format!("{provider_id}-{key_id}");
    let name = format!("generic-{provider_id}-{key_id}.json");
    (
        name,
        json!({
            "type": "generic",
            "provider": provider_id,
            "management_provider_name": provider_name,
            "management_key_id": key_id,
            "management_key_label": label,
            "identity_slug": account,
            "label": if label.is_empty() { provider_name } else { label },
            "adapter": adapter,
            "base_url": base_url,
            "api_key": api_key,
            "models": models,
            "disabled": false,
        }),
    )
}

async fn create_provider(State(state): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let (id, name, base_url, adapter, models) = match provider_fields(&body) {
        Ok(fields) => fields,
        Err(response) => return *response,
    };
    let api_key = match required_body_string(&body, "api_key") {
        Ok(value) => value,
        Err(_) => {
            return typed_error(
                StatusCode::BAD_REQUEST,
                "invalid_provider",
                "api_key is required",
            )
        }
    };
    let dir = auth_dir(&state);
    if !provider_files(&dir, &id).is_empty() {
        return typed_error(
            StatusCode::CONFLICT,
            "provider_exists",
            "provider already exists",
        );
    }
    if let Err(error) = std::fs::create_dir_all(&dir) {
        return typed_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "credential_write_failed",
            error,
        );
    }
    let label = body.get("key_label").and_then(Value::as_str).unwrap_or("");
    let (filename, credential) =
        provider_credential(&id, &name, &base_url, &adapter, &models, api_key, label);
    if let Err(response) = write_json(&dir.join(filename), &credential) {
        return *response;
    }
    if let Err(response) = rescan(&state) {
        return *response;
    }
    json_status(
        StatusCode::CREATED,
        json!({ "status": "ok", "provider": provider_view(&id, &provider_files(&dir, &id)) }),
    )
}

async fn update_provider(
    State(state): State<Arc<AppState>>,
    Path(provider_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let dir = auth_dir(&state);
    let files = provider_files(&dir, &provider_id);
    if files.is_empty() {
        return typed_error(
            StatusCode::NOT_FOUND,
            "provider_not_found",
            "provider not found",
        );
    }
    let name = match required_body_string(&body, "name") {
        Ok(value) => value.to_string(),
        Err(response) => return *response,
    };
    let base_url = match required_body_string(&body, "base_url") {
        Ok(value) => value.to_string(),
        Err(response) => return *response,
    };
    let adapter = match required_body_string(&body, "adapter") {
        Ok(value) if GENERIC_ADAPTERS.contains(&value) => value.to_string(),
        _ => {
            return typed_error(
                StatusCode::BAD_REQUEST,
                "invalid_provider",
                "unsupported provider adapter",
            )
        }
    };
    let models = body
        .get("models")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|models| !models.is_empty())
        .ok_or_else(|| {
            typed_error(
                StatusCode::BAD_REQUEST,
                "invalid_provider",
                "models is required",
            )
        });
    let models = match models {
        Ok(models) => models,
        Err(response) => return response,
    };
    for (filename, mut value) in files {
        value["management_provider_name"] = json!(name);
        value["base_url"] = json!(base_url);
        value["adapter"] = json!(adapter);
        value["models"] = json!(models);
        if let Err(response) = write_json(&dir.join(filename), &value) {
            return *response;
        }
    }
    if let Err(response) = rescan(&state) {
        return *response;
    }
    json_status(
        StatusCode::OK,
        json!({ "status": "ok", "provider": provider_view(&provider_id, &provider_files(&dir, &provider_id)) }),
    )
}

async fn delete_provider(
    State(state): State<Arc<AppState>>,
    Path(provider_id): Path<String>,
) -> Response {
    let dir = auth_dir(&state);
    let files = provider_files(&dir, &provider_id);
    if files.is_empty() {
        return typed_error(
            StatusCode::NOT_FOUND,
            "provider_not_found",
            "provider not found",
        );
    }
    for (filename, _) in files {
        if let Err(error) = std::fs::remove_file(dir.join(filename)) {
            return typed_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "credential_delete_failed",
                error,
            );
        }
    }
    if let Err(response) = rescan(&state) {
        return *response;
    }
    json_status(StatusCode::OK, json!({ "status": "ok" }))
}

async fn add_provider_key(
    State(state): State<Arc<AppState>>,
    Path(provider_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let dir = auth_dir(&state);
    let files = provider_files(&dir, &provider_id);
    let Some((_, first)) = files.first() else {
        return typed_error(
            StatusCode::NOT_FOUND,
            "provider_not_found",
            "provider not found",
        );
    };
    let api_key = match required_body_string(&body, "api_key") {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let label = body.get("label").and_then(Value::as_str).unwrap_or("");
    let models: Vec<String> = first
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    let (filename, credential) = provider_credential(
        &provider_id,
        first
            .get("management_provider_name")
            .and_then(Value::as_str)
            .unwrap_or(&provider_id),
        first.get("base_url").and_then(Value::as_str).unwrap_or(""),
        first
            .get("adapter")
            .and_then(Value::as_str)
            .unwrap_or("openai-chat"),
        &models,
        api_key,
        label,
    );
    if let Err(response) = write_json(&dir.join(filename), &credential) {
        return *response;
    }
    if let Err(response) = rescan(&state) {
        return *response;
    }
    json_status(
        StatusCode::CREATED,
        json!({ "status": "ok", "provider": provider_view(&provider_id, &provider_files(&dir, &provider_id)) }),
    )
}

fn provider_key_file(
    dir: &std::path::Path,
    provider_id: &str,
    wanted: &str,
) -> Option<(String, Value)> {
    provider_files(dir, provider_id)
        .into_iter()
        .find(|(name, value)| key_id(value, name) == wanted)
}

async fn patch_provider_key(
    State(state): State<Arc<AppState>>,
    Path((provider_id, key_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Response {
    let Some(enabled) = body.get("enabled").and_then(Value::as_bool) else {
        return typed_error(
            StatusCode::BAD_REQUEST,
            "invalid_body",
            "enabled is required",
        );
    };
    let dir = auth_dir(&state);
    let Some((filename, mut value)) = provider_key_file(&dir, &provider_id, &key_id) else {
        return typed_error(
            StatusCode::NOT_FOUND,
            "provider_key_not_found",
            "provider key not found",
        );
    };
    value["disabled"] = json!(!enabled);
    if let Err(response) = write_json(&dir.join(filename), &value) {
        return *response;
    }
    if let Err(response) = rescan(&state) {
        return *response;
    }
    json_status(
        StatusCode::OK,
        json!({ "status": "ok", "provider": provider_view(&provider_id, &provider_files(&dir, &provider_id)) }),
    )
}

async fn delete_provider_key(
    State(state): State<Arc<AppState>>,
    Path((provider_id, key_id)): Path<(String, String)>,
) -> Response {
    let dir = auth_dir(&state);
    let Some((filename, _)) = provider_key_file(&dir, &provider_id, &key_id) else {
        return typed_error(
            StatusCode::NOT_FOUND,
            "provider_key_not_found",
            "provider key not found",
        );
    };
    if let Err(error) = std::fs::remove_file(dir.join(filename)) {
        return typed_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "credential_delete_failed",
            error,
        );
    }
    if let Err(response) = rescan(&state) {
        return *response;
    }
    json_status(
        StatusCode::OK,
        json!({ "status": "ok", "provider": provider_view(&provider_id, &provider_files(&dir, &provider_id)) }),
    )
}

fn existing_identities(dir: &std::path::Path) -> BTreeMap<String, String> {
    let mut identities = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return identities;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.to_ascii_lowercase().ends_with(".json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if let Some(identity) = credential_identity(&value) {
            identities.insert(identity, name);
        }
    }
    identities
}

async fn import_auth_files(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let Some(files) = body.get("files").and_then(Value::as_array) else {
        return typed_error(
            StatusCode::BAD_REQUEST,
            "invalid_import",
            "files is required",
        );
    };
    if files.is_empty() {
        return typed_error(
            StatusCode::BAD_REQUEST,
            "invalid_import",
            "files cannot be empty",
        );
    }
    let dir = auth_dir(&state);
    if let Err(error) = std::fs::create_dir_all(&dir) {
        return typed_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "credential_write_failed",
            error,
        );
    }
    let mut existing = existing_identities(&dir);
    let mut pending = Vec::new();
    for file in files {
        let Some(name) = file.get("name").and_then(Value::as_str).map(str::trim) else {
            return typed_error(
                StatusCode::BAD_REQUEST,
                "invalid_import",
                "file name is required",
            );
        };
        if !valid_name(name) || !name.to_ascii_lowercase().ends_with(".json") {
            return typed_error(
                StatusCode::BAD_REQUEST,
                "invalid_import",
                "invalid credential name",
            );
        }
        let content = file.get("content").cloned().unwrap_or(Value::Null);
        if let Err(error) = validate_provider_credential(&content) {
            return typed_error(StatusCode::BAD_REQUEST, "invalid_credential", error);
        }
        if dir.join(name).exists() {
            return typed_error(
                StatusCode::CONFLICT,
                "duplicate_credential",
                "credential name already exists",
            );
        }
        if let Some(identity) = credential_identity(&content) {
            if existing.contains_key(&identity) {
                return typed_error(
                    StatusCode::CONFLICT,
                    "duplicate_credential",
                    "credential identity already exists",
                );
            }
            existing.insert(identity, name.to_string());
        }
        pending.push((name.to_string(), content));
    }
    let mut written = Vec::new();
    for (name, content) in &pending {
        if let Err(response) = write_json(&dir.join(name), content) {
            for previous in written {
                let _ = std::fs::remove_file(dir.join(previous));
            }
            return *response;
        }
        written.push(name.clone());
    }
    if let Err(response) = rescan(&state) {
        return *response;
    }
    json_status(
        StatusCode::CREATED,
        json!({ "status": "ok", "names": written }),
    )
}

pub(crate) fn export_authorized(state: &AppState, headers: &HeaderMap) -> bool {
    let expected = state
        .settings
        .current()
        .remote_management
        .secret_key
        .clone();
    !expected.is_empty()
        && headers
            .get(EXPORT_AUTHORIZATION_HEADER)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|presented| presented == expected)
}

pub(crate) fn export_refusal() -> Response {
    typed_error(
        StatusCode::FORBIDDEN,
        "export_unauthorized",
        "explicit export authorization is required",
    )
}

async fn export_auth_files(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !export_authorized(&state, &headers) {
        return export_refusal();
    }
    let names: Option<BTreeSet<String>> =
        body.get("names").and_then(Value::as_array).map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        });
    let dir = auth_dir(&state);
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return json_status(StatusCode::OK, json!({ "files": files }));
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.to_ascii_lowercase().ends_with(".json")
            || names
                .as_ref()
                .is_some_and(|selected| !selected.contains(&name))
        {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(content) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        files.push(json!({ "name": name, "content": content }));
    }
    files.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    json_status(StatusCode::OK, json!({ "files": files }))
}

async fn bulk_auth_files(State(state): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let Some(action) = body.get("action").and_then(Value::as_str) else {
        return typed_error(
            StatusCode::BAD_REQUEST,
            "invalid_bulk_action",
            "action is required",
        );
    };
    let Some(names) = body.get("names").and_then(Value::as_array) else {
        return typed_error(
            StatusCode::BAD_REQUEST,
            "invalid_bulk_action",
            "names is required",
        );
    };
    let disabled = match action {
        "disable" => true,
        "enable" => false,
        _ => {
            return typed_error(
                StatusCode::BAD_REQUEST,
                "invalid_bulk_action",
                "unsupported action",
            )
        }
    };
    let dir = auth_dir(&state);
    let mut changes = Vec::new();
    for name in names.iter().filter_map(Value::as_str) {
        if !valid_name(name) {
            return typed_error(
                StatusCode::BAD_REQUEST,
                "invalid_bulk_action",
                "invalid credential name",
            );
        }
        let path = dir.join(name);
        let mut value = match read_json(&path) {
            Ok(value) => value,
            Err(response) => return *response,
        };
        value["disabled"] = json!(disabled);
        changes.push((path, value));
    }
    for (path, value) in &changes {
        if let Err(response) = write_json(path, value) {
            return *response;
        }
    }
    if let Err(response) = rescan(&state) {
        return *response;
    }
    json_status(
        StatusCode::OK,
        json!({ "status": "ok", "changed": changes.len() }),
    )
}

fn binding_view(binding: &ApiKeyBinding) -> Value {
    json!({
        "key_identifier": binding.key_identifier,
        "account": binding.account,
        "provider": binding.provider,
    })
}

async fn list_bindings(State(state): State<Arc<AppState>>) -> Response {
    let bindings: Vec<Value> = state
        .settings
        .current()
        .api_key_bindings
        .iter()
        .map(binding_view)
        .collect();
    json_status(StatusCode::OK, json!({ "bindings": bindings }))
}

async fn put_binding(State(state): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let Some(api_key) = body.get("api_key").and_then(Value::as_str).map(str::trim) else {
        return typed_error(
            StatusCode::BAD_REQUEST,
            "binding_invalid",
            "api_key is required",
        );
    };
    if !state.api_keys.accepts(api_key) {
        return typed_error(
            StatusCode::BAD_REQUEST,
            "binding_key_invalid",
            "unknown inbound api key",
        );
    }
    let account = body
        .get("account")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let provider = body
        .get("provider")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if account.is_none() && provider.is_none() {
        return typed_error(
            StatusCode::BAD_REQUEST,
            "binding_target_invalid",
            "account or provider is required",
        );
    }
    if account
        .as_deref()
        .is_some_and(|wanted| state.find_member(wanted).is_none())
        || provider.as_deref().is_some_and(|wanted| {
            !state
                .pool
                .load()
                .members
                .iter()
                .any(|member| member.provider_name() == wanted)
        })
    {
        return typed_error(
            StatusCode::BAD_REQUEST,
            "binding_target_invalid",
            "binding target does not exist",
        );
    }
    let key_identifier = crate::request_history::stable_key_identifier(api_key);
    let binding = ApiKeyBinding {
        key_identifier: key_identifier.clone(),
        account,
        provider,
    };
    let saved = state.settings.mutate(|settings| {
        settings
            .api_key_bindings
            .retain(|existing| existing.key_identifier != key_identifier);
        settings.api_key_bindings.push(binding.clone());
    });
    if let Err(error) = saved {
        return typed_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "binding_save_failed",
            error,
        );
    }
    json_status(
        StatusCode::OK,
        json!({ "status": "ok", "binding": binding_view(&binding) }),
    )
}

pub fn binding_for_key(state: &AppState, raw_key: Option<&str>) -> Option<ApiKeyBinding> {
    let identifier = raw_key.map(crate::request_history::stable_key_identifier)?;
    state
        .settings
        .current()
        .api_key_bindings
        .iter()
        .find(|binding| binding.key_identifier == identifier)
        .cloned()
}

pub fn account_management_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/providers", get(list_providers).post(create_provider))
        .route(
            "/providers/{provider_id}",
            put(update_provider).delete(delete_provider),
        )
        .route("/providers/{provider_id}/keys", post(add_provider_key))
        .route(
            "/providers/{provider_id}/keys/{key_id}",
            axum::routing::patch(patch_provider_key).delete(delete_provider_key),
        )
        .route("/auth-files/import", post(import_auth_files))
        .route("/auth-files/export", post(export_auth_files))
        .route("/auth-files/bulk", post(bulk_auth_files))
        .route("/api-key-bindings", get(list_bindings).put(put_binding))
}
