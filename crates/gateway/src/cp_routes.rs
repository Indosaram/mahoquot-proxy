//! Handlers for the CLIProxyAPI surface beyond the OpenAI/Anthropic chat routes.
//!
//! Several of these are permanent errors rather than relays, and that is the
//! correct implementation: CLIProxyAPI returns the same errors against this
//! credential pool because the ChatGPT/Codex OAuth upstream has no SIP,
//! translation, transcription, image or video capability. Each such handler
//! mirrors the status and body CLIProxyAPI v7.2.140 produced, so a client cannot
//! tell the two proxies apart. Where a route *is* backed by a real upstream it
//! relays for real - see `responses`, `v1beta_generate` and `v1beta_models`.

use std::sync::Arc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use axum::body::Body;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use mahoquot_registry::ModelCapability;
use mahoquot_types::PoolMember;
use serde_json::{json, Value};

use crate::capability::{self, model_of};
use crate::inbound::ResolvedAuth;
use crate::realtime;
use crate::relay::{handle_relay, RelayMode};
use crate::state::AppState;
use crate::static_pages::{CALLBACK_HTML, MANAGEMENT_HTML, ROOT_JSON};
use crate::v1beta::{self, GeminiAction};
use axum::Extension;

const CODEX_RESPONSES_PATH: &str = "/backend-api/codex/responses";
const CODEX_RESPONSES_COMPACT_PATH: &str = "/backend-api/codex/responses/compact";

fn json_status(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

fn parse_body(body: &Bytes) -> Result<Value, Box<Response>> {
    if body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(body).map_err(|e| {
        Box::new(json_status(
            StatusCode::BAD_REQUEST,
            json!({"error": {"message": e.to_string(), "type": "invalid_request_error"}}),
        ))
    })
}

pub(crate) fn owner_of(state: &AppState, model: &str) -> Option<String> {
    state
        .pool
        .load()
        .models
        .iter()
        .find(|m| m.id == model)
        .map(|m| m.owned_by.clone())
}

pub async fn root() -> Response {
    (
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        ROOT_JSON,
    )
        .into_response()
}

pub async fn management_html() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        MANAGEMENT_HTML,
    )
        .into_response()
}

pub async fn oauth_callback() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        CALLBACK_HTML,
    )
        .into_response()
}

pub async fn images_generations(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    image_surface(state, &auth, &headers, body).await
}
pub use crate::media::image_edits as images_edits;

fn multipart_field(text: &str, name: &str) -> Option<String> {
    let marker = format!("name=\"{name}\"");
    let start = text.find(&marker)? + marker.len();
    let rest = &text[start..];
    let value_start = rest.find("\r\n\r\n")? + 4;
    let value = &rest[value_start..];
    let end = value.find("\r\n")?;
    Some(value[..end].to_string())
}

async fn image_surface(
    state: Arc<AppState>,
    auth: &ResolvedAuth,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    let parsed = match parse_body(&body) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let model = model_of(&parsed);
    let resolved = {
        let snapshot = state.pool.load();
        capability::resolve_for_capability(&snapshot, model, ModelCapability::Image)
    };
    let Some(resolved) = resolved else {
        let snapshot = state.pool.load();
        return json_status(
            StatusCode::BAD_REQUEST,
            capability::check_image(&snapshot, model).expect("missing image capability"),
        );
    };
    let mut canonical_body = parsed;
    if let Some(object) = canonical_body.as_object_mut() {
        object.insert("model".to_string(), json!(resolved.canonical_id.as_str()));
    }
    handle_relay(
        state,
        auth,
        RelayMode::Image,
        "/v1/images/generations",
        headers,
        Bytes::from(canonical_body.to_string()),
    )
    .await
}
pub use crate::media::openai_videos;
pub use crate::media::speech;
pub use crate::media::videos;
pub use crate::media::videos_by_id;

fn realtime_model(body: &Value) -> &str {
    body.get("session")
        .and_then(|session| session.get("model"))
        .and_then(Value::as_str)
        .or_else(|| body.get("model").and_then(Value::as_str))
        .filter(|model| !model.is_empty())
        .unwrap_or("gpt-realtime")
}

fn realtime_allowed(state: &AppState, body: &Value) -> bool {
    let model = realtime_model(body);
    if model == "gpt-realtime" {
        return true;
    }
    capability::resolve_registry_capability(&state.pool.load(), model, ModelCapability::Realtime)
        .is_some()
}

pub struct RealtimeCall {
    pub call_id: String,
    pub owner_key: Option<String>,
    pub relay: Arc<crate::realtime::media::MediaRelay>,
    pub member: Arc<crate::account::AccountMember>,
    pub _activity: crate::account::AccountActivity,
    pub upstream_ws: String,
    pub authorization: String,
    pub issuer: String,
}

async fn select_realtime_member(
    state: &AppState,
    model: &str,
    auth: &ResolvedAuth,
    pinned: Option<&str>,
) -> Option<Arc<crate::account::AccountMember>> {
    let pool = state.pool.load_full();
    let settings = state.settings.current();
    let binding = settings
        .api_key_bindings
        .iter()
        .find(|binding| Some(&binding.key_identifier) == auth.key_identifier.as_ref());
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0);
    let candidates: Vec<_> = pool
        .members
        .iter()
        .filter(|member| {
            member.kind() == crate::account::ProviderKind::Codex
                && pinned.is_none_or(|id| member.id() == id)
                && member.health().is_available(now_ms)
                && member.supports_model(model)
                && state.scheduler.permits(member.id())
                && crate::models_route::member_matches_scope(
                    member,
                    auth.identity.scoped().map(AsRef::as_ref),
                )
                && binding.is_none_or(|binding| {
                    binding
                        .account
                        .as_deref()
                        .is_none_or(|id| member.id() == id)
                        && binding
                            .provider
                            .as_deref()
                            .is_none_or(|provider| member.provider_name() == provider)
                })
        })
        .cloned()
        .collect();
    let routable: Vec<Arc<dyn PoolMember>> = candidates
        .iter()
        .cloned()
        .map(|member| member as Arc<dyn PoolMember>)
        .collect();
    let selected = state.router.select(
        &routable,
        &mahoquot_types::SessionHint {
            affinity_key: auth.key_identifier.clone(),
        },
    )?;
    let picked = candidates.get(selected)?.clone();
    pool.members.iter().find(|m| m.id() == picked.id()).cloned()
}

fn realtime_calls() -> &'static tokio::sync::RwLock<
    std::collections::HashMap<String, Arc<tokio::sync::Mutex<RealtimeCall>>>,
> {
    static CALLS: std::sync::OnceLock<
        tokio::sync::RwLock<
            std::collections::HashMap<String, Arc<tokio::sync::Mutex<RealtimeCall>>>,
        >,
    > = std::sync::OnceLock::new();
    CALLS.get_or_init(|| tokio::sync::RwLock::new(std::collections::HashMap::new()))
}

fn realtime_upstream_url(member: &crate::account::AccountMember, call: bool) -> String {
    let base = member.effective_base_url();
    let path = if call {
        "/v1/realtime/calls"
    } else {
        "/v1/realtime/client_secrets"
    };
    format!("{}{}", base.trim_end_matches('/'), path)
}

fn realtime_sideband_url(location: &str, base_url: &str) -> String {
    let suffix = location
        .split("/v1/realtime/calls/")
        .nth(1)
        .unwrap_or_default();
    let scheme = if base_url.starts_with("https://") {
        "wss://"
    } else {
        "ws://"
    };
    let host = base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    format!("{scheme}{host}/v1/realtime/calls/{suffix}")
}

async fn realtime_upstream_offer(
    member: &crate::account::AccountMember,
    sdp: &str,
    session: &Value,
) -> Result<reqwest::Response, String> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| e.to_string())?;
    // The relay's own offer goes upstream as the documented JSON call body
    // ({sdp, session}) so the client's session options survive the hop.
    let session = session
        .get("session")
        .cloned()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let mut request = client
        .post(realtime_upstream_url(member, true))
        .json(&json!({ "sdp": sdp, "session": session }));
    for (name, value) in member.build_upstream_headers() {
        request = request.header(name, value);
    }
    request.send().await.map_err(|e| e.to_string())
}

pub async fn realtime_offer(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
    Extension(store): Extension<Arc<realtime::RealtimeStore>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let secret_value = crate::inbound::presented_api_key(&headers).unwrap_or_default();
    let secret = if secret_value.starts_with("ek_") {
        store.lookup(secret_value).await
    } else {
        None
    };
    if secret_value.starts_with("ek_") && secret.is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let (sdp, parsed) = match parse_realtime_payload(content_type, &body) {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    if let Some(err) = realtime::validate_offer(&parsed) {
        return json_status(StatusCode::BAD_REQUEST, err);
    }
    let (model, member_id, issuer) = if let Some(secret) = secret {
        if secret.issuer != auth.key_identifier.clone().unwrap_or_default() {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        (
            secret.model.clone(),
            secret.member_id.clone(),
            secret.issuer.clone(),
        )
    } else {
        if !realtime_allowed(&state, &parsed) {
            return json_status(
                StatusCode::NOT_IMPLEMENTED,
                realtime::capability_not_supported("Realtime calls"),
            );
        }
        (
            realtime_model(&parsed).to_string(),
            None,
            auth.key_identifier
                .clone()
                .unwrap_or_else(|| "unrestricted".into()),
        )
    };
    let Some(member) = select_realtime_member(&state, &model, &auth, member_id.as_deref()).await
    else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Ok((relay, upstream_offer)) = crate::realtime::media::MediaRelay::offer(&sdp).await else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let upstream = match realtime_upstream_offer(&member, &upstream_offer, &parsed).await {
        Ok(value) => value,
        Err(_) => {
            relay.close().await;
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    if upstream.status() != StatusCode::CREATED {
        let status = upstream.status();
        let bytes = upstream.bytes().await.unwrap_or_default();
        relay.close().await;
        return (status, bytes).into_response();
    }
    let location = upstream
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let Some(call_id) = location
        .rsplit('/')
        .next()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
    else {
        relay.close().await;
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let answer = match upstream.text().await {
        Ok(answer) => answer,
        Err(_) => {
            relay.close().await;
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let client_answer = match relay.answer(&answer).await {
        Ok(answer) => answer,
        Err(_) => {
            relay.close().await;
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let call = RealtimeCall {
        call_id: call_id.clone(),
        owner_key: Some(issuer.clone()),
        relay,
        member: member.clone(),
        _activity: member.begin_activity(),
        upstream_ws: realtime_sideband_url(&location, &member.effective_base_url()),
        authorization: member.access_token(),
        issuer,
    };
    realtime_calls()
        .write()
        .await
        .insert(call_id.clone(), Arc::new(tokio::sync::Mutex::new(call)));
    Response::builder()
        .status(StatusCode::CREATED)
        .header(header::CONTENT_TYPE, "application/sdp")
        .header(header::LOCATION, format!("/v1/realtime/calls/{call_id}"))
        .body(Body::from(client_answer))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn realtime_error_body(message: impl Into<String>) -> Response {
    json_status(
        StatusCode::BAD_REQUEST,
        json!({"error":{"message":message.into(),"type":"invalid_request_error"}}),
    )
}

fn requested_secret_ttl(body: &Value) -> u64 {
    body.get("expires_after")
        .and_then(Value::as_u64)
        .unwrap_or(realtime::DEFAULT_EPHEMERAL_TTL_SECS)
}

// Preserve the response-valued error contract shared with the HTTP handler.
#[allow(clippy::result_large_err)]
fn parse_realtime_payload(content_type: &str, body: &[u8]) -> Result<(String, Value), Response> {
    if body.len() > realtime::MAX_SDP_BYTES {
        return Err(StatusCode::PAYLOAD_TOO_LARGE.into_response());
    }
    if content_type.starts_with("application/sdp") || body.starts_with(b"v=") {
        let sdp =
            std::str::from_utf8(body).map_err(|_| realtime_error_body("invalid utf-8 in sdp"))?;
        return Ok((sdp.to_owned(), json!({"sdp": sdp, "session": {}})));
    }
    if content_type.starts_with("multipart/form-data") {
        let text = std::str::from_utf8(body)
            .map_err(|_| realtime_error_body("invalid multipart encoding"))?;
        let sdp = multipart_field(text, "sdp").ok_or_else(|| realtime_error_body("missing sdp"))?;
        let options = multipart_field(text, "session")
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_else(|| json!({}));
        return Ok((sdp.clone(), json!({"sdp": sdp, "session": options})));
    }
    if body.len() > realtime::MAX_SECRET_BODY_BYTES {
        return Err(StatusCode::PAYLOAD_TOO_LARGE.into_response());
    }
    let parsed: Value =
        serde_json::from_slice(body).map_err(|e| realtime_error_body(e.to_string()))?;
    let Some(sdp) = parsed.get("sdp").and_then(Value::as_str) else {
        return Err(realtime_error_body("Field `sdp` must be a string"));
    };
    if !parsed.get("session").is_some_and(Value::is_object) {
        return Err(realtime_error_body("Field `session` must be an object"));
    }
    if sdp.len() > realtime::MAX_SDP_BYTES {
        return Err(StatusCode::PAYLOAD_TOO_LARGE.into_response());
    }
    Ok((sdp.to_owned(), parsed))
}

pub async fn realtime_client_secrets(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
    Extension(store): Extension<Arc<realtime::RealtimeStore>>,
    body: Bytes,
) -> Response {
    if body.len() > realtime::MAX_SECRET_BODY_BYTES {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let parsed = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    if !realtime_allowed(&state, &parsed) {
        return json_status(
            StatusCode::NOT_IMPLEMENTED,
            realtime::capability_not_supported("Realtime client secrets"),
        );
    }
    let model = realtime_model(&parsed).to_string();
    let Some(member) = select_realtime_member(&state, &model, &auth, None).await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let issuer = auth
        .key_identifier
        .clone()
        .unwrap_or_else(|| "unrestricted".into());
    let (value, expires_at) = match store
        .mint(
            issuer,
            model,
            auth.identity.scoped().cloned(),
            Some(member.id().to_string()),
            requested_secret_ttl(&parsed),
        )
        .await
    {
        Ok(secret) => secret,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    Json(realtime::client_secret(&parsed, value, expires_at)).into_response()
}

pub async fn realtime_sessions(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
    Extension(store): Extension<Arc<realtime::RealtimeStore>>,
    body: Bytes,
) -> Response {
    if body.len() > realtime::MAX_SECRET_BODY_BYTES {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let parsed = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    if !realtime_allowed(&state, &parsed) {
        return json_status(
            StatusCode::NOT_IMPLEMENTED,
            realtime::capability_not_supported("Realtime sessions"),
        );
    }
    let model = realtime_model(&parsed).to_string();
    let Some(member) = select_realtime_member(&state, &model, &auth, None).await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let issuer = auth
        .key_identifier
        .clone()
        .unwrap_or_else(|| "unrestricted".into());
    let (value, expires_at) = match store
        .mint(
            issuer,
            model,
            auth.identity.scoped().cloned(),
            Some(member.id().to_string()),
            requested_secret_ttl(&parsed),
        )
        .await
    {
        Ok(secret) => secret,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    Json(realtime::legacy_session(&parsed, value, expires_at)).into_response()
}

pub async fn realtime_transcription() -> Response {
    json_status(
        StatusCode::NOT_IMPLEMENTED,
        realtime::capability_not_supported("Realtime transcription-only sessions"),
    )
}

pub async fn realtime_translations() -> Response {
    json_status(
        StatusCode::NOT_IMPLEMENTED,
        realtime::capability_not_supported("Realtime translation sessions"),
    )
}

pub async fn realtime_hangup(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
    Path(call_id): Path<String>,
) -> Response {
    let call = realtime_calls().read().await.get(&call_id).cloned();
    let Some(call) = call else {
        return json_status(StatusCode::NOT_FOUND, realtime::call_not_found());
    };
    let guard = call.lock().await;
    if guard.owner_key.as_deref() != auth.key_identifier.as_deref() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let ws_url = guard
        .upstream_ws
        .trim_start_matches("wss://")
        .trim_start_matches("ws://");
    let url = format!(
        "{}://{ws_url}/hangup",
        if guard.upstream_ws.starts_with("wss://") {
            "https"
        } else {
            "http"
        }
    );
    let mut request = state.direct_client().post(url);
    if let Ok(value) = guard.authorization.parse::<axum::http::HeaderValue>() {
        request = request.header(header::AUTHORIZATION, value);
    }
    let forwarded = request.send().await;
    if forwarded.as_ref().is_err()
        || forwarded.is_ok_and(|response| !response.status().is_success())
    {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    guard.relay.close().await;
    drop(guard);
    realtime_calls().write().await.remove(&call_id);
    StatusCode::OK.into_response()
}

pub async fn realtime_sip_accept() -> Response {
    json_status(
        StatusCode::NOT_IMPLEMENTED,
        realtime::capability_not_supported("Realtime SIP accept"),
    )
}

pub async fn realtime_sip_reject() -> Response {
    json_status(
        StatusCode::NOT_IMPLEMENTED,
        realtime::capability_not_supported("Realtime SIP reject"),
    )
}

pub async fn realtime_sip_refer() -> Response {
    json_status(
        StatusCode::NOT_IMPLEMENTED,
        realtime::capability_not_supported("Realtime SIP refer"),
    )
}

pub async fn realtime_sip(Path((_call_id, action)): Path<(String, String)>) -> Response {
    json_status(
        StatusCode::NOT_IMPLEMENTED,
        realtime::capability_not_supported(&format!("Realtime SIP {action}")),
    )
}

type MaybeUpgrade = Result<WebSocketUpgrade, WebSocketUpgradeRejection>;

pub async fn realtime_call_get(ws: MaybeUpgrade) -> Response {
    match ws {
        Ok(_) => json_status(
            StatusCode::UPGRADE_REQUIRED,
            realtime::upgrade_required_nested(),
        ),
        Err(_) => json_status(
            StatusCode::UPGRADE_REQUIRED,
            realtime::upgrade_required_nested(),
        ),
    }
}

async fn sideband_upgrade(
    ws: WebSocketUpgrade,
    call: Arc<tokio::sync::Mutex<RealtimeCall>>,
) -> Response {
    ws.on_upgrade(move |mut client| async move {
        let (url, authorization, relay) = {
            let call = call.lock().await;
            (call.upstream_ws.clone(), call.authorization.clone(), call.relay.clone())
        };
        let mut request = match url.into_client_request() { Ok(request) => request, Err(_) => { relay.close().await; return; } };
        let Ok(value) = authorization.parse() else { relay.close().await; return; };
        request.headers_mut().insert(header::AUTHORIZATION, value);
        let (mut upstream, _) = match tokio_tungstenite::connect_async(request).await { Ok(connection) => connection, Err(_) => { relay.close().await; return; } };
        // The call's lifetime owns the sideband: a hangup (or media failure)
        // closes the relay and must close this socket too.
        let mut cancelled = relay.cancellation();
        loop {
            tokio::select! {
                changed = cancelled.changed() => {
                    if changed.is_err() || *cancelled.borrow() {
                        let _ = client.send(axum::extract::ws::Message::Close(None)).await;
                        let _ = upstream.close(None).await;
                        break;
                    }
                },
                incoming = client.recv() => match incoming {
                    Some(Ok(message)) => {
                        let message = match message {
                            axum::extract::ws::Message::Text(text) => tokio_tungstenite::tungstenite::Message::Text(text.to_string().into()),
                            axum::extract::ws::Message::Binary(data) => tokio_tungstenite::tungstenite::Message::Binary(data.to_vec().into()),
                            axum::extract::ws::Message::Ping(data) => tokio_tungstenite::tungstenite::Message::Ping(data.to_vec().into()),
                            axum::extract::ws::Message::Pong(data) => tokio_tungstenite::tungstenite::Message::Pong(data.to_vec().into()),
                            axum::extract::ws::Message::Close(_) => break,
                        };
                        if upstream.send(message).await.is_err() { break; }
                    }
                    _ => break,
                },
                outgoing = upstream.next() => match outgoing {
                    Some(Ok(message)) => {
                        let message = match message {
                            tokio_tungstenite::tungstenite::Message::Text(text) => axum::extract::ws::Message::Text(text.to_string().into()),
                            tokio_tungstenite::tungstenite::Message::Binary(data) => axum::extract::ws::Message::Binary(data.to_vec().into()),
                            tokio_tungstenite::tungstenite::Message::Ping(data) => axum::extract::ws::Message::Ping(data.to_vec().into()),
                            tokio_tungstenite::tungstenite::Message::Pong(data) => axum::extract::ws::Message::Pong(data.to_vec().into()),
                            tokio_tungstenite::tungstenite::Message::Close(_) => break,
                            _ => continue,
                        };
                        if client.send(message).await.is_err() { break; }
                    }
                    _ => break,
                }
            }
        }
        relay.close().await;
    }).into_response()
}

pub async fn realtime_call_get_owned(
    Extension(auth): Extension<ResolvedAuth>,
    Path(call_id): Path<String>,
    ws: MaybeUpgrade,
) -> Response {
    let calls = realtime_calls().read().await;
    let Some(call) = calls.get(&call_id).cloned() else {
        return json_status(StatusCode::NOT_FOUND, realtime::call_not_found());
    };
    if call.lock().await.owner_key.as_deref() != auth.key_identifier.as_deref() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let ws = match ws {
        Ok(ws) => ws,
        Err(_) => {
            return json_status(
                StatusCode::UPGRADE_REQUIRED,
                realtime::upgrade_required_nested(),
            )
        }
    };
    sideband_upgrade(ws, call).await
}

pub async fn live_sideband(
    Extension(auth): Extension<ResolvedAuth>,
    Path(call_id): Path<String>,
    ws: MaybeUpgrade,
) -> Response {
    let calls = realtime_calls().read().await;
    let Some(call) = calls.get(&call_id).cloned() else {
        return json_status(StatusCode::NOT_FOUND, realtime::call_not_found());
    };
    if call.lock().await.owner_key.as_deref() != auth.key_identifier.as_deref() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let ws = match ws {
        Ok(ws) => ws,
        Err(_) => {
            return json_status(
                StatusCode::UPGRADE_REQUIRED,
                realtime::upgrade_required_flat(),
            );
        }
    };
    sideband_upgrade(ws, call).await
}

/// The three GET upgrade endpoints answer 101 so a client's handshake succeeds,
/// then close, because no upstream duplex session is established.
/// Responses/realtime WebSocket (Task 19): a real upgrade owned by
/// `wsrelay`, which dials the upstream before answering 101 and pins the
/// session to one account. A plain GET without an upgrade stays 426.
pub async fn ws_upgrade(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<ResolvedAuth>>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    ws: MaybeUpgrade,
) -> Response {
    let Ok(ws) = ws else {
        return json_status(
            StatusCode::UPGRADE_REQUIRED,
            realtime::upgrade_required_nested(),
        );
    };
    // `ws-auth` is honoured at runtime: the inbound gate already resolved a
    // presented key; otherwise the upgrade is authenticated here (subprotocol
    // keys included) and refused when ws-auth requires a key.
    let ws_auth = state.settings.current().ws_auth;
    let auth = match auth {
        Some(Extension(auth)) => auth,
        None => match crate::wsrelay::authenticate_upgrade(&state.api_keys, ws_auth, &headers) {
            Some(auth) => auth,
            None => {
                return json_status(
                    StatusCode::UNAUTHORIZED,
                    json!({"error":{"type":"invalid_request_error","code":"invalid_api_key"}}),
                )
            }
        },
    };
    crate::wsrelay::upgrade(state, auth, uri, headers, ws).await
}

/// Codex speaks the Responses protocol natively, so those accounts get a
/// passthrough. Antigravity does not, so the Responses `input` is normalised
/// into chat messages and the reply is re-rendered as a Responses object.
pub async fn responses(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let parsed = match parse_body(&body) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let model = model_of(&parsed).to_string();

    // Codex speaks Responses natively: its events pass through untouched.
    // Every other provider goes through the dedicated Responses translation
    // (request -> provider wire, stream rendered locally with monotonic
    // sequence numbers), never an assumed Codex URL or a buffered re-wrap
    // (Task 9).
    // An owner the catalog does not resolve stays on the Codex passthrough,
    // as before Task 9; only a known non-Codex provider is translated.
    let native = matches!(
        owner_of(&state, &model).as_deref(),
        None | Some("openai" | "codex")
    );
    if native {
        return handle_relay(
            state,
            &auth,
            RelayMode::Native,
            CODEX_RESPONSES_PATH,
            &headers,
            body,
        )
        .await;
    }
    if let Err(error) = crate::compat::responses::responses_to_openai(&parsed) {
        return json_status(StatusCode::BAD_REQUEST, error.to_json_value());
    }
    handle_relay(
        state,
        &auth,
        RelayMode::Responses,
        "/v1/responses",
        &headers,
        body,
    )
    .await
}

const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// `compact` resolves the model first, so an unknown model is a 400 while a
/// known non-codex model reaches the "not supported" branch.
pub async fn responses_compact(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let parsed = match parse_body(&body) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let model = model_of(&parsed).to_string();
    let model = model.as_str();
    match owner_of(&state, model) {
        None => json_status(StatusCode::BAD_REQUEST, capability::unknown_provider(model)),
        Some(owner) if owner == "openai" => {
            handle_relay(
                state,
                &auth,
                RelayMode::Native,
                CODEX_RESPONSES_COMPACT_PATH,
                &headers,
                body,
            )
            .await
        }
        Some(owner) if matches!(owner.as_str(), "anthropic" | "google" | "antigravity") => {
            compact_via_summary(state, auth, headers, parsed, model.to_string()).await
        }
        Some(_) => json_status(
            StatusCode::BAD_REQUEST,
            json!({"error": {
                "message": format!("/responses/compact is not available for model '{model}'"),
                "type": "invalid_request_error",
                "param": "model",
                "code": "unsupported_capability",
            }}),
        ),
    }
}

/// Non-Codex compaction (Task 25): the provider summarises the conversation
/// through the regular Responses translation, and the summary is returned as
/// a sealed, model-bound `cpa-ag-compact-v1` capsule that a later
/// `/v1/responses` turn unseals (tampering is rejected there).
async fn compact_via_summary(
    state: Arc<AppState>,
    auth: ResolvedAuth,
    headers: HeaderMap,
    parsed: Value,
    model: String,
) -> Response {
    let mut request = parsed.clone();
    if let Some(object) = request.as_object_mut() {
        object.insert("stream".into(), json!(false));
        object.remove("store");
    }
    if let Err(error) = crate::compat::responses::responses_to_openai(&request) {
        return json_status(StatusCode::BAD_REQUEST, error.to_json_value());
    }
    let relayed = handle_relay(
        state,
        &auth,
        RelayMode::Responses,
        "/v1/responses",
        &headers,
        Bytes::from(request.to_string()),
    )
    .await;
    let (parts, body) = relayed.into_parts();
    let Ok(raw) = axum::body::to_bytes(body, MAX_RESPONSE_BYTES).await else {
        return json_status(
            StatusCode::BAD_GATEWAY,
            json!({"error": {"message": "upstream reply too large", "type": "server_error"}}),
        );
    };
    if !parts.status.is_success() {
        return (parts.status, raw).into_response();
    }
    let Ok(reply) = serde_json::from_slice::<Value>(&raw) else {
        return json_status(
            StatusCode::BAD_GATEWAY,
            json!({"error": {"message": "invalid compaction summary reply", "type": "server_error"}}),
        );
    };
    let summary: String = reply["output"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "message")
        .flat_map(|item| item["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|part| part["text"].as_str().map(str::to_string))
        .collect::<Vec<_>>()
        .join("");
    let capsule = match crate::compat::responses::seal_compaction_capsule(&summary, &model) {
        Ok(capsule) => capsule,
        Err(error) => {
            return json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": {"message": error, "type": "server_error"}}),
            )
        }
    };
    Json(json!({
        "id": reply.get("id").cloned().unwrap_or_else(|| json!("resp_compact")),
        "object": "response.compaction",
        "created_at": reply.get("created_at").cloned().unwrap_or(json!(0)),
        "output": [{
            "id": "cmp_0",
            "type": "compaction",
            "encrypted_content": capsule,
        }],
        "usage": reply.get("usage").cloned().unwrap_or(Value::Null),
    }))
    .into_response()
}

/// Relayed rather than answered locally: the upstream owns this decision and
/// returns its own HTML body, which a synthesised JSON error would not match.
pub async fn alpha_search(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let parsed = match parse_body(&body) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let model = model_of(&parsed);
    if owner_of(&state, model).as_deref() != Some("openai") {
        return json_status(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "error": "auth_not_found: no auth available" }),
        );
    }
    handle_relay(
        state,
        &auth,
        RelayMode::Native,
        "/backend-api/codex/alpha/search",
        &headers,
        body,
    )
    .await
}

pub async fn v1beta_models(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
) -> Response {
    let pool = state.pool.load();
    let models = crate::models_route::resolve_visible_models(&pool, auth.identity.scoped());
    Json(v1beta::models_payload(&models)).into_response()
}

pub async fn v1beta_action(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
    Path(action): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (model, verb) = v1beta::parse_action(&action);

    let pool = state.pool.load();
    let models = crate::models_route::resolve_visible_models(&pool, auth.identity.scoped());
    let entry = match models.iter().find(|m| m.id == model).cloned() {
        Some(e) => e,
        None => {
            // A model the pool no longer lists must not be advertised by the
            // single-model view either: disabling the last account for a model
            // drops it from /v1/models and /v1beta/models, so answering 200 here
            // would describe a model nothing can serve. Relay verbs still fall
            // back to the registry, which is how discovered provider models stay
            // routable before they appear in the pool listing.
            if verb.is_some() && pool.registry().resolve(&model).is_ok() {
                let owner = owner_of(&state, &model).unwrap_or_else(|| "devin".to_string());
                crate::models_route::ModelEntry {
                    id: model.clone(),
                    owned_by: owner,
                }
            } else {
                return json_status(StatusCode::NOT_FOUND, v1beta::model_not_found(&model));
            }
        }
    };

    let Some(verb) = verb else {
        return Json(v1beta::single_model_payload(&entry)).into_response();
    };

    let parsed = match parse_body(&body) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };

    match verb {
        GeminiAction::Generate | GeminiAction::StreamGenerate => {
            if !parsed.get("contents").map(Value::is_array).unwrap_or(false) {
                return json_status(StatusCode::BAD_REQUEST, v1beta::contents_not_specified());
            }
            let mut chat = parsed.clone();
            if let Some(obj) = chat.as_object_mut() {
                obj.insert("model".into(), json!(model));
                obj.insert(
                    "stream".into(),
                    json!(matches!(verb, GeminiAction::StreamGenerate)),
                );
            }
            handle_relay(
                state,
                &auth,
                RelayMode::GeminiNative,
                "/v1beta/models",
                &headers,
                Bytes::from(chat.to_string()),
            )
            .await
        }
        GeminiAction::CountTokens => {
            if !parsed.get("contents").map(Value::is_array).unwrap_or(false) {
                return json_status(StatusCode::BAD_REQUEST, v1beta::contents_not_specified());
            }
            if capability::resolve_for_capability(
                &state.pool.load(),
                &model,
                ModelCapability::CountTokens,
            )
            .is_none()
            {
                return json_status(StatusCode::NOT_FOUND, v1beta::model_not_found(&model));
            }
            let mut req = parsed.clone();
            if let Some(obj) = req.as_object_mut() {
                obj.insert("model".into(), json!(model));
            }
            handle_relay(
                state,
                &auth,
                RelayMode::GeminiCountTokens,
                "/v1beta/models",
                &headers,
                Bytes::from(req.to_string()),
            )
            .await
        }
        GeminiAction::Unknown(verb) => json_status(
            StatusCode::BAD_REQUEST,
            json!({"error": {
                "code": 400,
                "message": format!("Unknown method: {verb}"),
                "status": "INVALID_ARGUMENT",
            }}),
        ),
    }
}
pub use crate::interactions::handle as v1beta_interactions;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multipart_model_is_extracted_from_the_part_body() {
        let text =
            "--B\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ngpt-image-2\r\n--B--\r\n";
        assert_eq!(
            multipart_field(text, "model"),
            Some("gpt-image-2".to_string())
        );
    }

    #[test]
    fn missing_multipart_field_is_none() {
        let text = "--B\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nhi\r\n--B--\r\n";
        assert_eq!(multipart_field(text, "model"), None);
    }

    #[test]
    fn empty_body_parses_as_an_empty_object() {
        assert_eq!(parse_body(&Bytes::new()).unwrap(), json!({}));
    }
}
