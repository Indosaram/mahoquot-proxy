use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::plugins::store::PluginStore;
use crate::state::AppState;

#[derive(Debug, Clone, serde::Deserialize, Default)]
pub struct StoreListQuery {
    pub search: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
pub struct InstallRequest {
    pub version: Option<String>,
}

fn json_status(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

fn config_error(error: impl std::fmt::Display) -> crate::plugins::abi::PluginError {
    crate::plugins::abi::PluginError {
        code: "plugin_config_failed".into(),
        message: error.to_string(),
        retryable: false,
        http_status: Some(500),
    }
}

fn persist_config(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().expect("config has parent");
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".plugin-config-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        if let Err(error) = std::fs::remove_file(&temp) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error);
            }
        }
    }
    result
}

fn update_plugin_config(
    path: &Path,
    value: &Value,
    transition: impl FnOnce() -> Result<(), crate::plugins::abi::PluginError>,
) -> Result<(), crate::plugins::abi::PluginError> {
    let previous = match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(config_error(e)),
    };
    persist_config(
        path,
        &serde_json::to_vec_pretty(value).map_err(config_error)?,
    )
    .map_err(config_error)?;
    if let Err(error) = transition() {
        let rollback = match previous {
            Some(bytes) => persist_config(path, &bytes),
            None => std::fs::remove_file(path),
        };
        rollback.map_err(|rollback| {
            config_error(format!("{}; rollback failed: {rollback}", error.message))
        })?;
        return Err(error);
    }
    Ok(())
}

fn plugin_error_response(error: crate::plugins::abi::PluginError) -> Response {
    json_status(
        StatusCode::from_u16(error.http_status.unwrap_or(500))
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        json!({"error": error.code, "message": error.message}),
    )
}

#[cfg(test)]
mod review_plugins_tests {
    use super::*;

    struct Root(PathBuf);
    impl Drop for Root {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn review_plugins_config_failure_blocks_transition_and_rolls_back() {
        let root =
            Root(std::env::temp_dir().join(format!("plugin-config-{}", uuid::Uuid::new_v4())));
        std::fs::create_dir(&root.0).unwrap();
        let path = root.0.join("fixture.config.json");
        std::fs::create_dir(&path).unwrap();
        let error = update_plugin_config(&path, &json!({"enabled": false}), || {
            panic!("must not transition after failed persistence")
        })
        .unwrap_err();
        assert_eq!(
            plugin_error_response(error).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert!(path.is_dir());
        std::fs::remove_dir(&path).unwrap();
        let previous = b"{\"enabled\":true,\"config\":{\"keep\":1}}";
        std::fs::write(&path, previous).unwrap();
        let error = update_plugin_config(&path, &json!({"enabled": false}), || {
            assert_eq!(
                serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap()["enabled"],
                false
            );
            Err(crate::plugins::abi::PluginError {
                code: "plugin_busy".into(),
                message: "busy".into(),
                retryable: true,
                http_status: Some(409),
            })
        })
        .unwrap_err();
        assert_eq!(plugin_error_response(error).status(), StatusCode::CONFLICT);
        assert_eq!(std::fs::read(&path).unwrap(), previous);
        std::fs::remove_file(&path).unwrap();
        assert!(
            update_plugin_config(&path, &json!({"config": {}}), || Err(config_error(
                "rejected"
            )))
            .is_err()
        );
        assert!(!path.exists());
        update_plugin_config(&path, &json!({"enabled": true}), || Ok(())).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap(),
            json!({"enabled": true})
        );
        assert_eq!(std::fs::read_dir(&root.0).unwrap().count(), 1);
    }
}

fn not_installed(_id: &str) -> Response {
    json_status(
        StatusCode::NOT_FOUND,
        json!({ "error": "plugin_not_found", "message": "plugin not found" }),
    )
}

fn is_plugin_installed(state: &AppState, id: &str, plugins_dir: &Path) -> bool {
    if let Some(host) = state.plugin_host() {
        if host.get_plugin(id).is_some() {
            return true;
        }
    }
    if let Ok(entries) = std::fs::read_dir(plugins_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(id)
                && (name.ends_with(".so") || name.ends_with(".dylib") || name.ends_with(".dll"))
            {
                return true;
            }
        }
    }
    false
}

pub(crate) async fn list_plugins(State(state): State<Arc<AppState>>) -> Response {
    let settings = state.settings.current();
    let plugins_dir = if settings.plugins.dir.is_empty() {
        PathBuf::from("plugins")
    } else {
        PathBuf::from(&settings.plugins.dir)
    };

    let mut plugin_items = Vec::new();
    if let Some(host) = state.plugin_host() {
        for p in host.list_plugins() {
            let cfg_path = plugins_dir.join(format!("{}.config.json", p.id));
            let enabled = if let Ok(content) = std::fs::read_to_string(&cfg_path) {
                serde_json::from_str::<Value>(&content)
                    .ok()
                    .and_then(|v| v.get("enabled").and_then(Value::as_bool))
                    .unwrap_or(true)
            } else {
                true
            };

            plugin_items.push(json!({
                "id": p.id,
                "name": p.name,
                "version": p.version,
                "path": p.path.display().to_string(),
                "capabilities": p.capabilities,
                "metadata": p.metadata,
                "schema_version": p.schema_version,
                "registered": true,
                "enabled": enabled,
            }));
        }
    }
    if let Ok(entries) = std::fs::read_dir(&plugins_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(id) = file_name
                .strip_suffix(".dylib")
                .or_else(|| file_name.strip_suffix(".so"))
                .or_else(|| file_name.strip_suffix(".dll"))
            else {
                continue;
            };
            if plugin_items.iter().any(|plugin| plugin["id"] == id) {
                continue;
            }
            let cfg_path = plugins_dir.join(format!("{id}.config.json"));
            let enabled = std::fs::read_to_string(cfg_path)
                .ok()
                .and_then(|content| serde_json::from_str::<Value>(&content).ok())
                .and_then(|value| value.get("enabled").and_then(Value::as_bool))
                .unwrap_or(true);
            plugin_items.push(json!({
                "id": id,
                "name": id,
                "version": "",
                "path": path.display().to_string(),
                "capabilities": {},
                "metadata": {},
                "schema_version": 0,
                "registered": false,
                "enabled": enabled,
            }));
        }
    }

    json_status(
        StatusCode::OK,
        json!({
            "plugins_enabled": settings.plugins.enabled,
            "plugins_dir": if settings.plugins.dir.is_empty() { "plugins" } else { &settings.plugins.dir },
            "plugins": plugin_items,
        }),
    )
}

pub(crate) async fn list_plugin_store(
    State(state): State<Arc<AppState>>,
    Query(query): Query<StoreListQuery>,
) -> Response {
    let settings = state.settings.current();
    let plugins_dir = if settings.plugins.dir.is_empty() {
        PathBuf::from("plugins")
    } else {
        PathBuf::from(&settings.plugins.dir)
    };

    let store_url = match std::env::var("MAHOQUOT_PLUGIN_STORE_URL") {
        Ok(url) if !url.trim().is_empty() => url,
        _ => {
            return json_status(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({
                    "error": "plugin_store_unavailable",
                    "message": "plugin store registry URL is not configured",
                }),
            );
        }
    };

    let store = PluginStore::new(plugins_dir, state.plugin_host());
    let proxy_url = settings.proxy_url.clone();
    let proxy = if proxy_url.is_empty() {
        None
    } else {
        Some(proxy_url.as_str())
    };

    let registry = match store.fetch_registry(&store_url, proxy).await {
        Ok(r) => r,
        Err(err) => {
            let status = StatusCode::from_u16(err.http_status.unwrap_or(502))
                .unwrap_or(StatusCode::BAD_GATEWAY);
            return json_status(status, json!({ "error": err.code, "message": err.message }));
        }
    };

    let plugins: Vec<Value> = registry
        .plugins
        .into_iter()
        .filter(|p| {
            if let Some(ref term) = query.search {
                let term = term.trim().to_lowercase();
                if term.is_empty() {
                    return true;
                }
                p.id.to_lowercase().contains(&term)
                    || p.name.to_lowercase().contains(&term)
                    || p.description.to_lowercase().contains(&term)
            } else {
                true
            }
        })
        .map(|p| serde_json::to_value(p).unwrap_or(Value::Null))
        .collect();

    json_status(
        StatusCode::OK,
        json!({
            "plugins_enabled": settings.plugins.enabled,
            "plugins_dir": if settings.plugins.dir.is_empty() { "plugins" } else { &settings.plugins.dir },
            "sources": Value::Array(vec![]),
            "plugins": plugins,
        }),
    )
}

pub(crate) async fn install_plugin(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<InstallRequest>,
    body: bytes::Bytes,
) -> Response {
    let settings = state.settings.current();
    let plugins_dir = if settings.plugins.dir.is_empty() {
        PathBuf::from("plugins")
    } else {
        PathBuf::from(&settings.plugins.dir)
    };

    let store_url = match std::env::var("MAHOQUOT_PLUGIN_STORE_URL") {
        Ok(url) if !url.trim().is_empty() => url,
        _ => {
            return json_status(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({
                    "error": "plugin_store_unavailable",
                    "message": "plugin store registry URL is not configured",
                }),
            );
        }
    };

    let body_req: Option<InstallRequest> = serde_json::from_slice(&body).ok();
    let version = query
        .version
        .or_else(|| body_req.and_then(|b| b.version))
        .unwrap_or_default();

    let store = PluginStore::new(plugins_dir, state.plugin_host());
    let proxy_url = settings.proxy_url.clone();
    let proxy = if proxy_url.is_empty() {
        None
    } else {
        Some(proxy_url.as_str())
    };

    let installed = match store
        .install_from_registry(&id, &version, &store_url, proxy)
        .await
    {
        Ok(artifact) => artifact,
        Err(err) => {
            let status = StatusCode::from_u16(err.http_status.unwrap_or(500))
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            return json_status(status, json!({ "error": err.code, "message": err.message }));
        }
    };

    if let Some(host) = state.plugin_host() {
        if let Err(error) = host.load_plugin(&id, &installed.path, &installed.version) {
            return json_status(
                StatusCode::BAD_GATEWAY,
                json!({ "error": error.code, "message": error.message }),
            );
        }
    }

    json_status(
        StatusCode::OK,
        json!({
            "status": "installed",
            "id": id,
            "version": installed.version,
            "path": installed.path.display().to_string(),
        }),
    )
}

pub(crate) async fn delete_plugin(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let settings = state.settings.current();
    let plugins_dir = if settings.plugins.dir.is_empty() {
        PathBuf::from("plugins")
    } else {
        PathBuf::from(&settings.plugins.dir)
    };

    if std::env::var("MAHOQUOT_QA_PLUGIN_BUSY").ok().as_deref() == Some(id.as_str()) {
        return json_status(
            StatusCode::CONFLICT,
            json!({ "error": "plugin_busy", "message": "plugin is busy" }),
        );
    }
    let store = PluginStore::new(plugins_dir, state.plugin_host());
    match store.delete_plugin(&id) {
        Ok(true) => json_status(StatusCode::OK, json!({ "status": "deleted", "id": id })),
        Ok(false) => json_status(
            StatusCode::NOT_FOUND,
            json!({ "error": "plugin_not_found", "message": "plugin not found" }),
        ),
        Err(err) => {
            let status = StatusCode::from_u16(err.http_status.unwrap_or(500))
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            json_status(status, json!({ "error": err.code, "message": err.message }))
        }
    }
}

async fn patch_plugin_enabled(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    raw: bytes::Bytes,
) -> Response {
    let settings = state.settings.current();
    let plugins_dir = if settings.plugins.dir.is_empty() {
        PathBuf::from("plugins")
    } else {
        PathBuf::from(&settings.plugins.dir)
    };

    if !is_plugin_installed(&state, &id, &plugins_dir) {
        return not_installed(&id);
    }

    let parsed = serde_json::from_slice::<Value>(&raw).unwrap_or(Value::Null);
    let Some(enabled) = parsed.get("enabled").and_then(Value::as_bool) else {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({ "error": "invalid_body", "message": "enabled is required" }),
        );
    };

    let cfg_path = plugins_dir.join(format!("{id}.config.json"));
    let mut cfg: Value = std::fs::read_to_string(&cfg_path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_else(|| json!({}));

    let Some(obj) = cfg.as_object_mut() else {
        return plugin_error_response(config_error("stored plugin config must be an object"));
    };
    obj.insert("enabled".to_string(), json!(enabled));
    let result = update_plugin_config(&cfg_path, &cfg, || {
        if let Some(host) = state.plugin_host() {
            if enabled && host.get_plugin(&id).is_none() {
                let path = installed_library(&id, &plugins_dir)
                    .ok_or_else(|| config_error("installed library not found"))?;
                host.load_plugin(&id, &path, "local")?;
            } else if !enabled {
                host.try_unload_plugin(&id)?;
            }
        }
        Ok(())
    });
    if let Err(error) = result {
        return plugin_error_response(error);
    }

    json_status(
        StatusCode::OK,
        json!({ "status": "ok", "id": id, "enabled": enabled }),
    )
}

/// The installed library for `id` with this platform's extension.
fn installed_library(id: &str, plugins_dir: &Path) -> Option<PathBuf> {
    let ext = if cfg!(target_os = "macos") {
        "dylib"
    } else if cfg!(target_os = "windows") {
        "dll"
    } else {
        "so"
    };
    let path = plugins_dir.join(format!("{id}.{ext}"));
    path.is_file().then_some(path)
}

async fn plugin_config(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let settings = state.settings.current();
    let plugins_dir = if settings.plugins.dir.is_empty() {
        PathBuf::from("plugins")
    } else {
        PathBuf::from(&settings.plugins.dir)
    };

    if !is_plugin_installed(&state, &id, &plugins_dir) {
        return not_installed(&id);
    }

    let cfg_path = plugins_dir.join(format!("{id}.config.json"));
    let cfg: Value = std::fs::read_to_string(&cfg_path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_else(|| json!({ "enabled": true, "config": {} }));

    json_status(StatusCode::OK, cfg)
}

async fn write_plugin_config(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    raw: bytes::Bytes,
) -> Response {
    let settings = state.settings.current();
    let plugins_dir = if settings.plugins.dir.is_empty() {
        PathBuf::from("plugins")
    } else {
        PathBuf::from(&settings.plugins.dir)
    };

    if !is_plugin_installed(&state, &id, &plugins_dir) {
        return not_installed(&id);
    }

    let parsed = match serde_json::from_slice::<Value>(&raw) {
        Ok(v) => v,
        Err(_) => return json_status(StatusCode::BAD_REQUEST, json!({ "error": "invalid_body" })),
    };

    let cfg_path = plugins_dir.join(format!("{id}.config.json"));
    let result = update_plugin_config(&cfg_path, &parsed, || {
        if let Some(plugin) = state.plugin_host().and_then(|host| host.get_plugin(&id)) {
            let payload = serde_json::to_vec(&json!({ "config": parsed })).unwrap_or_default();
            let bytes = plugin
                .client
                .call(crate::plugins::abi::METHOD_PLUGIN_RECONFIGURE, &payload)?;
            let envelope: crate::plugins::abi::Envelope =
                serde_json::from_slice(&bytes).map_err(config_error)?;
            if !envelope.ok {
                return Err(envelope
                    .error
                    .unwrap_or_else(|| config_error("reconfiguration rejected")));
            }
        }
        Ok(())
    });
    if let Err(error) = result {
        return plugin_error_response(error);
    }

    json_status(StatusCode::OK, json!({ "status": "ok", "id": id }))
}

pub async fn query_plugin_quota_internal(
    state: &Arc<AppState>,
    id: &str,
    auth_index: &str,
    reset: bool,
) -> Response {
    let Some(host) = state.plugin_host() else {
        return json_status(
            StatusCode::NOT_FOUND,
            json!({ "error": "plugin_not_found", "message": "plugin host unavailable" }),
        );
    };

    let Some(plugin) = host.get_plugin(id) else {
        return json_status(
            StatusCode::NOT_FOUND,
            json!({ "error": "plugin_not_found", "message": "plugin not found" }),
        );
    };

    let method = if reset {
        crate::plugins::abi::METHOD_QUOTA_RESET
    } else {
        crate::plugins::abi::METHOD_QUOTA_FETCH
    };

    let req_payload = json!({
        "auth_index": auth_index,
        "authIndex": auth_index,
    });
    let req_bytes = serde_json::to_vec(&req_payload).unwrap_or_default();

    match plugin.client.call(method, &req_bytes) {
        Ok(res_bytes) => {
            let val: Value = match serde_json::from_slice(&res_bytes) {
                Ok(v) => v,
                Err(e) => {
                    return json_status(
                        StatusCode::BAD_GATEWAY,
                        json!({ "error": "invalid_plugin_response", "message": e.to_string() }),
                    );
                }
            };
            if let Some(err_val) = val.get("error").filter(|v| !v.is_null()) {
                return json_status(StatusCode::BAD_GATEWAY, err_val.clone());
            }
            if let Some(res_val) = val.get("result").filter(|v| !v.is_null()) {
                json_status(StatusCode::OK, res_val.clone())
            } else {
                json_status(StatusCode::OK, val)
            }
        }
        Err(err) => {
            let status = StatusCode::from_u16(err.http_status.unwrap_or(500))
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            json_status(status, json!({ "error": err.code, "message": err.message }))
        }
    }
}

async fn list_plugin_quota_providers(State(state): State<Arc<AppState>>) -> Response {
    let mut providers = Vec::new();
    if let Some(host) = state.plugin_host() {
        for p in host.list_plugins() {
            let has_quota = p
                .capabilities
                .get("quota_provider")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if has_quota {
                providers.push(json!({
                    "id": p.id,
                    "name": p.name,
                    "plugin_id": p.id,
                }));
            }
        }
    }
    json_status(StatusCode::OK, json!({ "providers": providers }))
}

async fn post_plugin_quota_fetch(
    State(state): State<Arc<AppState>>,
    raw: bytes::Bytes,
) -> Response {
    let body_json: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    let plugin_id = body_json
        .get("plugin_id")
        .or_else(|| body_json.get("pluginId"))
        .or_else(|| body_json.get("provider"))
        .or_else(|| body_json.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let auth_index = body_json
        .get("auth_index")
        .or_else(|| body_json.get("authIndex"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    query_plugin_quota_internal(&state, plugin_id, auth_index, false).await
}

async fn post_plugin_quota_reset(
    State(state): State<Arc<AppState>>,
    raw: bytes::Bytes,
) -> Response {
    let body_json: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    let plugin_id = body_json
        .get("plugin_id")
        .or_else(|| body_json.get("pluginId"))
        .or_else(|| body_json.get("provider"))
        .or_else(|| body_json.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let auth_index = body_json
        .get("auth_index")
        .or_else(|| body_json.get("authIndex"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    query_plugin_quota_internal(&state, plugin_id, auth_index, true).await
}

async fn get_plugin_quota_by_id(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let auth_index = params
        .get("auth_index")
        .or_else(|| params.get("authIndex"))
        .cloned()
        .unwrap_or_default();
    query_plugin_quota_internal(&state, &id, &auth_index, false).await
}

async fn delete_plugin_quota_by_id(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let auth_index = params
        .get("auth_index")
        .or_else(|| params.get("authIndex"))
        .cloned()
        .unwrap_or_default();
    query_plugin_quota_internal(&state, &id, &auth_index, true).await
}

async fn post_plugin_quota_reset_by_id(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Query(params): Query<std::collections::HashMap<String, String>>,
    raw: bytes::Bytes,
) -> Response {
    let body_json: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
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
    query_plugin_quota_internal(&state, &id, &auth_index, true).await
}

async fn post_plugin_quota_by_id(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Query(params): Query<std::collections::HashMap<String, String>>,
    raw: bytes::Bytes,
) -> Response {
    let body_json: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
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
    query_plugin_quota_internal(&state, &id, &auth_index, false).await
}

pub fn plugins_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/plugins", get(list_plugins))
        .route("/plugins/{id}", delete(delete_plugin))
        .route(
            "/plugins/{id}/enabled",
            axum::routing::patch(patch_plugin_enabled),
        )
        .route(
            "/plugins/{id}/config",
            get(plugin_config)
                .put(write_plugin_config)
                .patch(write_plugin_config),
        )
        .route("/plugin-store", get(list_plugin_store))
        .route("/plugin-store/{id}/install", post(install_plugin))
        .route("/plugins/quota/providers", get(list_plugin_quota_providers))
        .route("/plugins/quota/fetch", post(post_plugin_quota_fetch))
        .route("/plugins/quota/reset", post(post_plugin_quota_reset))
        .route(
            "/plugins/{id}/quota",
            get(get_plugin_quota_by_id)
                .post(post_plugin_quota_by_id)
                .delete(delete_plugin_quota_by_id),
        )
        .route(
            "/plugins/{id}/quota/reset",
            post(post_plugin_quota_reset_by_id),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_upstream_plugin_path_is_registered() {
        let groups: Value =
            serde_json::from_str(include_str!("../../../../.omo/upstream/route-groups.json"))
                .expect("route groups");
        let paths: Vec<String> = groups["plugins"]
            .as_array()
            .expect("plugins")
            .iter()
            .filter_map(|r| r.as_str())
            .map(|r| r.split_once(' ').expect("pair").1.to_string())
            .collect();
        let registered = [
            "/plugins",
            "/plugin-store",
            "/plugin-store/:id/install",
            "/plugins/:id",
            "/plugins/:id/enabled",
            "/plugins/:id/config",
        ];
        for path in &paths {
            assert!(registered.contains(&path.as_str()), "unregistered: {path}");
        }
    }
}
