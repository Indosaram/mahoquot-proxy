//! A native WebSocket session owns one account and one upstream socket.
//! No reconnect, replay, or account selection is possible after HTTP 101.
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::extract::WebSocketUpgrade;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use mahoquot_types::{PoolMember, SessionHint};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::{self, protocol::Role};
use tokio_tungstenite::WebSocketStream;

use crate::account::{AccountMember, ProviderKind};
use crate::inbound::ResolvedAuth;
use crate::state::{AppState, PoolSnapshot};

pub fn authenticate_upgrade(
    keys: &crate::inbound::ApiKeys,
    ws_auth: bool,
    headers: &HeaderMap,
) -> Option<ResolvedAuth> {
    let presented = crate::inbound::presented_api_key(headers).or_else(|| {
        headers
            .get("sec-websocket-protocol")
            .and_then(|value| value.to_str().ok())?
            .split(',')
            .map(str::trim)
            .find_map(|protocol| protocol.strip_prefix("openai-insecure-api-key."))
    });
    let identity = match presented {
        Some(key) => keys.authorizes(Some(key))?,
        None if !ws_auth => crate::inbound::AuthIdentity::Unrestricted,
        None => keys.authorizes(None)?,
    };
    Some(ResolvedAuth {
        identity,
        key_identifier: presented.map(crate::request_history::stable_key_identifier),
    })
}

// Liveness bounds apply to idle reads and blocked writes, not generation time.
const LIVENESS: Duration = Duration::from_secs(300);
const MAX_FRAME: usize = 16 * 1024 * 1024;

fn error(status: StatusCode, code: &str) -> Response {
    (
        status,
        axum::Json(json!({"error":{"type":"invalid_request_error","code":code}})),
    )
        .into_response()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Surface {
    Responses,
    Realtime,
}

fn native(member: &AccountMember, surface: Surface) -> bool {
    match member.kind() {
        ProviderKind::Codex => true,
        ProviderKind::Generic => {
            surface == Surface::Responses
                && matches!(
                    member.generic_adapter().as_deref(),
                    Some("xai" | "openai-responses")
                )
        }
        _ => false,
    }
}

fn upstream_model(pool: &PoolSnapshot, member: &AccountMember, model: &str) -> Option<String> {
    let resolved = crate::relay::resolve_model(pool, model).ok()?;
    resolved
        .eligible_bindings
        .iter()
        .find(|binding| binding.provider_id.as_str() == member.provider_name())
        .map(|binding| {
            binding
                .effective_upstream_id(&resolved.canonical_id)
                .to_string()
        })
}

type UpstreamEndpoint = (String, Vec<(String, String)>);

fn endpoint(
    member: &AccountMember,
    surface: Surface,
    model: Option<&str>,
) -> Result<UpstreamEndpoint, &'static str> {
    let (mut url, headers) = if let Some(account) = member.xai_account() {
        let transport = mahoquot_providers::xai::Transport::Websocket;
        let base = account.base_url_for(transport, member.upstream_override.as_deref());
        (
            crate::url::join_provider_path(base, "/v1/responses"),
            account.headers(
                transport,
                base,
                false,
                mahoquot_providers::xai::CLIENT_VERSION,
            ),
        )
    } else {
        let url = match surface {
            Surface::Responses if member.kind() == ProviderKind::Codex => {
                crate::url::build_target_url(
                    member.upstream_override.as_deref(),
                    "/backend-api/codex/responses",
                )
            }
            Surface::Responses => {
                crate::url::join_provider_path(&member.effective_base_url(), "/v1/responses")
            }
            Surface::Realtime => crate::url::join_provider_path(
                member
                    .upstream_override
                    .as_deref()
                    .unwrap_or("https://api.openai.com/v1"),
                "/v1/realtime",
            ),
        };
        (url, member.build_upstream_headers())
    };
    // Dial through reqwest so credential/global/provider proxy and QA DNS policy
    // remain exactly the same as HTTP. The upgraded stream already owns TLS.
    if let Some(rest) = url.strip_prefix("wss://") {
        url = format!("https://{rest}");
    } else if let Some(rest) = url.strip_prefix("ws://") {
        url = format!("http://{rest}");
    }
    let mut parsed = reqwest::Url::parse(&url).map_err(|_| "invalid_websocket_endpoint")?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err("invalid_websocket_endpoint");
    }
    if crate::loopback_guard::is_qa_loopback_only()
        && (!crate::loopback_guard::is_literal_loopback_url(&parsed)
            || member.upstream_override.is_none()
            || !parsed
                .port()
                .is_some_and(|port| (18840..=18899).contains(&port)))
    {
        return Err("websocket_endpoint_not_loopback");
    }
    if surface == Surface::Realtime {
        if let Some(model) = model {
            parsed.query_pairs_mut().append_pair("model", model);
        }
    }
    Ok((parsed.to_string(), headers))
}

pub(crate) fn safe_header(name: &str) -> bool {
    !matches!(
        name.to_ascii_lowercase().as_str(),
        "host"
            | "connection"
            | "upgrade"
            | "sec-websocket-key"
            | "sec-websocket-version"
            | "sec-websocket-protocol"
            | "sec-websocket-extensions"
            | "x-session-id"
            | "x-client-request-id"
            | "session-id"
            | "x-session-affinity"
            | "x-parent-session-id"
            | "x-grok-conv-id"
    )
}

pub mod aistudio {
    pub use crate::aistudio::*;
}

/// Upgrade an inbound AI Studio worker connection at /v1/ws.
pub async fn upgrade_worker(
    state: axum::extract::State<std::sync::Arc<crate::state::AppState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    crate::aistudio::worker_upgrade(state, headers, ws).await
}

pub async fn upgrade(
    state: Arc<AppState>,
    auth: ResolvedAuth,
    uri: Uri,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let permit = match state.inference_gate().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "websocket_admission_full"),
    };
    let surface = if uri.path() == "/v1/realtime" {
        Surface::Realtime
    } else {
        Surface::Responses
    };
    let query: Vec<(String, String)> = reqwest::Url::parse(&format!("http://localhost{uri}"))
        .map(|url| url.query_pairs().into_owned().collect())
        .unwrap_or_default();
    if query.iter().any(|(key, _)| key == "call_id") {
        // Existing-call ownership and ephemeral authentication belong to Task21.
        return error(StatusCode::NOT_FOUND, "realtime_call_not_found");
    }
    let model = query
        .iter()
        .find(|(key, _)| key == "model")
        .map(|(_, value)| value.as_str())
        .filter(|value| !value.is_empty())
        .or_else(|| (surface == Surface::Realtime).then_some("gpt-realtime"));
    if let Some(scope) = auth.identity.scoped() {
        if model.is_none()
            || !crate::models_route::allow_list_admits(
                &scope.allowed_models,
                model.unwrap_or_default(),
            )
        {
            return error(StatusCode::FORBIDDEN, "websocket_model_scope_required");
        }
        if state
            .scoped_keys
            .get(&scope.key_identifier)
            .is_some_and(|entry| entry.is_exhausted())
        {
            return error(StatusCode::TOO_MANY_REQUESTS, "token_quota_exceeded");
        }
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0);
    let pool = state.pool.load_full();
    let ids = model
        .filter(|model| surface == Surface::Responses || *model != "gpt-realtime")
        .map(|model| crate::relay::eligible_account_ids_for_model(&state, model, now_ms));
    let settings = state.settings.current();
    let binding = settings
        .api_key_bindings
        .iter()
        .find(|binding| Some(&binding.key_identifier) == auth.key_identifier.as_ref());
    let candidates: Vec<Arc<AccountMember>> = match ids {
        Some(ids) => ids.iter().filter_map(|id| pool.find_member(id)).collect(),
        None => pool.members.clone(),
    }
    .into_iter()
    .filter(|member| {
        native(member, surface)
            && member.health().is_available(now_ms)
            && !member.is_manually_disabled()
            && state.scheduler.permits(member.id())
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
    let selectable: Vec<Arc<dyn PoolMember>> = candidates
        .iter()
        .map(|member| member.clone() as Arc<dyn PoolMember>)
        .collect();
    let Some(member) = state
        .router
        .select(&selectable, &SessionHint { affinity_key: None })
        .and_then(|index| candidates.get(index))
        .cloned()
    else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "websocket_auth_not_found");
    };
    let mapped_model = model
        .and_then(|model| upstream_model(&pool, &member, model))
        .or_else(|| model.map(str::to_owned));
    let (url, upstream_headers) = match endpoint(&member, surface, mapped_model.as_deref()) {
        Ok(target) => target,
        Err(code) => return error(StatusCode::BAD_GATEWAY, code),
    };
    let client = match state.try_client_for_member(&member) {
        Ok(client) => client,
        Err(_) => return error(StatusCode::BAD_GATEWAY, "websocket_proxy_invalid"),
    };
    let key = tungstenite::handshake::client::generate_key();
    let mut request = client
        .get(&url)
        .version(reqwest::Version::HTTP_11)
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", &key);
    for (name, value) in upstream_headers {
        if safe_header(&name) {
            request = request.header(name, value);
        }
    }
    // Never copy the client's header map. Only a native transport capability
    // header is accepted, and browser auth subprotocols are never sent upstream.
    if member.kind() == ProviderKind::Codex && surface == Surface::Responses {
        request = request.header("openai-beta", "responses_websockets=2026-02-06");
    } else if let Some(beta) = headers.get("openai-beta") {
        request = request.header("openai-beta", beta);
    }
    let response = match tokio::time::timeout(Duration::from_secs(30), request.send()).await {
        Ok(Ok(response)) => response,
        _ => return error(StatusCode::BAD_GATEWAY, "websocket_upstream_dial_failed"),
    };
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        let status = if response.status().is_client_error() || response.status().is_server_error() {
            response.status()
        } else {
            StatusCode::BAD_GATEWAY
        };
        return error(status, "websocket_upstream_upgrade_rejected");
    }
    let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
    if response
        .headers()
        .get("sec-websocket-accept")
        .and_then(|value| value.to_str().ok())
        != Some(accept.as_str())
        || !response
            .headers()
            .get(header::UPGRADE)
            .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
        || !response
            .headers()
            .get(header::CONNECTION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
            })
    {
        return error(
            StatusCode::BAD_GATEWAY,
            "websocket_upstream_handshake_invalid",
        );
    }
    let upgraded = match response.upgrade().await {
        Ok(upgraded) => upgraded,
        Err(_) => return error(StatusCode::BAD_GATEWAY, "websocket_upstream_upgrade_failed"),
    };
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME))
        .max_frame_size(Some(MAX_FRAME))
        .max_write_buffer_size(MAX_FRAME * 2);
    let upstream = WebSocketStream::from_raw_socket(upgraded, Role::Client, Some(config)).await;
    let selected_model = model.map(str::to_owned);
    ws.max_message_size(MAX_FRAME)
        .max_frame_size(MAX_FRAME)
        .max_write_buffer_size(MAX_FRAME * 2)
        .on_upgrade(move |downstream| async move {
            let _permit = permit;
            let _activity = member.begin_activity();
            let _in_flight = state.monitor.track_in_flight();
            relay(
                downstream,
                upstream,
                state,
                auth,
                pool,
                member,
                surface,
                selected_model,
                headers,
            )
            .await;
        })
}

fn to_upstream(message: Message) -> tungstenite::Message {
    match message {
        Message::Text(text) => tungstenite::Message::Text(text.as_str().into()),
        Message::Binary(bytes) => tungstenite::Message::Binary(bytes),
        Message::Ping(bytes) => tungstenite::Message::Ping(bytes),
        Message::Pong(bytes) => tungstenite::Message::Pong(bytes),
        Message::Close(frame) => {
            tungstenite::Message::Close(frame.map(|frame| tungstenite::protocol::CloseFrame {
                code: frame.code.into(),
                reason: frame.reason.as_str().into(),
            }))
        }
    }
}

fn to_downstream(message: tungstenite::Message) -> Option<Message> {
    match message {
        tungstenite::Message::Text(text) => Some(Message::Text(text.as_str().into())),
        tungstenite::Message::Binary(bytes) => Some(Message::Binary(bytes)),
        tungstenite::Message::Ping(bytes) => Some(Message::Ping(bytes)),
        tungstenite::Message::Pong(bytes) => Some(Message::Pong(bytes)),
        tungstenite::Message::Close(frame) => Some(Message::Close(frame.map(|frame| CloseFrame {
            code: frame.code.into(),
            reason: frame.reason.as_str().into(),
        }))),
        tungstenite::Message::Frame(_) => None,
    }
}

fn prepare_create(
    value: &mut Value,
    state: &AppState,
    auth: &ResolvedAuth,
    pool: &PoolSnapshot,
    member: &AccountMember,
    selected_model: Option<&str>,
    headers: &HeaderMap,
) -> Result<(), &'static str> {
    if value.get("type").and_then(Value::as_str) != Some("response.create") {
        return Ok(());
    }
    let requested = value
        .get("model")
        .and_then(Value::as_str)
        .or(selected_model)
        .ok_or("model_required")?
        .to_owned();
    if let Some(scope) = auth.identity.scoped() {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
            .unwrap_or(0);
        if !scope.is_usable_at(now_ms)
            || !crate::models_route::allow_list_admits(&scope.allowed_models, &requested)
            || state
                .scoped_keys
                .get(&scope.key_identifier)
                .is_some_and(|entry| entry.is_exhausted())
        {
            return Err("websocket_scope_denied");
        }
    }
    if !pool
        .routable_accounts_for_model(&requested)
        .iter()
        .any(|account| account.id() == member.id())
    {
        return Err("websocket_pinned_account_model_mismatch");
    }
    let mapped = upstream_model(pool, member, &requested).ok_or("websocket_model_not_found")?;
    let original = value.clone();
    let object = value.as_object_mut().ok_or("invalid_response_create")?;
    object.insert("model".into(), json!(mapped));
    if member.provider_name() == "xai" {
        object.remove("stream");
        object.remove("stream_options");
        object.remove("background");
        object.insert("store".into(), json!(true));
        if object
            .get("previous_response_id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
        {
            object.remove("instructions");
        }
        crate::compat::xai::normalize_request(value);
    }
    // Native upstream owns previous_response_id, cancellation, reset and prewarm.
    // No synthetic completion or local history substitutes for a native turn.
    state
        .settings
        .current()
        .payload
        .apply(
            value,
            &crate::management::settings::PayloadContext {
                model: &mapped,
                requested_model: &requested,
                protocol: "responses",
                from_protocol: "responses",
                headers,
                original: &original,
            },
        )
        .map_err(|_| "websocket_payload_invalid")?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn relay(
    downstream: WebSocket,
    upstream: WebSocketStream<reqwest::Upgraded>,
    state: Arc<AppState>,
    auth: ResolvedAuth,
    pool: Arc<PoolSnapshot>,
    member: Arc<AccountMember>,
    surface: Surface,
    selected_model: Option<String>,
    headers: HeaderMap,
) {
    let (mut client_tx, mut client_rx) = downstream.split();
    let (mut upstream_tx, mut upstream_rx) = upstream.split();
    let mut turns = std::collections::HashMap::<String, (String, std::time::Instant)>::new();
    let session = uuid::Uuid::new_v4().to_string();
    // Both pumps run concurrently. A blocked client write never prevents reading
    // response.cancel. Each sink admits one frame; there is no unbounded queue.
    let close = {
        let client_to_upstream = async {
            loop {
                let message = match tokio::time::timeout(LIVENESS, client_rx.next()).await {
                    Ok(Some(Ok(message))) => message,
                    Ok(None) => return 1000,
                    _ => return 1011,
                };
                let mut message = message;
                if let Message::Text(text) = &message {
                    let mut value: Value = match serde_json::from_str(text.as_str()) {
                        Ok(value) => value,
                        Err(_) => return 1007,
                    };
                    match surface {
                        Surface::Responses => {
                            if prepare_create(
                                &mut value,
                                &state,
                                &auth,
                                &pool,
                                &member,
                                selected_model.as_deref(),
                                &headers,
                            )
                            .is_err()
                            {
                                return 1008;
                            }
                            message = Message::Text(value.to_string().into());
                        }
                        Surface::Realtime => {
                            if let Some(model) = value
                                .get("session")
                                .and_then(|session| session.get("model"))
                                .and_then(Value::as_str)
                            {
                                if selected_model.as_deref() != Some(model) {
                                    return 1008;
                                }
                            }
                        }
                    }
                }
                let closing = matches!(message, Message::Close(_));
                if !matches!(
                    tokio::time::timeout(LIVENESS, upstream_tx.send(to_upstream(message))).await,
                    Ok(Ok(()))
                ) {
                    return 1011;
                }
                if closing {
                    return 1000;
                }
            }
        };
        let upstream_to_client = async {
            loop {
                let message = match tokio::time::timeout(LIVENESS, upstream_rx.next()).await {
                    Ok(Some(Ok(message))) => message,
                    // EOF without a close frame is an upstream failure.
                    _ => return 1011,
                };
                if let tungstenite::Message::Text(text) = &message {
                    if let Ok(value) = serde_json::from_str::<Value>(text.as_str()) {
                        if let Some(id) = value["response"]["id"].as_str() {
                            match value["type"].as_str() {
                                Some("response.created") => {
                                    turns.entry(id.to_owned()).or_insert_with(|| {
                                        (
                                            value["response"]["model"]
                                                .as_str()
                                                .or(selected_model.as_deref())
                                                .unwrap_or("unknown")
                                                .to_owned(),
                                            std::time::Instant::now(),
                                        )
                                    });
                                }
                                Some(
                                    "response.completed"
                                    | "response.failed"
                                    | "response.cancelled"
                                    | "response.incomplete",
                                ) => {
                                    if let Some((model, start)) = turns.remove(id) {
                                        let usage = crate::usage::extract_response_token_usage(
                                            text.as_bytes(),
                                            text.as_bytes(),
                                        )
                                        .unwrap_or_default();
                                        record_turn(
                                            &state,
                                            &auth,
                                            &member,
                                            &session,
                                            model,
                                            start,
                                            value["type"] == "response.completed",
                                            usage,
                                        )
                                        .await;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                let closing = matches!(message, tungstenite::Message::Close(_));
                if let Some(message) = to_downstream(message) {
                    if !matches!(
                        tokio::time::timeout(LIVENESS, client_tx.send(message)).await,
                        Ok(Ok(()))
                    ) {
                        return 1011;
                    }
                }
                if closing {
                    return 1000;
                }
            }
        };
        tokio::select! {
            code = client_to_upstream => code,
            code = upstream_to_client => code,
            _ = state.shutdown.notified() => 1001,
        }
    };
    for (_, (model, start)) in turns {
        record_turn(
            &state,
            &auth,
            &member,
            &session,
            model,
            start,
            false,
            Default::default(),
        )
        .await;
    }
    // Dropping the losing pump cancels its pending I/O. Close both peers, with
    // bounded cleanup even when the recipient no longer drains its socket.
    let client_close = Message::Close(Some(CloseFrame {
        code: close,
        reason: "".into(),
    }));
    let upstream_close = tungstenite::Message::Close(Some(tungstenite::protocol::CloseFrame {
        code: close.into(),
        reason: "".into(),
    }));
    let results = tokio::join!(
        tokio::time::timeout(Duration::from_secs(5), client_tx.send(client_close)),
        tokio::time::timeout(Duration::from_secs(5), upstream_tx.send(upstream_close))
    );
    if !matches!(results, (Ok(Ok(())), Ok(Ok(())))) {
        tracing::debug!(account = %member.id(), "WebSocket close ended with a disconnected peer");
    }
}

#[allow(clippy::too_many_arguments)]
async fn record_turn(
    state: &AppState,
    auth: &ResolvedAuth,
    member: &AccountMember,
    session: &str,
    model: String,
    start: std::time::Instant,
    succeeded: bool,
    usage: crate::usage::ResponseTokenUsage,
) {
    let total_tokens = usage.total_tokens();
    let occurred_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0);
    let latency_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    if let Some(charged) = state
        .scoped_keys
        .record_usage(auth.key_identifier.as_deref(), total_tokens)
    {
        if let Some(scope) = auth.identity.scoped() {
            let settings = state.settings.clone();
            let id = scope.id.clone();
            let result = tokio::task::spawn_blocking(move || {
                settings.mutate(|settings| {
                    if let Some(key) = settings.scoped_api_keys.iter_mut().find(|key| key.id == id)
                    {
                        key.token_used = charged;
                    }
                })
            })
            .await;
            if !matches!(result, Ok(Ok(_))) {
                tracing::warn!("WebSocket scoped usage persistence failed");
            }
        }
    }
    state.history.enqueue(crate::request_history::UsageEvent {
        event_id: uuid::Uuid::new_v4().to_string(),
        occurred_at_ms,
        account_identifier: member.id().to_owned(),
        provider: member.provider_name(),
        model,
        key_identifier: auth.key_identifier.clone(),
        session_identifier: Some(session.to_owned()),
        status_code: if succeeded { 200 } else { 502 },
        succeeded,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        cached_input_tokens_known: usage.cached_input_tokens_known,
        cache_write_tokens: usage.cache_write_tokens,
        cache_write_tokens_known: usage.cache_write_tokens_known,
        reasoning_tokens: usage.reasoning_tokens,
        total_tokens,
        latency_ms,
        fast: false,
    });
    state.telemetry.record_with_account(
        occurred_at_ms / 1000,
        &member.provider_name(),
        Some(member.id()),
        succeeded,
    );
    state.notify_finalizer(Some(member.id()), session);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip_when_all_public_opcodes_are_used() {
        let frames = [
            Message::Text("response.cancel".into()),
            Message::Binary(vec![0, 255].into()),
            Message::Ping(vec![1].into()),
            Message::Pong(vec![2].into()),
            Message::Close(Some(CloseFrame {
                code: 1008,
                reason: "scope".into(),
            })),
            Message::Close(None),
        ];
        for frame in frames {
            let observed = to_downstream(to_upstream(frame.clone())).expect("public frame");
            assert_eq!(observed, frame);
        }
    }

    #[test]
    fn headers_when_account_metadata_contains_routing_fields_are_isolated() {
        for header in [
            "X-Session-Id",
            "x-client-request-id",
            "session-id",
            "x-parent-session-id",
            "x-session-affinity",
            "sec-websocket-protocol",
            "Sec-WebSocket-Extensions",
            "Host",
        ] {
            assert!(!safe_header(header), "{header}");
        }
        assert!(safe_header("authorization"));
        assert!(safe_header("chatgpt-account-id"));
    }

    #[test]
    fn endpoint_when_xai_oauth_uses_native_api_instead_of_chat_proxy() {
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
        let (url, headers) =
            endpoint(&member, Surface::Responses, None).expect("native xAI endpoint");
        assert_eq!(url, "https://api.x.ai/v1/responses");
        assert!(headers
            .iter()
            .any(|(key, value)| key == "authorization" && value == "Bearer fixture"));
        assert!(!headers.iter().any(|(key, _)| key == "x-xai-token-auth"));
    }

    #[tokio::test]
    async fn create_when_continuation_or_prewarm_preserves_native_state() {
        let fixture = crate::state::task2_proxy_consumers::Fixture::new("http://127.0.0.1:18841");
        let auth = ResolvedAuth {
            identity: crate::inbound::AuthIdentity::Master,
            key_identifier: None,
        };
        let member = fixture.member();
        let pool = fixture.state.pool.load_full();
        for generate in [true, false] {
            let mut value = json!({"type":"response.create","model":"gpt-5.6-sol","input":[],
                "previous_response_id":"resp_one","generate":generate});
            prepare_create(
                &mut value,
                &fixture.state,
                &auth,
                &pool,
                &member,
                None,
                &HeaderMap::new(),
            )
            .expect("native turn");
            assert_eq!(value["previous_response_id"], "resp_one");
            assert_eq!(value["generate"], generate);
            assert_eq!(value["input"], json!([]));
        }
    }

    #[tokio::test]
    async fn create_when_model_is_not_in_pinned_account_is_rejected() {
        let fixture = crate::state::task2_proxy_consumers::Fixture::new("http://127.0.0.1:18841");
        let auth = ResolvedAuth {
            identity: crate::inbound::AuthIdentity::Master,
            key_identifier: None,
        };
        let mut value =
            json!({"type":"response.create","model":"anthropic/absent-fixture-model","input":[]});
        assert!(prepare_create(
            &mut value,
            &fixture.state,
            &auth,
            &fixture.state.pool.load_full(),
            &fixture.member(),
            None,
            &HeaderMap::new()
        )
        .is_err());
    }

    #[tokio::test]
    async fn create_when_scoped_model_is_denied_does_not_mutate_frame() {
        let fixture = crate::state::task2_proxy_consumers::Fixture::new("http://127.0.0.1:18841");
        let key = crate::management::settings::ScopedApiKey {
            id: "scope".into(),
            name: "scope".into(),
            key_identifier: "scope".into(),
            key_prefix: "scope".into(),
            raw_key: None,
            allowed_providers: vec![],
            allowed_accounts: vec![],
            allowed_models: vec!["other".into()],
            token_limit: 0,
            token_used: 0,
            is_active: true,
            created_at_ms: 0,
            expires_at_ms: None,
        };
        let auth = ResolvedAuth {
            identity: crate::inbound::AuthIdentity::Scoped(Arc::new(key)),
            key_identifier: Some("scope".into()),
        };
        let original = json!({"type":"response.create","model":"gpt-5.6-sol","input":[]});
        let mut value = original.clone();
        assert_eq!(
            prepare_create(
                &mut value,
                &fixture.state,
                &auth,
                &fixture.state.pool.load_full(),
                &fixture.member(),
                None,
                &HeaderMap::new()
            ),
            Err("websocket_scope_denied")
        );
        assert_eq!(value, original);
    }

    #[tokio::test]
    async fn create_when_payload_rules_override_model_remains_final() {
        let fixture = crate::state::task2_proxy_consumers::Fixture::new("http://127.0.0.1:18841");
        let settings = crate::management::settings::Settings::from_yaml(
            r#"
payload:
  override:
    - models:
        - name: '*'
          protocol: responses
      params:
        store: false
        fixture: configured
"#,
        )
        .expect("payload config");
        fixture
            .state
            .settings
            .mutate(|current| current.payload = settings.payload)
            .expect("publish");
        let auth = ResolvedAuth {
            identity: crate::inbound::AuthIdentity::Master,
            key_identifier: None,
        };
        let mut value =
            json!({"type":"response.create","model":"gpt-5.6-sol","input":[],"store":true});
        prepare_create(
            &mut value,
            &fixture.state,
            &auth,
            &fixture.state.pool.load_full(),
            &fixture.member(),
            None,
            &HeaderMap::new(),
        )
        .expect("finalized frame");
        assert_eq!(value["store"], false);
        assert_eq!(value["fixture"], "configured");
    }

    #[test]
    fn auth_when_runtime_enabled_rejects_missing_and_invalid_keys() {
        let keys = crate::inbound::ApiKeys::new(vec!["fixture-key".into()]);
        assert!(authenticate_upgrade(&keys, true, &HeaderMap::new()).is_none());
        assert!(authenticate_upgrade(&keys, false, &HeaderMap::new()).is_some());
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer wrong".parse().expect("header"),
        );
        assert!(authenticate_upgrade(&keys, false, &headers).is_none());
        headers.remove(header::AUTHORIZATION);
        headers.insert(
            "sec-websocket-protocol",
            "realtime, openai-insecure-api-key.fixture-key"
                .parse()
                .expect("header"),
        );
        assert!(authenticate_upgrade(&keys, true, &headers).is_some());
    }
}

// AI Studio worker protocol: each authenticated worker owns one socket. Request IDs
// correlate concurrent upstream operations; no inference is attempted without one.
// This is intentionally distinct from the native client->provider WebSocket relay above.
pub mod aistudio_worker {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use tokio::sync::{mpsc, oneshot};

    #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    pub struct WorkerFrame {
        pub id: String,
        #[serde(rename = "type")]
        pub kind: String,
        #[serde(default)]
        pub payload: serde_json::Value,
    }

    #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    pub struct WorkerModel {
        pub id: String,
    }

    #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    pub struct WorkerCatalog {
        pub models: Vec<WorkerModel>,
    }

    struct Pending {
        response: oneshot::Sender<Result<WorkerFrame, RelayError>>,
    }

    #[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
    pub enum RelayError {
        #[error("AI Studio worker is unavailable")]
        Unavailable,
        #[error("AI Studio worker request was cancelled")]
        Cancelled,
        #[error("AI Studio worker response correlation failed")]
        Correlation,
        #[error("AI Studio worker returned an error")]
        Worker,
    }

    #[derive(Clone)]
    pub struct WorkerSession {
        pub worker_id: String,
        outbound: mpsc::UnboundedSender<WorkerFrame>,
        pending: Arc<Mutex<HashMap<String, Pending>>>,
        catalog: Arc<Mutex<Option<WorkerCatalog>>>,
    }

    impl WorkerSession {
        pub fn new(worker_id: String) -> (Self, mpsc::UnboundedReceiver<WorkerFrame>) {
            let (outbound, receiver) = mpsc::unbounded_channel();
            (
                Self {
                    worker_id,
                    outbound,
                    pending: Arc::new(Mutex::new(HashMap::new())),
                    catalog: Arc::new(Mutex::new(None)),
                },
                receiver,
            )
        }

        pub fn set_catalog(&self, catalog: WorkerCatalog) {
            *self
                .catalog
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = Some(catalog);
        }

        pub fn catalog(&self) -> Result<WorkerCatalog, RelayError> {
            self.catalog
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone()
                .ok_or(RelayError::Unavailable)
        }

        pub async fn dispatch(&self, request: WorkerFrame) -> Result<WorkerFrame, RelayError> {
            if request.id.trim().is_empty() {
                return Err(RelayError::Correlation);
            }
            let id = request.id.clone();
            let (sender, receiver) = oneshot::channel();
            {
                let mut pending = self
                    .pending
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if pending.contains_key(&id) {
                    return Err(RelayError::Correlation);
                }
                pending.insert(id.clone(), Pending { response: sender });
            }
            if self.outbound.send(request).is_err() {
                self.pending
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .remove(&id);
                return Err(RelayError::Unavailable);
            }
            let result = receiver.await.unwrap_or(Err(RelayError::Unavailable));
            result
        }

        pub fn receive(&self, response: WorkerFrame) -> Result<(), RelayError> {
            let pending = self
                .pending
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(&response.id);
            let Some(pending) = pending else {
                return Err(RelayError::Correlation);
            };
            let result = if response.kind == "error" {
                Err(RelayError::Worker)
            } else {
                Ok(response)
            };
            let _ = pending.response.send(result);
            Ok(())
        }

        pub fn cancel(&self, id: &str) -> Result<(), RelayError> {
            let pending = self
                .pending
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(id);
            let Some(pending) = pending else {
                return Err(RelayError::Correlation);
            };
            let _ = pending.response.send(Err(RelayError::Cancelled));
            Ok(())
        }

        pub fn disconnect(&self) {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            for (_, request) in pending.drain() {
                let _ = request.response.send(Err(RelayError::Unavailable));
            }
            *self
                .catalog
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = None;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;

        fn request(id: &str) -> WorkerFrame {
            WorkerFrame {
                id: id.into(),
                kind: "http_request".into(),
                payload: json!({"path":"/v1beta/models"}),
            }
        }

        #[tokio::test]
        async fn concurrent_requests_correlate_out_of_order_and_catalog_is_dynamic() {
            let (session, mut outbound) = WorkerSession::new("worker-1".into());
            session.set_catalog(WorkerCatalog {
                models: vec![WorkerModel {
                    id: "gemini-live-model".into(),
                }],
            });
            let catalog = session.catalog().unwrap();
            assert_eq!(catalog.models[0].id, "gemini-live-model");
            let first = {
                let session = session.clone();
                tokio::spawn(async move { session.dispatch(request("one")).await })
            };
            let second = {
                let session = session.clone();
                tokio::spawn(async move { session.dispatch(request("two")).await })
            };
            let a = outbound.recv().await.unwrap();
            let b = outbound.recv().await.unwrap();
            session
                .receive(WorkerFrame {
                    id: b.id.clone(),
                    kind: "http_response".into(),
                    payload: json!({"status":200}),
                })
                .unwrap();
            session
                .receive(WorkerFrame {
                    id: a.id.clone(),
                    kind: "http_response".into(),
                    payload: json!({"status":200}),
                })
                .unwrap();
            assert_eq!(first.await.unwrap().unwrap().id, "one");
            assert_eq!(second.await.unwrap().unwrap().id, "two");
        }

        #[tokio::test]
        async fn disconnect_fails_pending_and_clears_catalog() {
            let (session, mut outbound) = WorkerSession::new("worker-1".into());
            session.set_catalog(WorkerCatalog {
                models: vec![WorkerModel {
                    id: "live-only".into(),
                }],
            });
            let pending = {
                let session = session.clone();
                tokio::spawn(async move { session.dispatch(request("pending")).await })
            };
            let _ = outbound.recv().await.unwrap();
            session.disconnect();
            assert_eq!(pending.await.unwrap(), Err(RelayError::Unavailable));
            assert_eq!(session.catalog(), Err(RelayError::Unavailable));
        }

        #[tokio::test]
        async fn unmatched_frame_is_rejected_without_affecting_other_requests() {
            let (session, mut outbound) = WorkerSession::new("worker-1".into());
            let pending = {
                let session = session.clone();
                tokio::spawn(async move { session.dispatch(request("valid")).await })
            };
            let _ = outbound.recv().await.unwrap();
            assert_eq!(
                session.receive(WorkerFrame {
                    id: "stranger".into(),
                    kind: "http_response".into(),
                    payload: json!({})
                }),
                Err(RelayError::Correlation)
            );
            session
                .receive(WorkerFrame {
                    id: "valid".into(),
                    kind: "http_response".into(),
                    payload: json!({"status":200}),
                })
                .unwrap();
            assert_eq!(pending.await.unwrap().unwrap().id, "valid");
        }

        #[tokio::test]
        async fn cancellation_removes_only_matching_request() {
            let (session, mut outbound) = WorkerSession::new("worker-1".into());
            let pending = {
                let session = session.clone();
                tokio::spawn(async move { session.dispatch(request("cancel-me")).await })
            };
            let _ = outbound.recv().await.unwrap();
            session.cancel("cancel-me").unwrap();
            assert_eq!(pending.await.unwrap(), Err(RelayError::Cancelled));
        }
    }
}
