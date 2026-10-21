use crate::account::{AccountMember, ProviderKind};
use crate::capability;
use crate::inbound::ResolvedAuth;
use crate::state::AppState;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{
    body::Body,
    extract::{Request, State},
    Extension,
};
use bytes::Bytes;
use futures::StreamExt;
use mahoquot_registry::ModelCapability;
use mahoquot_types::{PoolMember, SessionHint};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::RwLock;

const MAX_UPLOAD: u64 = 512 * 1024 * 1024;
const MAX_FIELD: usize = 64 * 1024;
const VIDEO_BINDING_TTL: Duration = Duration::from_secs(3 * 60 * 60);

#[derive(Clone)]
struct VideoBinding {
    caller_key: Option<String>,
    account_id: String,
    provider: String,
    model: String,
    expires_at: Instant,
}

fn video_bindings() -> &'static RwLock<HashMap<String, VideoBinding>> {
    static BINDINGS: std::sync::OnceLock<RwLock<HashMap<String, VideoBinding>>> =
        std::sync::OnceLock::new();
    BINDINGS.get_or_init(|| RwLock::new(HashMap::new()))
}

async fn bind_video(id: &str, caller_key: Option<&str>, member: &AccountMember, model: &str) {
    let mut bindings = video_bindings().write().await;
    bindings.retain(|_, value| value.expires_at > Instant::now());
    bindings.insert(
        id.to_owned(),
        VideoBinding {
            caller_key: caller_key.map(str::to_owned),
            account_id: member.id().to_owned(),
            provider: member.provider_name().to_owned(),
            model: model.to_owned(),
            expires_at: Instant::now() + VIDEO_BINDING_TTL,
        },
    );
}

async fn bound_video(id: &str) -> Option<VideoBinding> {
    let mut bindings = video_bindings().write().await;
    if bindings
        .get(id)
        .is_some_and(|binding| binding.expires_at <= Instant::now())
    {
        bindings.remove(id);
    }
    bindings.get(id).cloned()
}

fn video_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        axum::Json(json!({"error":{"type":"invalid_request_error","code":code,"message":message}})),
    )
        .into_response()
}

fn unsupported_video_error(state: &AppState, model: &str, fallback: Response) -> Response {
    let snapshot = state.pool.load();
    capability::check_video(&snapshot, model)
        .map(|body| (StatusCode::BAD_REQUEST, axum::Json(body)).into_response())
        .unwrap_or(fallback)
}

fn video_id(body: &Value) -> Option<&str> {
    body.get("request_id")
        .or_else(|| body.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
}

fn video_status(payload: &mut Value) {
    let status = payload
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mapped = match status.as_str() {
        "queued" | "pending" => Some("queued"),
        "in_progress" | "processing" | "running" => Some("in_progress"),
        "completed" | "done" | "succeeded" | "success" => Some("completed"),
        "failed" | "error" | "expired" | "cancelled" | "canceled" => Some("failed"),
        _ => None,
    };
    if let Some(mapped) = mapped {
        payload["status"] = json!(mapped);
    }
}

fn video_upstream_path(path: &str) -> &'static str {
    match path {
        "/v1/videos/edits" | "/videos/edits" => "/videos/edits",
        "/v1/videos/extensions" | "/videos/extensions" => "/videos/extensions",
        _ => "/videos/generations",
    }
}

async fn video_request(
    state: Arc<AppState>,
    auth: ResolvedAuth,
    requested_model: String,
    body: Value,
    path: String,
    method: reqwest::Method,
    bind_created: bool,
) -> Response {
    let (member, mapped) = match select(&state, &auth, &requested_model, ModelCapability::Video) {
        Ok(selected) => selected,
        Err(response) => return unsupported_video_error(&state, &requested_model, response),
    };
    let mut payload = body;
    if method == reqwest::Method::POST {
        payload["model"] = json!(mapped);
    }
    let (url, headers) = target(&member, false, &path);
    let client = match state.try_client_for_member(&member) {
        Ok(client) => client,
        Err(_) => return error(StatusCode::BAD_GATEWAY, "media_proxy_invalid"),
    };
    let activity = member.begin_activity();
    let mut request = client
        .request(method, url)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json")
        .json(&payload);
    for (name, value) in headers {
        if crate::wsrelay::safe_header(&name) {
            request = request.header(name, value);
        }
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(_) => return error(StatusCode::BAD_GATEWAY, "media_upstream_failed"),
    };
    let status = response.status();
    let source_headers = response.headers().clone();
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(_) => return error(StatusCode::BAD_GATEWAY, "media_upstream_failed"),
    };
    drop(activity);
    if !status.is_success() {
        let mut builder = Response::builder().status(status);
        if let Some(content_type) = source_headers.get(header::CONTENT_TYPE) {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        return builder
            .body(Body::from(bytes))
            .unwrap_or_else(|_| error(StatusCode::BAD_GATEWAY, "media_upstream_failed"));
    }
    let mut result: Value = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) if value.is_object() => value,
        _ => return error(StatusCode::BAD_GATEWAY, "invalid_media_response"),
    };
    if bind_created {
        let Some(id) = video_id(&result).map(str::to_owned) else {
            return error(StatusCode::BAD_GATEWAY, "video_id_missing");
        };
        bind_video(&id, auth.key_identifier.as_deref(), &member, &mapped).await;
    } else {
        video_status(&mut result);
    }
    let mut response = (status, axum::Json(result)).into_response();
    for name in [header::RETRY_AFTER] {
        if let Some(value) = source_headers.get(&name) {
            response.headers_mut().insert(name.clone(), value.clone());
        }
    }
    response
}

pub async fn videos(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let path = request.uri().path().to_owned();
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let raw = match axum::body::to_bytes(request.into_body(), MAX_UPLOAD as usize).await {
        Ok(raw) => raw,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "video_upload_too_large"),
    };
    let mut payload: Value = match serde_json::from_slice::<Value>(&raw) {
        Ok(value) if value.is_object() => value,
        _ => return error(StatusCode::BAD_REQUEST, "invalid_request"),
    };
    if content_type.starts_with("multipart/form-data") {
        return error(StatusCode::NOT_IMPLEMENTED, "video_multipart_unsupported");
    }
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    if model.is_empty()
        || payload
            .get("prompt")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return error(StatusCode::BAD_REQUEST, "invalid_video_request");
    }
    let path = video_upstream_path(&path).to_owned();
    let original = payload.clone();
    let (member, mapped) = match select(&state, &auth, &model, ModelCapability::Video) {
        Ok(selected) => selected,
        Err(_) => {
            return unsupported_video_error(
                &state,
                &model,
                error(StatusCode::BAD_REQUEST, "video_model_unsupported"),
            );
        }
    };
    payload["model"] = json!(mapped);
    if state
        .settings
        .current()
        .payload
        .apply(
            &mut payload,
            &crate::management::settings::PayloadContext {
                model: &mapped,
                requested_model: &model,
                protocol: "openai",
                from_protocol: "openai-video",
                headers: &headers,
                original: &original,
            },
        )
        .is_err()
    {
        return error(StatusCode::BAD_REQUEST, "invalid_payload");
    }
    let idempotency = headers
        .get("x-idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let (url, upstream_headers) = target(&member, false, &path);
    let client = match state.try_client_for_member(&member) {
        Ok(client) => client,
        Err(_) => return error(StatusCode::BAD_GATEWAY, "media_proxy_invalid"),
    };
    let _activity = member.begin_activity();
    let mut request = client
        .post(url)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json")
        .json(&payload);
    for (name, value) in upstream_headers {
        if crate::wsrelay::safe_header(&name) {
            request = request.header(name, value);
        }
    }
    if let Some(key) = idempotency {
        request = request.header("x-idempotency-key", key);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(_) => return error(StatusCode::BAD_GATEWAY, "media_upstream_failed"),
    };
    let status = response.status();
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(_) => return error(StatusCode::BAD_GATEWAY, "media_upstream_failed"),
    };
    if !status.is_success() {
        return (
            status,
            [(header::CONTENT_TYPE, "application/json")],
            Body::from(bytes),
        )
            .into_response();
    }
    let mut result: Value = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) if value.is_object() => value,
        _ => return error(StatusCode::BAD_GATEWAY, "invalid_media_response"),
    };
    let Some(id) = video_id(&result).map(str::to_owned) else {
        return error(StatusCode::BAD_GATEWAY, "video_id_missing");
    };
    bind_video(&id, auth.key_identifier.as_deref(), &member, &mapped).await;
    if result.get("id").is_none() {
        result["id"] = json!(id);
    }
    result["object"] = json!("video");
    if result.get("status").is_none() {
        result["status"] = json!("queued");
    }
    video_status(&mut result);
    (status, axum::Json(result)).into_response()
}

pub async fn videos_by_id(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let Some(binding) = bound_video(&id).await else {
        return video_error(
            StatusCode::NOT_FOUND,
            "video_not_found",
            "video is unknown or expired",
        );
    };
    if binding.caller_key.is_some() && binding.caller_key != auth.key_identifier {
        return error(StatusCode::NOT_FOUND, "video_not_found");
    }
    let model = binding.model.clone();
    let (member, _) = match select(&state, &auth, &model, ModelCapability::Video) {
        Ok(selected) => selected,
        Err(response) => return response,
    };
    if member.id() != binding.account_id || member.provider_name() != binding.provider {
        return error(StatusCode::NOT_FOUND, "video_not_found");
    }
    let mut body = json!({"request_id": id});
    let path = format!("/videos/{}", urlencoding::encode(&id));
    let response = video_request(
        state,
        auth,
        model,
        body.take(),
        path,
        reqwest::Method::GET,
        false,
    )
    .await;
    response
}

pub async fn openai_videos(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
    axum::extract::Path(id): axum::extract::Path<String>,
    request: Request,
) -> Response {
    let path = request.uri().path().to_owned();
    if request.method() == axum::http::Method::POST {
        let raw = match axum::body::to_bytes(request.into_body(), 16 * 1024 * 1024).await {
            Ok(raw) => raw,
            Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "video_request_too_large"),
        };
        let payload: Value = match serde_json::from_slice::<Value>(&raw) {
            Ok(value) if value.is_object() => value,
            _ => return error(StatusCode::BAD_REQUEST, "invalid_request"),
        };
        let _model = payload
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("grok-imagine-video")
            .to_owned();
        return videos(
            State(state),
            Extension(auth),
            HeaderMap::new(),
            Request::new(Body::from(payload.to_string())),
        )
        .await;
    }
    let Some(binding) = bound_video(&id).await else {
        return video_error(
            StatusCode::NOT_FOUND,
            "video_not_found",
            "video is unknown or expired",
        );
    };
    if binding.caller_key.is_some() && binding.caller_key != auth.key_identifier {
        return error(StatusCode::NOT_FOUND, "video_not_found");
    }
    let model = binding.model.clone();
    let (member, _) = match select(&state, &auth, &model, ModelCapability::Video) {
        Ok(selected) => selected,
        Err(response) => return response,
    };
    if member.id() != binding.account_id || member.provider_name() != binding.provider {
        return error(StatusCode::NOT_FOUND, "video_not_found");
    }
    if path.ends_with("/content") {
        let (url, headers) = target(
            &member,
            false,
            &format!("/videos/{}/content", urlencoding::encode(&id)),
        );
        let client = match state.try_client_for_member(&member) {
            Ok(client) => client,
            Err(_) => return error(StatusCode::BAD_GATEWAY, "media_proxy_invalid"),
        };
        let activity = member.begin_activity();
        let mut outbound = client.get(url);
        for (name, value) in headers {
            if crate::wsrelay::safe_header(&name) {
                outbound = outbound.header(name, value);
            }
        }
        if let Some(range) = request.headers().get(header::RANGE) {
            outbound = outbound.header(header::RANGE, range);
        }
        let response = match outbound.send().await {
            Ok(response) => response,
            Err(_) => return error(StatusCode::BAD_GATEWAY, "media_upstream_failed"),
        };
        let status = response.status();
        let source_headers = response.headers().clone();
        if !status.is_success() {
            drop(activity);
            let bytes = match response.bytes().await {
                Ok(bytes) => bytes,
                Err(_) => return error(StatusCode::BAD_GATEWAY, "media_upstream_failed"),
            };
            let mut builder = Response::builder().status(status);
            if let Some(content_type) = source_headers.get(header::CONTENT_TYPE) {
                builder = builder.header(header::CONTENT_TYPE, content_type);
            }
            return builder
                .body(Body::from(bytes))
                .unwrap_or_else(|_| error(StatusCode::BAD_GATEWAY, "media_download_failed"));
        }
        let mut builder = Response::builder().status(status);
        for name in [
            header::CONTENT_TYPE,
            header::CONTENT_LENGTH,
            header::CONTENT_DISPOSITION,
            header::CACHE_CONTROL,
            header::ETAG,
            header::LAST_MODIFIED,
            header::ACCEPT_RANGES,
            header::CONTENT_RANGE,
        ] {
            if let Some(value) = source_headers.get(&name) {
                builder = builder.header(name.clone(), value);
            }
        }
        if source_headers.get(header::CONTENT_TYPE).is_none() {
            builder = builder.header(header::CONTENT_TYPE, "video/mp4");
        }
        let stream = futures::stream::unfold(
            (response.bytes_stream(), activity),
            |(mut upstream, activity)| async move {
                upstream
                    .next()
                    .await
                    .map(|chunk| (chunk, (upstream, activity)))
            },
        );
        return builder
            .body(Body::from_stream(stream))
            .unwrap_or_else(|_| error(StatusCode::BAD_GATEWAY, "media_download_failed"));
    }
    let query = request
        .uri()
        .query()
        .map(|value| format!("?{value}"))
        .unwrap_or_default();
    video_request(
        state,
        auth,
        model,
        json!({"request_id": id}),
        format!("/videos/{id}{query}"),
        reqwest::Method::GET,
        false,
    )
    .await
}

fn error(status: StatusCode, code: &str) -> Response {
    error_with_message(status, code, &code.replace('_', " "))
}

/// OpenAI-compatible error body: clients read `error.message`, so it is always present.
fn error_with_message(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        axum::Json(json!({"error":{"type":"invalid_request_error","code":code,"message":message}})),
    )
        .into_response()
}

fn unsupported_media_model(model: &str, capability: ModelCapability) -> Response {
    error_with_message(
        StatusCode::BAD_REQUEST,
        "unsupported_media_model",
        &format!("model '{model}' is not supported for {capability:?} by any configured account"),
    )
}

// Keep HTTP response errors compatible with the media handlers.
#[allow(clippy::result_large_err)]
fn select(
    state: &AppState,
    auth: &ResolvedAuth,
    model: &str,
    capability: ModelCapability,
) -> Result<(Arc<AccountMember>, String), Response> {
    let pool = state.pool.load_full();
    let resolved = if capability == ModelCapability::Audio {
        crate::relay::resolve_model(&pool, model)
            .map_err(|_| unsupported_media_model(model, capability))?
    } else {
        crate::capability::resolve_for_capability(&pool, model, capability)
            .ok_or_else(|| unsupported_media_model(model, capability))?
    };
    if let Some(scope) = auth.identity.scoped() {
        if !crate::models_route::allow_list_admits(&scope.allowed_models, model) {
            return Err(error(StatusCode::FORBIDDEN, "model_not_allowed"));
        }
        if state
            .scoped_keys
            .get(&scope.key_identifier)
            .is_some_and(|entry| entry.is_exhausted())
        {
            return Err(error(StatusCode::TOO_MANY_REQUESTS, "token_quota_exceeded"));
        }
    }
    let settings = state.settings.current();
    let binding = settings
        .api_key_bindings
        .iter()
        .find(|binding| Some(&binding.key_identifier) == auth.key_identifier.as_ref());
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    let ids = crate::relay::eligible_account_ids_for_model(state, model, now_ms);
    let accounts: Vec<_> = pool
        .routable_accounts_for_model(model)
        .into_iter()
        .filter(|member| {
            (member.kind() == ProviderKind::Generic
                || capability == ModelCapability::Image && member.kind() == ProviderKind::Codex)
                && ids.iter().any(|id| id == member.id())
                && member.health().is_available(now_ms)
                && !member.is_manually_disabled()
                && state.scheduler.permits(member.id())
                && (if capability == ModelCapability::Audio {
                    matches!(
                        member.generic_adapter().as_deref(),
                        Some("xai" | "openai-chat" | "openai-responses" | "azure-openai")
                    )
                } else {
                    crate::capability::member_supports_capability(&pool, member, model, capability)
                })
                && crate::models_route::member_matches_scope(
                    member,
                    auth.identity.scoped().map(Arc::as_ref),
                )
                && binding.is_none_or(|binding| {
                    binding
                        .account
                        .as_deref()
                        .is_none_or(|id| id == member.id())
                        && binding
                            .provider
                            .as_deref()
                            .is_none_or(|provider| provider == member.provider_name())
                })
        })
        .collect();
    let candidates: Vec<Arc<dyn PoolMember>> = accounts
        .iter()
        .map(|member| member.clone() as Arc<dyn PoolMember>)
        .collect();
    let member = state
        .router
        .select(&candidates, &SessionHint { affinity_key: None })
        .and_then(|index| accounts.get(index))
        .cloned()
        .ok_or_else(|| error(StatusCode::SERVICE_UNAVAILABLE, "media_account_unavailable"))?;
    let mapped = resolved
        .eligible_bindings
        .iter()
        .find(|binding| binding.provider_id.as_str() == member.provider_name())
        .map(|binding| {
            binding
                .effective_upstream_id(&resolved.canonical_id)
                .to_string()
        })
        .ok_or_else(|| error(StatusCode::BAD_REQUEST, "unsupported_media_model"))?;
    Ok((member, mapped))
}

fn target(member: &AccountMember, speech: bool, path: &str) -> (String, Vec<(String, String)>) {
    if let Some(account) = member.xai_account() {
        let transport = if speech {
            mahoquot_providers::xai::Transport::Speech
        } else {
            mahoquot_providers::xai::Transport::Media
        };
        let base = account.base_url_for(transport, member.upstream_override.as_deref());
        let path = if speech { "/v1/tts" } else { path };
        (
            crate::url::join_provider_path(base, path),
            account.headers(
                transport,
                base,
                false,
                mahoquot_providers::xai::CLIENT_VERSION,
            ),
        )
    } else {
        (
            crate::url::join_provider_path(&member.effective_base_url(), path),
            member.build_upstream_headers(),
        )
    }
}

async fn relay(
    state: Arc<AppState>,
    member: Arc<AccountMember>,
    content_type: &str,
    body: reqwest::Body,
    speech: bool,
    path: &str,
) -> Response {
    let (url, headers) = target(&member, speech, path);
    let client = match state.try_client_for_member(&member) {
        Ok(client) => client,
        Err(_) => return error(StatusCode::BAD_GATEWAY, "media_proxy_invalid"),
    };
    let activity = member.begin_activity();
    let mut request = client
        .post(url)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT, "*/*")
        .body(body);
    for (name, value) in headers {
        if crate::wsrelay::safe_header(&name) {
            request = request.header(name, value);
        }
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(_) => return error(StatusCode::BAD_GATEWAY, "media_upstream_failed"),
    };
    let status = response.status();
    let mut headers = HeaderMap::new();
    for name in [
        header::CONTENT_TYPE,
        header::CONTENT_LENGTH,
        header::CONTENT_DISPOSITION,
        header::CACHE_CONTROL,
        header::RETRY_AFTER,
    ] {
        if let Some(value) = response.headers().get(&name) {
            headers.insert(name, value.clone());
        }
    }
    if !status.is_success()
        && headers.get(header::CONTENT_TYPE).is_none_or(|value| {
            value
                .to_str()
                .ok()
                .is_none_or(|value| !value.contains("json"))
        })
    {
        headers.insert(
            header::CONTENT_TYPE,
            "application/json".parse().expect("static header"),
        );
    }
    let stream = futures::stream::unfold(
        (response.bytes_stream(), activity),
        |(mut upstream, activity)| async move {
            upstream
                .next()
                .await
                .map(|chunk| (chunk, (upstream, activity)))
        },
    );
    (status, headers, Body::from_stream(stream)).into_response()
}

pub async fn speech(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
    headers: HeaderMap,
    raw: Bytes,
) -> Response {
    let mut payload: Value = match serde_json::from_slice::<Value>(&raw) {
        Ok(value) if value.is_object() => value,
        _ => return error(StatusCode::BAD_REQUEST, "invalid_request"),
    };
    let model = payload["model"].as_str().unwrap_or_default().to_owned();
    if model.is_empty()
        || payload
            .get("input")
            .or_else(|| payload.get("text"))
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return error(StatusCode::BAD_REQUEST, "invalid_speech_request");
    }
    if payload.get("stream").and_then(Value::as_bool) == Some(true) {
        return error(StatusCode::BAD_REQUEST, "speech_stream_unsupported");
    }
    let (member, mapped) = match select(&state, &auth, &model, ModelCapability::Audio) {
        Ok(selected) => selected,
        Err(response) => return response,
    };
    let original = payload.clone();
    payload["model"] = json!(mapped);
    if state
        .settings
        .current()
        .payload
        .apply(
            &mut payload,
            &crate::management::settings::PayloadContext {
                model: &mapped,
                requested_model: &model,
                protocol: "openai",
                from_protocol: "openai-speech",
                headers: &headers,
                original: &original,
            },
        )
        .is_err()
    {
        return error(StatusCode::BAD_REQUEST, "invalid_payload");
    }
    relay(
        state,
        member,
        "application/json",
        reqwest::Body::from(payload.to_string()),
        true,
        "/v1/audio/speech",
    )
    .await
}

#[derive(Debug)]
struct Upload {
    path: std::path::PathBuf,
}

impl Drop for Upload {
    fn drop(&mut self) {
        let path = self.path.clone();
        tokio::spawn(async move {
            if let Err(error) = tokio::fs::remove_file(path).await {
                tracing::debug!(%error, "media spool cleanup");
            }
        });
    }
}

fn file_stream(
    file: tokio::fs::File,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Send {
    futures::stream::try_unfold(file, |mut file| async move {
        let mut chunk = vec![0u8; 64 * 1024];
        let size = file.read(&mut chunk).await?;
        if size == 0 {
            return Ok(None);
        }
        chunk.truncate(size);
        Ok(Some((Bytes::from(chunk), file)))
    })
}

// Keep HTTP response errors compatible with the media handlers.
#[allow(clippy::result_large_err)]
async fn spool(body: Body) -> Result<Upload, Response> {
    let path =
        std::env::temp_dir().join(format!("mahoquot-image-{}.multipart", uuid::Uuid::new_v4()));
    let mut options = tokio::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&path)
        .await
        .map_err(|_| error(StatusCode::INSUFFICIENT_STORAGE, "media_spool_failed"))?;
    let upload = Upload { path };
    let mut stream = body.into_data_stream();
    let mut size = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| error(StatusCode::BAD_REQUEST, "invalid_multipart"))?;
        size = size.saturating_add(chunk.len() as u64);
        if size > MAX_UPLOAD {
            return Err(error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "media_upload_too_large",
            ));
        }
        file.write_all(&chunk)
            .await
            .map_err(|_| error(StatusCode::INSUFFICIENT_STORAGE, "media_spool_failed"))?;
    }
    file.flush()
        .await
        .map_err(|_| error(StatusCode::INSUFFICIENT_STORAGE, "media_spool_failed"))?;
    Ok(upload)
}

// Keep HTTP response errors compatible with the media handlers.
#[allow(clippy::result_large_err)]
async fn multipart_model(upload: &Upload, boundary: String) -> Result<String, Response> {
    let file = tokio::fs::File::open(&upload.path)
        .await
        .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "media_spool_failed"))?;
    let mut multipart = multer::Multipart::new(file_stream(file), boundary);
    let mut model = None;
    let mut images = 0;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|_| error(StatusCode::BAD_REQUEST, "invalid_multipart"))?
    {
        let name = field.name().unwrap_or_default().to_owned();
        let is_file = field.file_name().is_some();
        if is_file && matches!(name.as_str(), "image" | "image[]" | "mask") {
            images += 1;
        }
        let mut value = Vec::new();
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|_| error(StatusCode::BAD_REQUEST, "invalid_multipart"))?
        {
            if !is_file {
                if value.len().saturating_add(chunk.len()) > MAX_FIELD {
                    return Err(error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "multipart_field_too_large",
                    ));
                }
                value.extend_from_slice(&chunk);
            }
        }
        if name == "model" && !is_file {
            if model.is_some() {
                return Err(error(StatusCode::BAD_REQUEST, "duplicate_model"));
            }
            model = Some(
                String::from_utf8(value)
                    .map_err(|_| error(StatusCode::BAD_REQUEST, "invalid_model"))?,
            );
        }
    }
    model
        .filter(|model| !model.trim().is_empty())
        .filter(|_| images > 0)
        .ok_or_else(|| error(StatusCode::BAD_REQUEST, "invalid_image_edit"))
}

pub async fn image_edits(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    if !content_type.starts_with("multipart/form-data") {
        let raw = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
            Ok(raw) => raw,
            Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "media_upload_too_large"),
        };
        let mut payload: Value = match serde_json::from_slice::<Value>(&raw) {
            Ok(value) if value.is_object() => value,
            _ => return error(StatusCode::BAD_REQUEST, "invalid_request"),
        };
        let model = payload["model"].as_str().unwrap_or_default().to_owned();
        let (member, mapped) = match select(&state, &auth, &model, ModelCapability::Image) {
            Ok(selected) => selected,
            Err(response) => return response,
        };
        let original = payload.clone();
        payload["model"] = json!(mapped);
        if state
            .settings
            .current()
            .payload
            .apply(
                &mut payload,
                &crate::management::settings::PayloadContext {
                    model: &mapped,
                    requested_model: &model,
                    protocol: "openai",
                    from_protocol: "openai",
                    headers: &parts.headers,
                    original: &original,
                },
            )
            .is_err()
        {
            return error(StatusCode::BAD_REQUEST, "invalid_payload");
        }
        return relay(
            state,
            member,
            "application/json",
            reqwest::Body::from(payload.to_string()),
            false,
            "/v1/images/edits",
        )
        .await;
    }
    let boundary = match multer::parse_boundary(&content_type) {
        Ok(boundary) => boundary,
        Err(_) => return error(StatusCode::BAD_REQUEST, "invalid_multipart"),
    };
    let upload = match spool(body).await {
        Ok(upload) => upload,
        Err(response) => return response,
    };
    let model = match multipart_model(&upload, boundary).await {
        Ok(model) => model,
        Err(response) => return response,
    };
    let (member, mapped) = match select(&state, &auth, &model, ModelCapability::Image) {
        Ok(selected) => selected,
        Err(response) => return response,
    };
    if mapped != model {
        return error(
            StatusCode::BAD_REQUEST,
            "multipart_model_requires_upstream_id",
        );
    }
    let file = match tokio::fs::File::open(&upload.path).await {
        Ok(file) => file,
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "media_spool_failed"),
    };
    let response = relay(
        state,
        member,
        &content_type,
        reqwest::Body::wrap_stream(file_stream(file)),
        false,
        "/v1/images/edits",
    )
    .await;
    drop(upload);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xai_speech_uses_official_native_transport() {
        let member = AccountMember::for_test(crate::account::ProviderAccount::Generic(
            crate::account::GenericAccount {
                provider: "xai".into(),
                adapter: "xai".into(),
                auth_mode: "oauth".into(),
                api_key: "fixture".into(),
                base_url: mahoquot_providers::xai::CHAT_BASE.into(),
                ..Default::default()
            },
        ));
        let (url, headers) = target(&member, true, "/v1/audio/speech");
        assert_eq!(url, "https://api.x.ai/v1/tts");
        assert!(headers
            .iter()
            .any(|(name, value)| name == "authorization" && value == "Bearer fixture"));
        assert!(!headers.iter().any(|(name, _)| name == "x-xai-token-auth"));
    }

    #[tokio::test]
    async fn multipart_parser_preserves_binary_spool_and_model_after_file() {
        let mut raw = b"--B\r\nContent-Disposition: form-data; name=\"image\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n".to_vec();
        raw.extend_from_slice(&[0, 255, 128, 0, 13, 10]);
        raw.extend_from_slice(b"\r\n--B\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nfixture-image\r\n--B--\r\n");
        let upload = spool(Body::from(raw.clone())).await.unwrap();
        let model = multipart_model(&upload, "B".into()).await.unwrap();
        assert_eq!(model, "fixture-image");
        assert_eq!(tokio::fs::read(&upload.path).await.unwrap(), raw);
        tokio::fs::remove_file(&upload.path).await.unwrap();
    }

    #[tokio::test]
    async fn malformed_multipart_is_rejected_before_upstream_dispatch() {
        let upload = spool(Body::from("--B\r\ninvalid")).await.unwrap();
        let response = multipart_model(&upload, "B".into()).await.unwrap_err();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        tokio::fs::remove_file(&upload.path).await.unwrap();
    }

    #[tokio::test]
    async fn duplicate_model_fields_are_rejected() {
        let raw = "--B\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nx\r\n--B\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ny\r\n--B--\r\n";
        let upload = spool(Body::from(raw)).await.unwrap();
        assert_eq!(
            multipart_model(&upload, "B".into())
                .await
                .unwrap_err()
                .status(),
            StatusCode::BAD_REQUEST
        );
        tokio::fs::remove_file(&upload.path).await.unwrap();
    }

    #[tokio::test]
    async fn video_binding_lifecycle_and_account_pinning() {
        let member = AccountMember::for_test(crate::account::ProviderAccount::Generic(
            crate::account::GenericAccount {
                provider: "xai".into(),
                adapter: "xai".into(),
                auth_mode: "oauth".into(),
                api_key: "fixture".into(),
                base_url: mahoquot_providers::xai::CHAT_BASE.into(),
                ..Default::default()
            },
        ));
        bind_video(
            "vid_test_123",
            Some("caller_a"),
            &member,
            "grok-imagine-video",
        )
        .await;
        let bound = bound_video("vid_test_123").await.expect("bound entry");
        assert_eq!(bound.caller_key.as_deref(), Some("caller_a"));
        assert_eq!(bound.account_id, member.id());
        assert_eq!(bound.provider, "xai");
        assert_eq!(bound.model, "grok-imagine-video");
    }

    #[tokio::test]
    async fn video_binding_cross_caller_isolation() {
        let member = AccountMember::for_test(crate::account::ProviderAccount::Generic(
            crate::account::GenericAccount {
                provider: "xai".into(),
                adapter: "xai".into(),
                auth_mode: "oauth".into(),
                api_key: "fixture".into(),
                base_url: mahoquot_providers::xai::CHAT_BASE.into(),
                ..Default::default()
            },
        ));
        bind_video(
            "vid_isolated",
            Some("caller_one"),
            &member,
            "grok-imagine-video",
        )
        .await;
        let bound = bound_video("vid_isolated").await.expect("bound entry");
        assert_eq!(bound.caller_key.as_deref(), Some("caller_one"));
        assert_ne!(bound.caller_key.as_deref(), Some("caller_two"));
    }

    #[test]
    fn video_status_normalizes_aliases() {
        let mut payload = json!({"status": "pending", "progress": 10});
        video_status(&mut payload);
        assert_eq!(payload["status"], "queued");

        let mut payload_running = json!({"status": "processing"});
        video_status(&mut payload_running);
        assert_eq!(payload_running["status"], "in_progress");

        let mut payload_done = json!({"status": "succeeded"});
        video_status(&mut payload_done);
        assert_eq!(payload_done["status"], "completed");

        let mut payload_err = json!({"status": "canceled"});
        video_status(&mut payload_err);
        assert_eq!(payload_err["status"], "failed");
    }

    #[test]
    fn video_upstream_path_mapping() {
        assert_eq!(
            video_upstream_path("/v1/videos/generations"),
            "/videos/generations"
        );
        assert_eq!(video_upstream_path("/v1/videos/edits"), "/videos/edits");
        assert_eq!(
            video_upstream_path("/videos/extensions"),
            "/videos/extensions"
        );
        assert_eq!(video_upstream_path("/v1/videos"), "/videos/generations");
    }
}
