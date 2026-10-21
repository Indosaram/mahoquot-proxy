use crate::state::AppState;
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::Arc;

pub use super::v8_config as config;

pub fn v8_config_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/config", get(root).put(root).patch(root))
        .route("/config.yaml", get(yaml).put(yaml))
        .route(
            "/config/{*path}",
            get(field).put(field).patch(field).delete(field),
        )
}

pub fn v8_management_router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .nest(
            "/v8/management",
            v8_config_routes().merge(super::v8::routes()),
        )
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            crate::management::gate::require_management_auth,
        ))
        .layer(axum::middleware::from_fn(
            crate::management::gate::stamp_management_response,
        ))
}

async fn root(State(state): State<Arc<AppState>>, method: Method, body: Bytes) -> Response {
    handle(state, method, String::new(), body, false).await
}
async fn yaml(State(state): State<Arc<AppState>>, method: Method, body: Bytes) -> Response {
    handle(state, method, String::new(), body, true).await
}
async fn field(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
    method: Method,
    body: Bytes,
) -> Response {
    handle(state, method, path, body, false).await
}
fn error_response(error: config::ConfigError) -> Response {
    use config::ConfigError;
    let (status, body) = match error {
        ConfigError::InvalidBody(message) => (
            StatusCode::BAD_REQUEST,
            json!({"error":"invalid_body", "message":message}),
        ),
        ConfigError::InvalidConfig(message) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({"error":"invalid_config", "message":message}),
        ),
        ConfigError::InvalidPath => (StatusCode::BAD_REQUEST, json!({"error":"invalid_path"})),
        ConfigError::NotFound => (StatusCode::NOT_FOUND, json!({"error":"not_found"})),
        ConfigError::ReadOnly(field) => (
            StatusCode::BAD_REQUEST,
            json!({"error":"read_only_field", "field":field.replace('.', "/")}),
        ),
        ConfigError::Store(error) => {
            let (status, code) = match &error {
                crate::management::settings::SettingsError::Store(
                    mahoquot_registry::store::StoreError::Unavailable(_),
                ) => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "storage_backend_unavailable",
                ),
                crate::management::settings::SettingsError::Store(
                    mahoquot_registry::store::StoreError::Conflict(_),
                ) => (StatusCode::CONFLICT, "storage_spool_conflict"),
                crate::management::settings::SettingsError::Read { .. }
                | crate::management::settings::SettingsError::Write { .. } => {
                    (StatusCode::INTERNAL_SERVER_ERROR, "write_failed")
                }
                _ => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_config"),
            };
            (status, json!({"error": code, "message": error.to_string()}))
        }
    };
    (status, Json(body)).into_response()
}
async fn handle(
    state: Arc<AppState>,
    method: Method,
    path: String,
    body: Bytes,
    yaml: bool,
) -> Response {
    let result = tokio::task::spawn_blocking(move || -> Result<Response, config::ConfigError> {
        if method == Method::GET {
            let raw = state.settings.config_yaml()?;
            let mut view = config::normalize(config::parse(&raw)?)?;
            config::archive_unknown(&mut view);
            if !path.is_empty() {
                config::project_aliases(&mut view, &path.replace('/', "."))?;
            }
            let response = if yaml {
                (
                    [(header::CONTENT_TYPE, "application/yaml; charset=utf-8")],
                    config::render(&view, &raw, true)?,
                )
                    .into_response()
            } else {
                let view = config::redact(view);
                let ptr = if path.is_empty() {
                    String::new()
                } else {
                    format!(
                        "/{}",
                        path.split('/')
                            .map(|s| s.replace('~', "~0").replace('/', "~1"))
                            .collect::<Vec<_>>()
                            .join("/")
                    )
                };
                Json(
                    view.pointer(&ptr)
                        .ok_or(config::ConfigError::NotFound)?
                        .clone(),
                )
                .into_response()
            };
            let mut response = response;
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-store"),
            );
            return Ok(response);
        }
        let mutation = match method {
            Method::PUT => config::Mutation::Put,
            Method::PATCH => config::Mutation::Patch,
            Method::DELETE => config::Mutation::Delete,
            _ => return Err(config::ConfigError::InvalidPath),
        };
        let raw = std::str::from_utf8(&body)
            .map_err(|e| config::ConfigError::InvalidBody(e.to_string()))?;
        let update: Option<Value> = if matches!(mutation, config::Mutation::Delete) {
            None
        } else if yaml {
            Some(config::parse(raw)?)
        } else {
            Some(
                serde_json::from_slice(&body)
                    .map_err(|e| config::ConfigError::InvalidBody(e.to_string()))?,
            )
        };
        state
            .settings
            .mutate_v8(&path, mutation, update, !yaml, yaml.then_some(raw))?;
        state.sync_routing_strategy();
        Ok(Json(json!({"status":"ok", "config-version":8})).into_response())
    })
    .await;
    match result {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => error_response(error),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"internal_error", "message":error.to_string()})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, extract::ConnectInfo, http::Request};
    use tower::ServiceExt;

    #[tokio::test]
    async fn mounted_v8_config_uses_the_existing_dedicated_management_gate() {
        let dir = std::env::temp_dir().join(format!("task27-router-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("fixture");
        std::fs::create_dir_all(dir.join("auth")).expect("fixture");
        let config_path = dir.join("config.yaml");
        std::fs::write(
            &config_path,
            "api-keys: [inference]\nremote-management: {secret-key: management}\n",
        )
        .expect("fixture");
        let state = Arc::new(
            AppState::new(&crate::config::GatewayConfig {
                auth_dir: dir.join("auth"),
                config_path,
                auth_refresh_enabled: false,
                ..Default::default()
            })
            .expect("state"),
        );
        let app = v8_management_router(state.clone()).with_state(state);
        for (key, peer, status) in [
            ("management", "127.0.0.1:18840", 200),
            ("inference", "127.0.0.1:18840", 401),
            ("management", "192.0.2.1:18840", 403),
        ] {
            let request = Request::builder()
                .uri("/v8/management/config")
                .header("x-management-key", key)
                .extension(ConnectInfo(
                    peer.parse::<std::net::SocketAddr>().expect("peer"),
                ))
                .body(Body::empty())
                .expect("request");
            assert_eq!(
                app.clone()
                    .oneshot(request)
                    .await
                    .expect("response")
                    .status()
                    .as_u16(),
                status
            );
        }
        std::fs::remove_dir_all(dir).expect("cleanup");
    }
}
