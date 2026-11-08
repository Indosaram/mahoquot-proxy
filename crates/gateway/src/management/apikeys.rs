use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use super::scalar_table::Refusal;
use super::settings::{ProviderKeyEntry, Settings};
use super::{lists, scalars};
use crate::state::AppState;

/// A provider key collection: its route, the key its GET answers under, and
/// where the list lives in the settings document.
struct KeyList {
    path: &'static str,
    key: &'static str,
    field: KeyField,
}

#[derive(Clone, Copy)]
enum KeyField {
    Inbound(fn(&mut Settings) -> &mut Vec<String>),
    Provider(fn(&mut Settings) -> &mut Vec<ProviderKeyEntry>),
}

const KEY_LISTS: &[KeyList] = &[
    KeyList {
        path: "/api-keys",
        key: "api-keys",
        field: KeyField::Inbound(|s| &mut s.api_keys),
    },
    KeyList {
        path: "/gemini-api-key",
        key: "gemini-api-key",
        field: KeyField::Provider(|s| &mut s.gemini_api_key),
    },
    KeyList {
        path: "/claude-api-key",
        key: "claude-api-key",
        field: KeyField::Provider(|s| &mut s.claude_api_key),
    },
    KeyList {
        path: "/codex-api-key",
        key: "codex-api-key",
        field: KeyField::Provider(|s| &mut s.codex_api_key),
    },
    KeyList {
        path: "/xai-api-key",
        key: "xai-api-key",
        field: KeyField::Provider(|s| &mut s.xai_api_key),
    },
    KeyList {
        path: "/vertex-api-key",
        key: "vertex-api-key",
        field: KeyField::Provider(|s| &mut s.vertex_api_key),
    },
    KeyList {
        path: "/interactions-api-key",
        key: "interactions-api-key",
        field: KeyField::Provider(|s| &mut s.interactions_api_key),
    },
    KeyList {
        path: "/openai-compatibility",
        key: "openai-compatibility",
        field: KeyField::Provider(|s| &mut s.openai_compatibility),
    },
];

fn find(path: &str) -> Option<&'static KeyList> {
    KEY_LISTS.iter().find(|k| k.path == path)
}

fn entry(path: &'static str) -> &'static KeyList {
    find(path).expect("registered key list")
}

pub fn key_lists_paths() -> impl Iterator<Item = &'static str> {
    KEY_LISTS.iter().map(|k| k.path)
}

async fn read(state: Arc<AppState>, list: &'static KeyList) -> Response {
    let mut settings = Settings::clone(&state.settings.current());
    let value = match list.field {
        KeyField::Inbound(field) => json!(field(&mut settings)),
        KeyField::Provider(field) => json!(field(&mut settings)),
    };
    (StatusCode::OK, Json(json!({ list.key: value }))).into_response()
}

async fn mutate(
    state: &Arc<AppState>,
    list: &'static KeyList,
    body: Value,
    patch: bool,
) -> Response {
    let response = scalars::apply_edit(state, move |settings| match list.field {
        KeyField::Inbound(field) => {
            if patch {
                lists::edit(field(settings), &body)
            } else {
                lists::replace(field(settings), &body)
            }
        }
        KeyField::Provider(field) => {
            if patch {
                lists::edit(field(settings), &body)
            } else {
                lists::replace(field(settings), &body)
            }
        }
    })
    .await;
    if response.status() == StatusCode::OK && matches!(list.field, KeyField::Provider(_)) {
        super::core::publish_retry_metadata(state);
        let state = state.clone();
        match tokio::task::spawn_blocking(move || state.rescan_pool()).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => return reload_failed(error),
            Err(error) => return reload_failed(error),
        }
    }
    response
}

async fn replace(state: Arc<AppState>, list: &'static KeyList, raw: bytes::Bytes) -> Response {
    let Ok(body) = serde_json::from_slice::<Value>(&raw) else {
        return scalars::refusal_response(Refusal::InvalidBody);
    };
    mutate(&state, list, body, false).await
}

async fn edit(state: Arc<AppState>, list: &'static KeyList, raw: bytes::Bytes) -> Response {
    let Ok(body) = serde_json::from_slice::<Value>(&raw) else {
        return scalars::refusal_response(Refusal::InvalidBody);
    };
    mutate(&state, list, body, true).await
}

async fn remove(
    state: Arc<AppState>,
    list: &'static KeyList,
    params: HashMap<String, String>,
) -> Response {
    let response = scalars::apply_edit(&state, move |settings| match list.field {
        KeyField::Inbound(field) => lists::remove(
            field(settings),
            params.get("index").map(String::as_str),
            params.get("value").map(String::as_str),
        ),
        KeyField::Provider(field) => lists::remove(
            field(settings),
            params.get("index").map(String::as_str),
            params.get("value").map(String::as_str),
        ),
    })
    .await;
    if response.status() == StatusCode::OK && matches!(list.field, KeyField::Provider(_)) {
        super::core::publish_retry_metadata(&state);
        let state = state.clone();
        match tokio::task::spawn_blocking(move || state.rescan_pool()).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => return reload_failed(error),
            Err(error) => return reload_failed(error),
        }
    }
    response
}

fn reload_failed(error: impl std::fmt::Display) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": "credential_reload_failed",
            "message": error.to_string(),
            "settings-saved": true,
            "retry-metadata-published": true,
            "credential-reload": "failed"
        })),
    )
        .into_response()
}

async fn api_key_usage(State(state): State<Arc<AppState>>) -> Response {
    (
        StatusCode::OK,
        Json(json!({ "api-key-usage": state.get_stats() })),
    )
        .into_response()
}

/// Upstream reports the pending usage-record queue here. This build persists
/// usage inline rather than through a queue, so the depth is always zero;
/// reporting a fabricated backlog would be worse than reporting the truth.
async fn usage_queue() -> Response {
    (StatusCode::OK, Json(json!([]))).into_response()
}

pub fn apikeys_routes() -> Router<Arc<AppState>> {
    let mut router = Router::new()
        .route("/api-key-usage", get(api_key_usage))
        .route("/usage-queue", get(usage_queue));

    for list in KEY_LISTS {
        let path = list.path;
        router =
            router.route(
                path,
                get(move |State(state): State<Arc<AppState>>| async move {
                    read(state, entry(path)).await
                })
                .post(
                    move |State(state): State<Arc<AppState>>, body: bytes::Bytes| async move {
                        replace(state, entry(path), body).await
                    },
                )
                .put(
                    move |State(state): State<Arc<AppState>>, body: bytes::Bytes| async move {
                        replace(state, entry(path), body).await
                    },
                )
                .patch(
                    move |State(state): State<Arc<AppState>>, body: bytes::Bytes| async move {
                        edit(state, entry(path), body).await
                    },
                )
                .delete(
                    move |State(state): State<Arc<AppState>>,
                          Query(params): Query<HashMap<String, String>>| async move {
                        remove(state, entry(path), params).await
                    },
                ),
            );
    }
    router
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ReloadFixture(std::path::PathBuf);

    impl ReloadFixture {
        fn cleanup(&self) -> std::io::Result<()> {
            let auth = self.0.join("auth");
            let held = self.0.join("held-auth");
            if held.try_exists()? {
                match std::fs::remove_file(&auth) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                std::fs::rename(&held, &auth)?;
            }
            match std::fs::remove_dir_all(&self.0) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            }
        }
    }

    impl Drop for ReloadFixture {
        fn drop(&mut self) {
            if let Err(error) = self.cleanup() {
                eprintln!("owned reload fixture cleanup {}: {error}", self.0.display());
            }
        }
    }

    #[tokio::test]
    async fn typed_provider_retry_write_reaches_live_credential() {
        let dir =
            std::env::temp_dir().join(format!("mahoquot-key-retry-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("codex-fixture.json"), r#"{"type":"codex","access_token":"fixture","account_id":"fixture","email":"fixture@example.invalid","expired":"2099-01-01T00:00:00Z","id_token":"i","last_refresh":"","refresh_token":"r","request_retry":2}"#).unwrap();
        let config_path = dir.join("config.yaml");
        std::fs::write(
            &config_path,
            format!(
                "auth-dir: {}\nmodel-catalog:\n  refresh-enabled: false\n",
                dir.display()
            ),
        )
        .unwrap();
        let state = Arc::new(
            AppState::new(&crate::config::GatewayConfig {
                auth_dir: dir.clone(),
                config_path,
                ..Default::default()
            })
            .unwrap(),
        );
        let old = state.pool.load_full();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            replace(
                state.clone(),
                entry("/codex-api-key"),
                bytes::Bytes::from_static(br#"[{"api-key":"fixture","request-retry":0}]"#),
            ),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            state.pool.load().members[0].request_retry_override(),
            Some(0)
        );
        assert_eq!(old.members[0].request_retry_override(), Some(2));
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            remove(
                state.clone(),
                entry("/codex-api-key"),
                HashMap::from([("index".to_string(), "0".to_string())]),
            ),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            state.pool.load().members[0].request_retry_override(),
            Some(2)
        );
        drop(old);
        drop(state);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn every_upstream_apikey_path_is_registered() {
        // given the upstream route list
        let groups: Value =
            serde_json::from_str(include_str!("../../../../.omo/upstream/route-groups.json"))
                .expect("route groups");
        let paths: std::collections::BTreeSet<String> = groups["apikeys"]
            .as_array()
            .expect("apikeys")
            .iter()
            .map(|r| {
                r.as_str()
                    .expect("route")
                    .split_once(' ')
                    .expect("pair")
                    .1
                    .to_string()
            })
            .collect();
        // then each is either a key list or one of the two read-only reports
        let known = ["/api-key-usage", "/usage-queue"];
        let missing: Vec<_> = paths
            .iter()
            .filter(|p| find(p).is_none() && !known.contains(&p.as_str()))
            .collect();
        assert!(missing.is_empty(), "unregistered apikey paths: {missing:?}");
    }

    #[tokio::test]
    async fn provider_reload_failure_reports_saved_settings_and_recovers() {
        use http_body_util::BodyExt;
        let dir = std::env::temp_dir().join(format!(
            "mahoquot-key-reload-failure-{}",
            rand::random::<u64>()
        ));
        std::fs::create_dir(&dir).unwrap();
        let fixture = ReloadFixture(dir.clone());
        let auth = dir.join("auth");
        std::fs::create_dir_all(&auth).unwrap();
        std::fs::write(auth.join("codex-fixture.json"), r#"{"type":"codex","access_token":"fixture","account_id":"fixture","email":"fixture@example.invalid","expired":"2099-01-01T00:00:00Z","id_token":"i","last_refresh":"","refresh_token":"r","request_retry":2}"#).unwrap();
        let config_path = dir.join("config.yaml");
        std::fs::write(
            &config_path,
            format!(
                "auth-dir: {}\nmodel-catalog:\n  refresh-enabled: false\n",
                auth.display()
            ),
        )
        .unwrap();
        let state = Arc::new(
            AppState::new(&crate::config::GatewayConfig {
                auth_dir: auth.clone(),
                config_path: config_path.clone(),
                ..Default::default()
            })
            .unwrap(),
        );
        let old = state.pool.load_full();
        let held = dir.join("held-auth");
        std::fs::rename(&auth, &held).unwrap();
        std::fs::write(&auth, b"auth-directory-obstruction").unwrap();
        assert!(std::fs::metadata(&auth).unwrap().is_file());
        assert!(crate::account::load_account_members(&auth).is_err());
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            replace(
                state.clone(),
                entry("/codex-api-key"),
                bytes::Bytes::from_static(br#"[{"api-key":"fixture","request-retry":0}]"#),
            ),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            response.into_body().collect(),
        )
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
        let error: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(error["error"], "credential_reload_failed");
        assert_eq!(error["settings-saved"], true);
        assert_eq!(error["retry-metadata-published"], true);
        assert_eq!(error["credential-reload"], "failed");
        assert!(!error["message"].as_str().unwrap().is_empty());
        assert_eq!(
            Settings::load(&config_path).unwrap().codex_api_key[0].retry_for_key("fixture"),
            Some(Some(0))
        );
        assert_eq!(
            state.settings.current().codex_api_key[0].retry_for_key("fixture"),
            Some(Some(0))
        );
        assert_eq!(state.pool.load().members.len(), 1);
        assert_eq!(
            state.pool.load().members[0].request_retry_override(),
            Some(0)
        );
        assert_eq!(old.members[0].request_retry_override(), Some(2));
        assert_eq!(std::fs::read(&auth).unwrap(), b"auth-directory-obstruction");
        std::fs::remove_file(&auth).unwrap();
        std::fs::rename(&held, &auth).unwrap();
        let recovery = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            replace(
                state.clone(),
                entry("/codex-api-key"),
                bytes::Bytes::from_static(br#"[{"api-key":"fixture","request-retry":0}]"#),
            ),
        )
        .await
        .unwrap();
        assert_eq!(recovery.status(), StatusCode::OK);
        assert_eq!(
            state.pool.load().members[0].request_retry_override(),
            Some(0)
        );
        drop(old);
        drop(state);
        fixture.cleanup().unwrap();
    }

    #[test]
    fn each_key_list_targets_a_distinct_field() {
        // given every registered list
        let mut settings = Settings::default();
        for list in KEY_LISTS {
            match list.field {
                KeyField::Inbound(field) => field(&mut settings).push(list.key.to_string()),
                KeyField::Provider(field) => {
                    field(&mut settings).push(ProviderKeyEntry::Key(list.key.to_string()))
                }
            }
        }
        // then no two share a backing field
        for list in KEY_LISTS {
            let target = match list.field {
                KeyField::Inbound(field) => json!(field(&mut settings)),
                KeyField::Provider(field) => json!(field(&mut settings)),
            };
            assert_eq!(target, json!([list.key]), "aliased: {}", list.path);
        }
    }
}
