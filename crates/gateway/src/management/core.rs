use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::state::AppState;

fn json_status(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

async fn get_config(State(state): State<Arc<AppState>>) -> Response {
    let settings = state.settings.current();
    match serde_json::to_value(&*settings) {
        Ok(value) => json_status(StatusCode::OK, redact(value)),
        Err(err) => json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": "encode_failed", "message": err.to_string() }),
        ),
    }
}

/// The management secret must never leave the process: upstream tags
/// RemoteManagement `json:"-"` so `GET /config` omits it entirely.
fn redact(mut value: Value) -> Value {
    if let Some(map) = value.as_object_mut() {
        map.remove("remote-management");
    }
    value
}

/// Marker the served document carries on a blanked management secret.
///
/// `put_config_yaml` restores the stored secret only when this marker comes
/// back, so an operator who deliberately clears the value (and drops the marker)
/// still clears it.
const REDACTED_SECRET_MARKER: &str = "# mahoquot: secret withheld";

/// Blank the management secret in the served YAML.
///
/// The JSON `GET /config` drops the whole `remote-management` block, but this
/// endpoint hands the operator's file back so the console can edit it. Rewriting
/// the document through serde would discard every comment, so the value is
/// blanked textually.
///
/// The textual pass only understands the block layout serde_yaml emits. A
/// hand-edited flow mapping or quoted key would slip past it, so the result is
/// verified against the actual secret and falls back to the normalized document
/// (comments lost, secret provably gone) when anything survives.
fn redact_yaml_secret(raw: &str, secret: &str, normalized: &str) -> String {
    if secret.is_empty() {
        return raw.to_string();
    }
    let redacted = blank_block_secret(raw);
    if redacted.contains(secret) {
        return normalized.to_string();
    }
    redacted
}

fn blank_block_secret(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut remote_indent: Option<usize> = None;
    for line in raw.split_inclusive('\n') {
        let (body, terminator) = match line.strip_suffix("\r\n") {
            Some(body) => (body, "\r\n"),
            None => match line.strip_suffix('\n') {
                Some(body) => (body, "\n"),
                None => (line, ""),
            },
        };
        let trimmed = body.trim_start();
        let indent = body.len() - trimmed.len();
        if !trimmed.starts_with('#') {
            // Leaving the block: a non-blank line at or above the block's own
            // indentation starts a sibling key.
            if let Some(block_indent) = remote_indent {
                if !trimmed.is_empty() && indent <= block_indent {
                    remote_indent = None;
                }
            }
            if trimmed.starts_with("remote-management:") {
                remote_indent = Some(indent);
            } else if remote_indent.is_some() && trimmed.starts_with("secret-key:") {
                out.push_str(&body[..indent]);
                out.push_str("secret-key: \"\" ");
                out.push_str(REDACTED_SECRET_MARKER);
                out.push_str(terminator);
                continue;
            }
        }
        out.push_str(line);
    }
    out
}

async fn get_config_yaml(State(state): State<Arc<AppState>>) -> Response {
    let path = state.settings.path();
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let settings = state.settings.current();
            let secret = settings.remote_management.secret_key.clone();
            // The fallback shape: the same settings with the secret blanked and
            // rendered by serde, so it cannot carry the value regardless of how
            // the on-disk document was laid out.
            let normalized = {
                let mut blanked = (*settings).clone();
                blanked.remote_management.secret_key.clear();
                serde_yaml::to_string(&blanked).unwrap_or_else(|_| "remote-management: {}\n".into())
            };
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/yaml; charset=utf-8")],
                redact_yaml_secret(&raw, &secret, &normalized),
            )
                .into_response()
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => json_status(
            StatusCode::NOT_FOUND,
            json!({ "error": "not_found", "message": "config file not found" }),
        ),
        Err(err) => json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": "read_failed", "message": err.to_string() }),
        ),
    }
}

async fn put_config_yaml(State(state): State<Arc<AppState>>, raw: bytes::Bytes) -> Response {
    let Ok(text) = std::str::from_utf8(&raw) else {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({ "error": "invalid_yaml", "message": "cannot read request body" }),
        );
    };
    let parsed = match super::settings::Settings::from_yaml(text) {
        Ok(parsed) => parsed,
        Err(err) => {
            return json_status(
                StatusCode::BAD_REQUEST,
                json!({ "error": "invalid_yaml", "message": err.to_string() }),
            )
        }
    };
    // Persisting the whole document is a synchronous YAML write under the
    // mutate lock, so it is moved off the executor.
    // `GET /config.yaml` blanks the management secret and marks the line.
    // Restore the stored value only when that marker comes back, so omission is
    // preserved as "unchanged" while an operator who drops the marker can still
    // clear the secret deliberately.
    let carried_redaction_marker = text.contains(REDACTED_SECRET_MARKER);
    let settings = Arc::clone(&state.settings);
    let saved = tokio::task::spawn_blocking(move || {
        settings.mutate(|current| {
            let mut next = parsed;
            if next.remote_management.secret_key.is_empty() && carried_redaction_marker {
                next.remote_management.secret_key = current.remote_management.secret_key.clone();
            }
            *current = next;
        })
    })
    .await;
    let saved = match saved {
        Ok(saved) => saved,
        Err(err) => {
            tracing::error!("config write task failed: {err}");
            return json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": "write_failed", "message": "settings write task failed" }),
            );
        }
    };
    match saved {
        // Upstream reports which document sections it rewrote, and a
        // whole-config write is always reported as the single "config"
        // section regardless of what actually differed.
        Ok(_) => json_status(StatusCode::OK, json!({ "ok": true, "changed": ["config"] })),
        Err(super::settings::SettingsError::Validation(err)) => json_status(
            StatusCode::BAD_REQUEST,
            json!({ "error": "validation_error", "message": err.to_string() }),
        ),
        Err(super::settings::SettingsError::InvalidCatalogConfig(err)) => json_status(
            StatusCode::BAD_REQUEST,
            json!({ "error": "validation_error", "message": err }),
        ),
        Err(err) => json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": format!("failed to save config: {err}") }),
        ),
    }
}

async fn get_latest_version() -> Response {
    json_status(
        StatusCode::OK,
        json!({ "latest-version": format!("v{}", super::gate::cpa_version()) }),
    )
}

async fn reset_quota(State(state): State<Arc<AppState>>, raw: bytes::Bytes) -> Response {
    let Ok(body) = serde_json::from_slice::<Value>(&raw) else {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({ "error": "invalid request body" }),
        );
    };
    let auth_index = body
        .get("auth_index")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if auth_index.is_empty() {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({ "error": "auth_index is required" }),
        );
    }
    // Callers address an account by the opaque handle `GET /auth-files`
    // publishes, so resolve that back to the credential the pool knows before
    // falling back to treating the value as a direct account identifier.
    let resolved = super::creds::resolve_auth_index(&state, &auth_index);
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
    let Some(member) = by_file.or_else(|| state.find_member(&auth_index)) else {
        return json_status(StatusCode::NOT_FOUND, json!({ "error": "auth not found" }));
    };
    match crate::quota::consume_reset_credit(&state, &member).await {
        Ok(()) => json_status(
            StatusCode::OK,
            json!({
                "status": "ok",
                "auth_index": auth_index,
                "models": member.usage_snapshot(),
            }),
        ),
        Err(error) => json_status(
            crate::quota::reset_error_status(&error),
            json!({ "status": "error", "auth_index": auth_index, "error": error.to_string() }),
        ),
    }
}

/// Upstream proxies an arbitrary upstream call here. Without a configured
/// target this build has nothing to forward to, and upstream answers the same
/// way when its own auth manager is unavailable.
async fn api_call(raw: bytes::Bytes) -> Response {
    let parsed = serde_json::from_slice::<Value>(&raw).unwrap_or(Value::Null);
    let has_method = parsed
        .get("method")
        .and_then(Value::as_str)
        .is_some_and(|m| !m.trim().is_empty());
    if !has_method {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({ "error": "missing method" }),
        );
    }
    json_status(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({ "error": "core auth manager unavailable" }),
    )
}

async fn shutdown(State(state): State<Arc<AppState>>) -> Response {
    state.shutdown.notify_waiters();
    json_status(StatusCode::OK, json!({ "status": "draining" }))
}

pub fn core_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/config", get(get_config))
        .route("/config.yaml", get(get_config_yaml).put(put_config_yaml))
        .route("/latest-version", get(get_latest_version))
        .route("/api-call", post(api_call))
        .route("/reset-quota", post(reset_quota))
        .route("/shutdown", post(shutdown))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::settings::{RemoteManagement, Settings};

    #[test]
    fn get_config_never_exposes_the_management_secret() {
        // given a config carrying a secret
        let settings = Settings {
            remote_management: RemoteManagement {
                secret_key: "top-secret".to_string(),
                ..RemoteManagement::default()
            },
            ..Settings::default()
        };
        // when it is rendered for GET /config
        let rendered = redact(serde_json::to_value(&settings).expect("encodes"));
        // then the whole block is gone, secret included
        let text = rendered.to_string();
        assert!(!text.contains("top-secret"), "{text}");
        assert!(!text.contains("remote-management"), "{text}");
    }

    #[test]
    fn redaction_keeps_the_rest_of_the_document() {
        // given a config with an ordinary field set
        let settings = Settings {
            request_retry: 4,
            ..Settings::default()
        };
        // when redacted
        let rendered = redact(serde_json::to_value(&settings).expect("encodes"));
        // then non-secret fields survive
        assert_eq!(rendered["request-retry"], 4);
    }

    #[test]
    fn served_yaml_blanks_the_management_secret_and_keeps_comments() {
        // given an operator document with the secret and a comment
        let raw = "port: 18801\n# keep this note\nremote-management:\n  allow-remote: false\n  secret-key: top-secret\n  disable-control-panel: false\napi-keys:\n- mq-master\n";
        // when it is served
        let served = redact_yaml_secret(raw, "top-secret", "remote-management: {}\n");
        // then the secret is gone but the rest of the file is byte-identical
        assert!(!served.contains("top-secret"), "{served}");
        assert!(served.contains("# keep this note"), "{served}");
        assert!(served.contains("  secret-key: \"\""), "{served}");
        assert!(served.contains("  allow-remote: false"), "{served}");
        assert!(
            served.contains("  disable-control-panel: false"),
            "{served}"
        );
        assert!(served.contains("- mq-master"), "{served}");
    }

    #[test]
    fn served_yaml_redaction_does_not_touch_a_lookalike_key_elsewhere() {
        // given a `secret-key` outside the remote-management block
        let raw =
            "remote-management:\n  secret-key: real\nquota-exceeded:\n  secret-key: keep-me\n";
        // when it is served
        let served = redact_yaml_secret(raw, "real", "remote-management: {}\n");
        // then only the management secret is blanked
        assert!(!served.contains("real"), "{served}");
        assert!(served.contains("  secret-key: keep-me"), "{served}");
    }

    #[test]
    fn served_yaml_never_leaks_the_secret_whatever_the_layout() {
        let secret = "top-secret";
        let normalized = "remote-management:\n  allow-remote: false\n";
        // Layouts the textual block pass cannot follow must fall back to the
        // normalized document rather than leaking or emitting invalid YAML.
        let cases: [(&str, &str); 5] = [
            (
                "block mapping",
                "remote-management:\n  secret-key: top-secret\n",
            ),
            (
                "flow mapping",
                "remote-management: {secret-key: top-secret}\n",
            ),
            (
                "quoted key",
                "remote-management:\n  \"secret-key\": top-secret\n",
            ),
            (
                "block scalar",
                "remote-management:\n  secret-key: |-\n    top-secret\n",
            ),
            ("crlf", "remote-management:\r\n  secret-key: top-secret\r\n"),
        ];
        for (name, raw) in cases {
            let served = redact_yaml_secret(raw, secret, normalized);
            assert!(
                !served.contains(secret),
                "{name} leaked the secret: {served:?}"
            );
            assert!(
                serde_yaml::from_str::<serde_json::Value>(&served).is_ok(),
                "{name} produced invalid YAML: {served:?}"
            );
        }
    }
}
