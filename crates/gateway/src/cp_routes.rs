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

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use bytes::Bytes;
use mahoquot_registry::ModelCapability;
use serde_json::{json, Value};

use crate::capability::{self, model_of};
use crate::realtime;
use crate::relay::{handle_relay, RelayMode};
use crate::state::AppState;
use axum::Extension;
use crate::inbound::ResolvedAuth;
use crate::static_pages::{CALLBACK_HTML, MANAGEMENT_HTML, ROOT_JSON};
use crate::v1beta::{self, GeminiAction};

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

pub async fn images_edits(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let is_multipart = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("multipart/form-data"))
        .unwrap_or(false);
    if is_multipart {
        let text = String::from_utf8_lossy(&body);
        let model = multipart_field(&text, "model").unwrap_or_default();
        let snapshot = state.pool.load();
        return match capability::check_image(&snapshot, &model) {
            Some(err) => json_status(StatusCode::BAD_REQUEST, err),
            None => json_status(
                StatusCode::SERVICE_UNAVAILABLE,
                capability::unknown_provider(&model),
            ),
        };
    }
    image_surface(state, &auth, &headers, body).await
}

fn multipart_field(text: &str, name: &str) -> Option<String> {
    let marker = format!("name=\"{name}\"");
    let start = text.find(&marker)? + marker.len();
    let rest = &text[start..];
    let value_start = rest.find("\r\n\r\n")? + 4;
    let value = &rest[value_start..];
    let end = value.find("\r\n")?;
    Some(value[..end].to_string())
}

async fn image_surface(state: Arc<AppState>, auth: &ResolvedAuth, headers: &HeaderMap, body: Bytes) -> Response {
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

pub async fn videos(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    let parsed = match parse_body(&body) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let model = model_of(&parsed);
    let snapshot = state.pool.load();
    match capability::check_video(&snapshot, model) {
        Some(err) => json_status(StatusCode::BAD_REQUEST, err),
        None => json_status(
            StatusCode::SERVICE_UNAVAILABLE,
            capability::unknown_provider(model),
        ),
    }
}

/// `/v1/videos/:id` and the `/openai/v1/videos` family resolve a fixed model
/// before looking at the path, so an unknown id reports the model, not the id.
pub async fn videos_by_id() -> Response {
    json_status(
        StatusCode::BAD_REQUEST,
        capability::unknown_provider(capability::OPENAI_VIDEO_MODEL),
    )
}

pub async fn openai_videos() -> Response {
    json_status(
        StatusCode::BAD_REQUEST,
        capability::unknown_provider(capability::OPENAI_VIDEO_MODEL),
    )
}

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

pub async fn realtime_offer(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    let parsed = match parse_body(&body) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    if !realtime_allowed(&state, &parsed) {
        return json_status(
            StatusCode::NOT_IMPLEMENTED,
            realtime::capability_not_supported("Realtime WebRTC offers"),
        );
    }
    match realtime::validate_offer(&parsed) {
        Some(err) => json_status(StatusCode::BAD_REQUEST, err),
        None => json_status(
            StatusCode::NOT_IMPLEMENTED,
            realtime::capability_not_supported("Realtime WebRTC offers"),
        ),
    }
}

pub async fn realtime_client_secrets(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    let parsed = parse_body(&body).unwrap_or_else(|_| json!({}));
    if !realtime_allowed(&state, &parsed) {
        return json_status(
            StatusCode::NOT_IMPLEMENTED,
            realtime::capability_not_supported("Realtime client secrets"),
        );
    }
    Json(realtime::client_secret(&parsed)).into_response()
}

pub async fn realtime_sessions(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    let parsed = parse_body(&body).unwrap_or_else(|_| json!({}));
    if !realtime_allowed(&state, &parsed) {
        return json_status(
            StatusCode::NOT_IMPLEMENTED,
            realtime::capability_not_supported("Realtime sessions"),
        );
    }
    Json(realtime::legacy_session(&parsed)).into_response()
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

pub async fn realtime_hangup() -> Response {
    json_status(StatusCode::NOT_FOUND, realtime::call_not_found())
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

pub async fn live_sideband(ws: MaybeUpgrade) -> Response {
    match ws {
        Ok(_) => json_status(
            StatusCode::UPGRADE_REQUIRED,
            realtime::upgrade_required_flat(),
        ),
        Err(_) => json_status(
            StatusCode::UPGRADE_REQUIRED,
            realtime::upgrade_required_flat(),
        ),
    }
}

/// The three GET upgrade endpoints answer 101 so a client's handshake succeeds,
/// then close, because no upstream duplex session is established.
pub async fn ws_upgrade(ws: MaybeUpgrade) -> Response {
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

    if owner_of(&state, &model).as_deref() == Some("devin") {
        return handle_relay(
            state,
            &auth,
            RelayMode::Responses,
            "/v1/responses",
            &headers,
            body,
        )
        .await;
    }

    if owner_of(&state, &model).as_deref() != Some("google") {
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

    if let Some(refusal) = responses_stream_unsupported(&parsed) {
        return refusal;
    }

    let chat = responses_input_to_chat(&parsed, &model);
    let relayed = handle_relay(
        state,
        &auth,
        RelayMode::OpenAiCompat,
        "/v1/chat/completions",
        &headers,
        Bytes::from(chat.to_string()),
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
    match serde_json::from_slice::<Value>(&raw) {
        Ok(chat_reply) => Json(chat_to_responses(&chat_reply, &model)).into_response(),
        Err(_) => (parts.status, raw).into_response(),
    }
}

const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// This path buffers the upstream reply and re-wraps it as a single Responses
/// object, so `stream: true` cannot be honoured. Refuse it rather than return
/// a buffered body to a client that is waiting for SSE frames.
fn responses_stream_unsupported(req: &Value) -> Option<Response> {
    if req.get("stream").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    Some(json_status(
        StatusCode::BAD_REQUEST,
        json!({"error": {
            "message": "Streaming is not supported on /v1/responses for this model. \
                        Retry without \"stream\": true, or use /v1/chat/completions.",
            "type": "invalid_request_error",
            "param": "stream",
        }}),
    ))
}

fn responses_input_to_chat(req: &Value, model: &str) -> Value {
    let mut messages = Vec::new();
    if let Some(instructions) = req.get("instructions").and_then(Value::as_str) {
        messages.push(json!({"role": "system", "content": instructions}));
    }
    match req.get("input") {
        Some(Value::String(text)) => {
            messages.push(json!({"role": "user", "content": text}));
        }
        Some(Value::Array(items)) => {
            for item in items {
                // Round-trip items carry no role and no text content. Mapping
                // them like ordinary turns collapses both the prior call and
                // its result into empty user messages, so the model never sees
                // the tool exchange it is being asked to continue.
                match item.get("type").and_then(Value::as_str) {
                    Some("function_call") => {
                        messages.push(json!({
                            "role": "assistant",
                            "content": Value::Null,
                            "tool_calls": [{
                                "id": item.get("call_id").and_then(Value::as_str).unwrap_or_default(),
                                "type": "function",
                                "function": {
                                    "name": item.get("name").and_then(Value::as_str).unwrap_or_default(),
                                    "arguments": item
                                        .get("arguments")
                                        .and_then(Value::as_str)
                                        .unwrap_or("{}"),
                                },
                            }],
                        }));
                        continue;
                    }
                    Some("function_call_output") => {
                        let output = match item.get("output") {
                            Some(Value::String(s)) => s.clone(),
                            Some(other) => other.to_string(),
                            None => String::new(),
                        };
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": item
                                .get("call_id")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                            "content": output,
                        }));
                        continue;
                    }
                    _ => {}
                }
                let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                let text = match item.get("content") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Array(parts)) => parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join(""),
                    _ => String::new(),
                };
                messages.push(json!({"role": role, "content": text}));
            }
        }
        _ => {}
    }
    let mut chat = json!({"model": model, "messages": messages, "stream": false});
    // Tool declarations must survive the hop or the model can never emit a
    // call. Responses names the function fields inline; chat nests them under
    // "function", so re-wrap any entry that is not already in chat shape.
    if let Some(Value::Array(tools)) = req.get("tools") {
        let mapped: Vec<Value> = tools
            .iter()
            .map(|tool| {
                if tool.get("function").is_some() {
                    return tool.clone();
                }
                // Only an inline-named Responses function can be re-wrapped.
                // Built-in tool types (web_search, file_search, ...) carry no
                // name, and synthesizing an empty function object makes the
                // upstream reject the whole request for a missing name.
                let Some(name) = tool.get("name").and_then(Value::as_str) else {
                    return Value::Null;
                };
                let mut function = json!({ "name": name });
                for key in ["description", "parameters", "strict"] {
                    if let Some(v) = tool.get(key) {
                        function[key] = v.clone();
                    }
                }
                json!({"type": "function", "function": function})
            })
            .filter(|tool| !tool.is_null())
            .collect();
        if !mapped.is_empty() {
            chat["tools"] = Value::Array(mapped);
        }
    }
    // Responses names a forced tool inline; chat nests it under "function",
    // so forwarding the object form verbatim loses the forced selection.
    if let Some(choice) = req.get("tool_choice") {
        let mapped = match choice.get("name").and_then(Value::as_str) {
            Some(name) if choice.get("function").is_none() => {
                json!({"type": "function", "function": {"name": name}})
            }
            _ => choice.clone(),
        };
        chat["tool_choice"] = mapped;
    }
    for key in ["temperature", "top_p", "max_output_tokens"] {
        if let Some(v) = req.get(key) {
            let mapped = if key == "max_output_tokens" {
                "max_tokens"
            } else {
                key
            };
            chat[mapped] = v.clone();
        }
    }
    chat
}

fn chat_to_responses(chat: &Value, model: &str) -> Value {
    let choice = chat.get("choices").and_then(|c| c.get(0));
    let text = choice
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let id = chat
        .get("id")
        .and_then(Value::as_str)
        .map(|i| format!("resp_{i}"))
        .unwrap_or_else(|| "resp_0".to_string());
    // A tool turn carries null content, so emitting only the message item
    // would hand the client an empty assistant reply and lose the call.
    let mut output = Vec::new();
    if !text.is_empty() {
        output.push(json!({
            "id": "msg_0",
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }));
    }
    let tool_calls = choice
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array);
    for (index, call) in tool_calls.into_iter().flatten().enumerate() {
        let function = call.get("function");
        output.push(json!({
            "id": format!("fc_{index}"),
            "type": "function_call",
            "status": "completed",
            "call_id": call.get("id").and_then(Value::as_str).unwrap_or_default(),
            "name": function
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default(),
            "arguments": function
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("{}"),
        }));
    }
    if output.is_empty() {
        output.push(json!({
            "id": "msg_0",
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }));
    }
    let mut out = json!({
        "id": id,
        "object": "response",
        "created_at": chat.get("created").and_then(Value::as_i64).unwrap_or(0),
        "status": "completed",
        "background": false,
        "error": Value::Null,
        "incomplete_details": Value::Null,
        "model": model,
        "output": output,
        "parallel_tool_calls": true,
        "tool_choice": "auto",
        "tools": [],
    });
    if let Some(usage) = chat.get("usage") {
        let input = usage
            .get("prompt_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let output = usage
            .get("completion_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        out["usage"] = json!({
            "input_tokens": input,
            "output_tokens": output,
            "total_tokens": input + output,
        });
    }
    out
}

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
    let model = model_of(&parsed);
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
        Some(_) => json_status(
            StatusCode::NOT_IMPLEMENTED,
            json!({"error": {
                "message": "/responses/compact not supported",
                "type": "server_error",
                "code": "internal_server_error",
            }}),
        ),
    }
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

pub async fn v1beta_models(State(state): State<Arc<AppState>>, Extension(auth): Extension<ResolvedAuth>) -> Response {
    let pool = state.pool.load();
    let models = match auth.identity.scoped() {
        Some(key) => crate::models_route::scoped_model_entries(&pool, key),
        None => pool.models.clone(),
    };
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
    let models = match auth.identity.scoped() {
        Some(key) => crate::models_route::scoped_model_entries(&pool, key),
        None => pool.models.clone(),
    };
    let entry = match models.iter().find(|m| m.id == model).cloned() {
        Some(e) => e,
        None => {
            if pool.registry().resolve(&model).is_ok() {
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

pub async fn v1beta_interactions(body: Bytes) -> Response {
    let parsed = match parse_body(&body) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let has_model = parsed.get("model").is_some();
    let has_agent = parsed.get("agent").is_some();
    if has_model == has_agent {
        return json_status(
            StatusCode::BAD_REQUEST,
            json!({"error": {
                "message": "request requires exactly one of model or agent",
                "type": "invalid_request_error",
            }}),
        );
    }
    // The interactions schema is not GenerateContentRequest, so a body carrying
    // top-level `contents` is rejected upstream the same way it is here.
    json_status(StatusCode::BAD_REQUEST, v1beta::contents_not_specified())
}

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

    #[test]
    fn instructions_become_a_system_message_ahead_of_the_input() {
        let chat = responses_input_to_chat(
            &json!({"instructions": "be terse", "input": "hi"}),
            "gemini-3.7-flash-high",
        );
        assert_eq!(chat["messages"][0]["role"], "system");
        assert_eq!(chat["messages"][0]["content"], "be terse");
        assert_eq!(chat["messages"][1]["role"], "user");
        assert_eq!(chat["messages"][1]["content"], "hi");
        assert_eq!(chat["model"], "gemini-3.7-flash-high");
    }

    #[test]
    fn structured_input_parts_are_concatenated_per_message() {
        let chat = responses_input_to_chat(
            &json!({"input": [{
                "role": "user",
                "content": [{"type": "input_text", "text": "al"},
                            {"type": "input_text", "text": "pha"}],
            }]}),
            "m",
        );
        assert_eq!(chat["messages"][0]["content"], "alpha");
    }

    #[test]
    fn max_output_tokens_is_renamed_for_the_chat_surface() {
        let chat = responses_input_to_chat(
            &json!({"input": "hi", "max_output_tokens": 32, "temperature": 0.5}),
            "m",
        );
        assert_eq!(chat["max_tokens"], 32);
        assert_eq!(chat["temperature"], 0.5);
        assert!(chat.get("max_output_tokens").is_none());
    }

    #[test]
    fn chat_reply_is_rewrapped_as_a_responses_object() {
        let out = chat_to_responses(
            &json!({
                "id": "chatcmpl-1",
                "created": 1700,
                "choices": [{"message": {"content": "alpha"}}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 4},
            }),
            "gemini-3.7-flash-high",
        );
        assert_eq!(out["object"], "response");
        assert_eq!(out["id"], "resp_chatcmpl-1");
        assert_eq!(out["status"], "completed");
        assert_eq!(out["created_at"], 1700);
        assert_eq!(out["output"][0]["type"], "message");
        assert_eq!(out["output"][0]["content"][0]["type"], "output_text");
        assert_eq!(out["output"][0]["content"][0]["text"], "alpha");
        assert_eq!(out["usage"]["input_tokens"], 3);
        assert_eq!(out["usage"]["output_tokens"], 4);
        assert_eq!(out["usage"]["total_tokens"], 7);
    }
    #[test]
    fn the_responses_shim_carries_tools_in_and_tool_calls_back_out() {
        // A Responses request declaring tools must still be able to call them
        // on the google path: dropping the declaration means the model can
        // never emit a call, and dropping the reply's tool_calls means an
        // agent sees an empty assistant turn instead of its function call.
        let req = json!({
            "model": "gemini-2.5-pro",
            "input": "weather in Seoul?",
            "tools": [{
                "type": "function",
                "name": "get_weather",
                "parameters": {"type": "object", "properties": {}},
            }],
            "tool_choice": "auto",
        });
        let chat = responses_input_to_chat(&req, "gemini-2.5-pro");
        assert!(
            chat.get("tools").is_some(),
            "tool declarations were dropped on the way upstream: {chat}"
        );

        let chat_reply = json!({
            "id": "c1",
            "created": 1,
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": Value::Null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "get_weather", "arguments": "{\"city\":\"Seoul\"}"},
                    }],
                },
                "finish_reason": "tool_calls",
            }],
        });
        let out = chat_to_responses(&chat_reply, "gemini-2.5-pro");
        let output = out["output"].as_array().expect("output array");
        let call = output
            .iter()
            .find(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
            .unwrap_or_else(|| panic!("no function_call in the Responses output: {out}"));
        assert_eq!(call["name"], "get_weather");
        assert_eq!(call["call_id"], "call_1");
    }


    #[test]
    fn a_reply_with_both_text_and_tool_calls_keeps_both_output_items() {
        // Self-audit of the output-array rewrite: a model may answer with prose
        // AND a call in the same turn. Neither may be dropped, and the message
        // item must still come first.
        let chat_reply = json!({
            "id": "c9", "created": 7,
            "choices": [{"message": {
                "role": "assistant",
                "content": "let me check",
                "tool_calls": [{
                    "id": "call_9", "type": "function",
                    "function": {"name": "lookup", "arguments": "{}"}
                }]
            }}]
        });
        let out = chat_to_responses(&chat_reply, "gemini-3-flash");
        let items = out["output"].as_array().expect("output");
        assert_eq!(items.len(), 2, "both items must survive: {out}");
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["content"][0]["text"], "let me check");
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[1]["call_id"], "call_9");
    }

    #[test]
    fn an_empty_reply_still_emits_one_message_item() {
        // Guard the fallback branch: no text and no tool calls must not yield
        // an empty output array, which clients treat as a malformed response.
        let out = chat_to_responses(&json!({"id":"c0","created":1,"choices":[]}), "m");
        let items = out["output"].as_array().expect("output");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "message");
    }

    #[test]
    fn a_tool_entry_already_in_chat_shape_is_not_double_wrapped() {
        // responses_input_to_chat re-wraps Responses-style tools; an entry that
        // already has a "function" key must pass through untouched.
        let req = json!({"tools": [{"type":"function","function":{"name":"already"}}]});
        let chat = responses_input_to_chat(&req, "m");
        let tools = chat["tools"].as_array().expect("tools");
        assert_eq!(tools[0]["function"]["name"], "already");
        assert!(tools[0]["function"]["function"].is_null(), "double wrapped: {chat}");
    }

    #[test]
    fn function_call_round_trip_items_survive_the_responses_hop() {
        // A client replying to an emitted function_call sends back the call and
        // its output; collapsing both to empty user turns loses the tool result.
        let req = json!({"input": [
            {"type":"function_call","call_id":"call_1","name":"get_weather","arguments":"{\"city\":\"Seoul\"}"},
            {"type":"function_call_output","call_id":"call_1","output":"22C"}
        ]});
        let chat = responses_input_to_chat(&req, "m");
        let messages = chat["messages"].as_array().expect("messages");
        let assistant = messages
            .iter()
            .find(|m| m["role"] == "assistant")
            .unwrap_or_else(|| panic!("prior tool call dropped: {chat}"));
        assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "get_weather");
        let tool = messages
            .iter()
            .find(|m| m["role"] == "tool")
            .unwrap_or_else(|| panic!("tool result dropped: {chat}"));
        assert_eq!(tool["tool_call_id"], "call_1");
        assert_eq!(tool["content"], "22C");
        assert!(
            !messages.iter().any(|m| m["role"] == "user" && m["content"] == ""),
            "round-trip item collapsed into an empty user turn: {chat}"
        );
    }

    #[test]
    fn builtin_tool_types_are_not_synthesized_into_nameless_functions() {
        // web_search has neither a "function" key nor an inline name; wrapping it
        // yields {"type":"function","function":{}}, which upstream rejects as a
        // missing function.name and fails the whole request.
        let req = json!({"tools": [{"type":"web_search"}, {"type":"function","name":"ok"}]});
        let chat = responses_input_to_chat(&req, "m");
        let tools = chat["tools"].as_array().expect("tools");
        assert!(
            !tools.iter().any(|t| t["type"] == "function"
                && t["function"]["name"].is_null()),
            "emitted a nameless function tool: {chat}"
        );
        assert!(
            tools.iter().any(|t| t["function"]["name"] == "ok"),
            "the real function tool must still survive: {chat}"
        );
    }

    #[test]
    fn object_form_tool_choice_is_translated_to_chat_shape() {
        // Responses names the function inline; chat nests it, so forwarding the
        // Responses shape verbatim loses forced single-tool selection.
        let req = json!({"tool_choice": {"type":"function","name":"pick_me"}});
        let chat = responses_input_to_chat(&req, "m");
        assert_eq!(chat["tool_choice"]["function"]["name"], "pick_me", "{chat}");
        let plain = responses_input_to_chat(&json!({"tool_choice":"auto"}), "m");
        assert_eq!(plain["tool_choice"], "auto");
    }

}
