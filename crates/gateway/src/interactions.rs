//! Gemini Interactions: native Google pass-through and provider translation.
//! The router owns the continuation cache through an Extension, never a global.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Extension, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures::StreamExt;
use mahoquot_types::{PoolMember, SessionHint};
use serde_json::{json, Value};

use crate::account::ProviderKind;
use crate::compat::events::SseParser;
use crate::inbound::ResolvedAuth;
use crate::state::AppState;

const CAPACITY: usize = 1024;
const TTL: Duration = Duration::from_secs(30 * 60);
const MAX_JSON: usize = 32 * 1024 * 1024;

#[derive(Clone)]
struct Continuation {
    id: String,
    owner: String,
    namespace: String,
    environment: Option<Value>,
    /// Full provider history, including opaque signed model parts.
    contents: Value,
    calls: Vec<Value>,
    expires: Instant,
}

#[derive(Default)]
pub struct InteractionsStore {
    entries: BTreeMap<String, Continuation>,
}

pub type SharedInteractionsStore = Arc<Mutex<InteractionsStore>>;

fn digest(value: &Value) -> String {
    crate::request_history::stable_key_identifier(&value.to_string())
}

fn call_key(ids: impl IntoIterator<Item = String>) -> Result<String, String> {
    let mut unique = BTreeSet::new();
    for id in ids {
        if id.is_empty() || id.contains('\0') || !unique.insert(id) {
            return Err("duplicate, empty or ambiguous function call IDs".into());
        }
    }
    if unique.is_empty() {
        return Err("function results require pending call IDs".into());
    }
    Ok(digest(&json!(unique)))
}

impl InteractionsStore {
    fn put(&mut self, key: String, entry: Continuation, now: Instant) {
        self.entries.retain(|_, entry| now < entry.expires);
        if !self.entries.contains_key(&key) && self.entries.len() >= CAPACITY {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires)
                .map(|(key, _)| key.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(key, entry);
    }

    fn get(&mut self, key: &str, now: Instant) -> Option<Continuation> {
        self.entries.retain(|_, entry| now < entry.expires);
        self.entries.get(key).cloned()
    }
}

fn parse_request(raw: &[u8]) -> Result<(Value, String, bool, bool), String> {
    let mut body: Value = serde_json::from_slice(raw).map_err(|error| error.to_string())?;
    if !body.is_object() {
        return Err("request must be a JSON object".into());
    }
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let agent = body
        .get("agent")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let (target, is_agent) = match (model, agent) {
        (Some(model), None) if body.get("agent").is_none_or(|v| v.is_string()) => (
            model.strip_prefix("models/").unwrap_or(model).to_string(),
            false,
        ),
        (None, Some(agent)) if body.get("model").is_none_or(|v| v.is_string()) => {
            (agent.to_string(), true)
        }
        _ => return Err("request requires exactly one of model or agent".into()),
    };
    let stream = match body.get("stream") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err("stream must be a boolean".into()),
    };
    if !body
        .get("input")
        .is_some_and(|v| v.is_string() || v.is_array())
    {
        return Err("input must be a string or array".into());
    }
    body[if is_agent { "agent" } else { "model" }] = json!(target);
    Ok((body, target, is_agent, stream))
}

fn namespace(
    body: &Value,
    caller: &str,
    credential: &str,
    model: &str,
    headers: &HeaderMap,
) -> String {
    let session = headers
        .get("x-session-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty());
    let input = body.get("input").cloned().unwrap_or(Value::Null);
    let prefix = if let Some(steps) = input.as_array() {
        let last = steps
            .iter()
            .rposition(|s| s.get("type").and_then(Value::as_str) == Some("user_input"));
        last.map(|index| json!(&steps[..=index]))
            .unwrap_or(json!([]))
    } else {
        input
    };
    let conversation = session.map(|s| json!(s)).unwrap_or_else(
        || json!({"input":prefix,"system_instruction":body.get("system_instruction")}),
    );
    digest(&json!([caller, credential, model, conversation]))
}

fn prepare(
    body: &mut Value,
    native: bool,
    namespace: &str,
    store: &mut InteractionsStore,
    now: Instant,
) -> Result<Option<Continuation>, String> {
    if let Some(previous) = body.get("previous_interaction_id") {
        let id = previous
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or("previous_interaction_id must be a nonempty string")?;
        let saved = store
            .get(&format!("{namespace}:interaction:{id}"), now)
            .ok_or("previous interaction is expired or outside caller scope")?;
        if saved.calls.is_empty() {
            if body.get("environment_id").is_none() {
                if let Some(environment) = &saved.environment {
                    body["environment_id"] = environment.clone();
                }
            }
            return Ok(Some(saved));
        }
    }
    let Some(input) = body.get("input").and_then(Value::as_array) else {
        return Ok(None);
    };
    let results: Vec<_> = input
        .iter()
        .rev()
        .take_while(|step| step.get("type").and_then(Value::as_str) == Some("function_result"))
        .cloned()
        .collect();
    if results.is_empty() {
        return Ok(None);
    }
    let ids = results.iter().map(|step| {
        step.get("call_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    });
    let key = format!("{namespace}:{}", call_key(ids)?);
    let saved = store.get(&key, now).ok_or(
        "no matching continuation for caller, credential, model, conversation and pending IDs",
    )?;
    if let Some(explicit) = body.get("previous_interaction_id") {
        if explicit.as_str() != Some(saved.id.as_str()) {
            return Err("previous_interaction_id does not match pending calls".into());
        }
    }
    if native {
        if body.get("previous_interaction_id").is_none() {
            body["previous_interaction_id"] = json!(saved.id);
        }
        if body.get("environment_id").is_none() {
            if let Some(environment) = &saved.environment {
                body["environment_id"] = environment.clone();
            }
        }
        body["input"] = json!(results.into_iter().rev().collect::<Vec<_>>());
    } else {
        let mut results: Vec<_> = results.into_iter().rev().collect();
        for result in &mut results {
            let call = saved
                .calls
                .iter()
                .find(|call| call.get("id") == result.get("call_id"))
                .ok_or("unknown pending call ID")?;
            result["name"] = call
                .get("name")
                .cloned()
                .ok_or("pending call has no name")?;
        }
        body["input"] = json!(results);
    }
    Ok(Some(saved))
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({"error":{"type":"invalid_request_error","message":message.into()}})),
    )
        .into_response()
}

pub async fn handle(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
    Extension(store): Extension<SharedInteractionsStore>,
    headers: HeaderMap,
    raw: Bytes,
) -> Response {
    let (original, target, agent, stream) = match parse_request(&raw) {
        Ok(request) => request,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    if let Some(scoped) = auth.identity.scoped() {
        if !crate::models_route::allow_list_admits(&scoped.allowed_models, &target) {
            return error(StatusCode::FORBIDDEN, "model or agent is not permitted");
        }
        if state
            .scoped_keys
            .get(&scoped.key_identifier)
            .is_some_and(|entry| entry.is_exhausted())
        {
            return error(StatusCode::TOO_MANY_REQUESTS, "token quota exhausted");
        }
    }
    let settings = state.settings.current();
    let binding = settings
        .api_key_bindings
        .iter()
        .find(|binding| Some(&binding.key_identifier) == auth.key_identifier.as_ref());
    let pool = state.pool.load_full();
    let candidates = if agent {
        pool.members.clone()
    } else {
        pool.routable_accounts_for_model(&target)
    };
    let eligible_ids = if agent {
        None
    } else {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|time| i64::try_from(time.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        Some(crate::relay::eligible_account_ids_for_model(
            &state, &target, now,
        ))
    };
    let accounts: Vec<_> = candidates
        .into_iter()
        .filter(|member| {
            let native = member.kind() == ProviderKind::Generic
                && member.generic_adapter().as_deref() == Some("google");
            (native
                || (!agent
                    && matches!(
                        member.kind(),
                        ProviderKind::Antigravity | ProviderKind::Devin
                    )))
                && eligible_ids
                    .as_ref()
                    .is_none_or(|ids| ids.contains(&member.id))
                && !member.is_manually_disabled()
                && crate::models_route::member_matches_scope(
                    member,
                    auth.identity.scoped().map(|key| key.as_ref()),
                )
                && binding.is_none_or(|binding| {
                    binding.account.as_ref().is_none_or(|id| id == &member.id)
                        && binding
                            .provider
                            .as_ref()
                            .is_none_or(|provider| provider == &member.provider_name())
                })
        })
        .collect();
    let candidates: Vec<Arc<dyn PoolMember>> = accounts
        .iter()
        .map(|member| member.clone() as Arc<dyn PoolMember>)
        .collect();
    let Some(index) = state
        .router
        .select(&candidates, &SessionHint { affinity_key: None })
    else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no eligible Interactions account",
        );
    };
    let Some(member) = accounts.get(index).cloned() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no eligible Interactions account",
        );
    };
    let native = member.kind() == ProviderKind::Generic;
    let devin = member.kind() == ProviderKind::Devin;
    let model = if agent {
        target.clone()
    } else {
        match crate::relay::resolve_model(&pool, &target) {
            Ok(resolved) => resolved
                .eligible_bindings
                .iter()
                .find(|binding| binding.provider_id.as_str() == member.provider_name())
                .map(|binding| {
                    binding
                        .effective_upstream_id(&resolved.canonical_id)
                        .to_string()
                })
                .unwrap_or_else(|| resolved.canonical_id.as_str().to_string()),
            Err(message) => return error(StatusCode::BAD_REQUEST, message.to_string()),
        }
    };
    let credential = digest(&json!([
        member.id,
        member.provider_name(),
        member.access_token(),
        member.effective_base_url()
    ]));
    let caller = auth.key_identifier.as_deref().unwrap_or("unrestricted");
    let owner = digest(&json!([
        caller,
        credential,
        model,
        headers.get("x-session-id").and_then(|v| v.to_str().ok())
    ]));
    let namespace = if let Some(previous) = original
        .get("previous_interaction_id")
        .and_then(Value::as_str)
    {
        let mut cache = store.lock().unwrap_or_else(|p| p.into_inner());
        cache
            .entries
            .retain(|_, entry| Instant::now() < entry.expires);
        let matches: Vec<_> = cache
            .entries
            .values()
            .filter(|entry| entry.id == previous && entry.owner == owner)
            .map(|entry| entry.namespace.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if matches.len() != 1 {
            return error(
                StatusCode::BAD_REQUEST,
                "unknown or ambiguous previous interaction in caller scope",
            );
        }
        matches[0].clone()
    } else {
        namespace(&original, caller, &credential, &model, &headers)
    };
    let mut body = original.clone();
    body[if agent { "agent" } else { "model" }] = json!(model);
    let saved = match prepare(
        &mut body,
        native,
        &namespace,
        &mut store.lock().unwrap_or_else(|p| p.into_inner()),
        Instant::now(),
    ) {
        Ok(saved) => saved,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    if body.get("previous_interaction_id").is_some() && saved.is_none() {
        return error(
            StatusCode::BAD_REQUEST,
            "explicit continuation requires a matching locally scoped pending call",
        );
    }
    let (url, mut business, history) = if native {
        let base = member.effective_base_url();
        let base = base.trim_end_matches('/');
        let url = if base.ends_with("/interactions") {
            base.to_string()
        } else if base.ends_with("/v1beta") {
            format!("{base}/interactions")
        } else {
            format!("{base}/v1beta/interactions")
        };
        (url, body, Value::Null)
    } else if devin {
        let request = match interactions_to_devin(&body, saved.as_ref()) {
            Ok(request) => request,
            Err(message) => return error(StatusCode::BAD_REQUEST, message),
        };
        let history = request["messages"].clone();
        (
            crate::url::build_provider_url(
                member.kind(),
                member.upstream_override.as_deref(),
                "/exa.api_server_pb.ApiServerService/GetChatMessage",
            ),
            request,
            history,
        )
    } else {
        let mut request = match crate::compat::gemini::interactions_to_gemini(&body) {
            Ok(request) => request,
            Err(message) => return error(StatusCode::BAD_REQUEST, message),
        };
        if let Some(saved) = &saved {
            let mut contents = saved.contents.as_array().cloned().unwrap_or_default();
            contents.extend(request["contents"].as_array().cloned().unwrap_or_default());
            request["contents"] = json!(contents);
        }
        let history = request["contents"].clone();
        (
            crate::url::build_antigravity_url(member.upstream_override.as_deref()),
            request,
            history,
        )
    };
    if let Err(message) = settings.payload.apply(
        &mut business,
        &crate::management::settings::PayloadContext {
            model: &model,
            requested_model: &target,
            protocol: if native {
                "interactions"
            } else if devin {
                "openai"
            } else {
                "gemini"
            },
            from_protocol: "interactions",
            headers: &headers,
            original: &original,
        },
    ) {
        return error(StatusCode::BAD_REQUEST, message);
    }
    let wire = if native || devin {
        business
    } else {
        let Some(project) = member.project_id() else {
            return error(
                StatusCode::BAD_REQUEST,
                "Antigravity account missing project_id",
            );
        };
        let labels = crate::v1beta::labels_for(&member, &model, model.starts_with("claude-"));
        crate::v1beta::wrap_for_antigravity_with_labels(&model, &project, &business, &labels)
    };
    let activity = member.begin_activity();
    let started = Instant::now();
    let sent = if devin {
        let uid = model.strip_prefix("devin/").unwrap_or(&model);
        let params = crate::compat::devin::DevinRequestParams {
            token: member.access_token(),
            chat_model_uid: uid.to_string(),
            supports_vision: pool.devin_model_supports_vision(&member.id, uid),
            trajectory_id: uuid::Uuid::new_v4().to_string(),
            cascade_id: uuid::Uuid::new_v4().to_string(),
            execution_id: uuid::Uuid::new_v4().to_string(),
            fingerprint: "0123456789abcdef0123456789abcdef".into(),
        };
        let wire = match crate::compat::devin::build_chat_request(&wire, &params, &mut || {
            uuid::Uuid::new_v4().to_string()
        }) {
            Ok(wire) => wire,
            Err(message) => return error(StatusCode::BAD_REQUEST, message),
        };
        let client = match state.try_devin_client_for_member(&member) {
            Ok(client) => client,
            Err(message) => return error(StatusCode::BAD_GATEWAY, message.to_string()),
        };
        client
            .post(&url)
            .header(
                header::AUTHORIZATION,
                crate::compat::devin::authorization_header(&params.token),
            )
            .header(
                header::CONTENT_TYPE,
                crate::compat::devin::STREAM_CONTENT_TYPE,
            )
            .header(
                "connect-protocol-version",
                crate::compat::devin::CONNECT_PROTOCOL_VERSION,
            )
            .body(crate::compat::devin::frame_data(&wire))
            .send()
            .await
            .map_err(|error| error.to_string())
    } else {
        crate::relay::send_interactions(&state, &member, &url, &wire).await
    };
    let response = match sent {
        Ok(response) => response,
        Err(message) => return error(StatusCode::BAD_GATEWAY, message.to_string()),
    };
    let status = response.status();
    if !status.is_success() {
        let response = match axum::body::to_bytes(
            Body::from_stream(response.bytes_stream()),
            MAX_JSON,
        )
        .await
        {
            Ok(raw) => (status, [(header::CONTENT_TYPE, "application/json")], raw).into_response(),
            Err(message) => error(StatusCode::BAD_GATEWAY, message.to_string()),
        };
        return crate::relay::count_interactions_response(
            state,
            member,
            &auth,
            &headers,
            target,
            raw.len(),
            started,
            activity,
            response,
        );
    }
    if devin
        && response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_none_or(|value| {
                value.split(';').next().unwrap_or("").trim()
                    != crate::compat::devin::STREAM_CONTENT_TYPE
            })
    {
        return error(
            StatusCode::BAD_GATEWAY,
            "invalid Devin Connect content type",
        );
    }
    let mut observer = Observer::new(native, model, namespace, history, store);
    observer.owner = owner;
    observer.devin = devin;
    observer.function_names = crate::compat::gemini::interactions_function_names(&original)
        .into_iter()
        .map(|(original, mapped)| (mapped, original))
        .collect();
    // Antigravity always returns SSE; native non-stream bodies remain JSON.
    if native && !stream {
        let bytes = match axum::body::to_bytes(Body::from_stream(response.bytes_stream()), MAX_JSON)
            .await
        {
            Ok(bytes) => bytes,
            Err(message) => return error(StatusCode::BAD_GATEWAY, message.to_string()),
        };
        let value = match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => value,
            Err(message) => return error(StatusCode::BAD_GATEWAY, message.to_string()),
        };
        let mut observer = observer;
        if let Err(message) = observer.observe_native(&value) {
            return error(StatusCode::BAD_GATEWAY, message);
        }
        let response = (status, Json(value)).into_response();
        return crate::relay::count_interactions_response(
            state,
            member,
            &auth,
            &headers,
            target,
            raw.len(),
            started,
            activity,
            response,
        );
    }
    let body = if devin {
        devin_stream_body(response, observer)
    } else {
        stream_body(response, observer)
    };
    if stream {
        let response = (
            status,
            [
                (header::CONTENT_TYPE, "text/event-stream"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            body,
        )
            .into_response();
        return crate::relay::count_interactions_response(
            state,
            member,
            &auth,
            &headers,
            target,
            raw.len(),
            started,
            activity,
            response,
        );
    }
    let response = match axum::body::to_bytes(body, MAX_JSON).await {
        Ok(bytes) => {
            let mut parser = SseParser::default();
            let mut frames = Vec::new();
            if let Err(message) = parser.push_raw_data(&bytes, &mut frames) {
                return error(StatusCode::BAD_GATEWAY, message.message());
            }
            for frame in frames.into_iter().rev() {
                if let Ok(value) = serde_json::from_slice::<Value>(&frame) {
                    if value.get("event_type").and_then(Value::as_str)
                        == Some("interaction.completed")
                    {
                        let response = (status, Json(value["interaction"].clone())).into_response();
                        return crate::relay::count_interactions_response(
                            state,
                            member,
                            &auth,
                            &headers,
                            target,
                            raw.len(),
                            started,
                            activity,
                            response,
                        );
                    }
                }
            }
            error(
                StatusCode::BAD_GATEWAY,
                "upstream did not complete interaction",
            )
        }
        Err(message) => error(StatusCode::BAD_GATEWAY, message.to_string()),
    };
    crate::relay::count_interactions_response(
        state,
        member,
        &auth,
        &headers,
        target,
        raw.len(),
        started,
        activity,
        response,
    )
}

fn interactions_to_devin(body: &Value, saved: Option<&Continuation>) -> Result<Value, String> {
    let mut messages = saved
        .and_then(|saved| saved.contents.as_array().cloned())
        .unwrap_or_default();
    if saved.is_none() {
        if let Some(system) = body.get("system_instruction") {
            messages.push(json!({"role":"system","content":system}));
        }
    }
    let input = body.get("input").ok_or("missing input")?;
    let steps = if let Some(text) = input.as_str() {
        vec![json!({"type":"user_input","content":text})]
    } else {
        input
            .as_array()
            .cloned()
            .ok_or("input must be a string or array")?
    };
    for step in steps {
        match step.get("type").and_then(Value::as_str) {
            Some("user_input" | "model_output") => {
                let content = step
                    .get("content")
                    .cloned()
                    .ok_or("input step requires content")?;
                let mut reasoning = String::new();
                let mut signature = None;
                let content = if let Some(parts) = content.as_array() {
                    let mut mapped = Vec::new();
                    for part in parts {
                        match part.get("type").and_then(Value::as_str) {
                            Some("text") => mapped.push(json!({"type":"text","text":part["text"]})),
                            Some("image") => {
                                let url =
                                    if let Some(data) = part.get("data").and_then(Value::as_str) {
                                        format!(
                                            "data:{};base64,{data}",
                                            part.get("mime_type")
                                                .and_then(Value::as_str)
                                                .unwrap_or("image/png")
                                        )
                                    } else {
                                        part.get("uri")
                                            .and_then(Value::as_str)
                                            .ok_or("image requires data or uri")?
                                            .to_string()
                                    };
                                mapped.push(json!({"type":"image_url","image_url":{"url":url}}));
                            }
                            Some("thought") => {
                                reasoning.push_str(
                                    part.get("text").and_then(Value::as_str).unwrap_or(""),
                                );
                                if let Some(value) = part
                                    .get("thought_signature")
                                    .or_else(|| part.get("thoughtSignature"))
                                {
                                    signature = Some(value.clone());
                                }
                            }
                            _ => return Err("unsupported Devin content part".into()),
                        }
                    }
                    json!(mapped)
                } else {
                    content
                };
                let mut message = json!({"role":if step["type"] == "user_input" {"user"} else {"assistant"},"content":content});
                if !reasoning.is_empty() {
                    message["reasoning_content"] = json!(reasoning);
                }
                if let Some(signature) = signature {
                    message["reasoning_signature"] = signature;
                }
                messages.push(message);
            }
            Some("thought") => {
                let content = step.get("text").or_else(|| step.get("content"));
                let text = match content {
                    Some(Value::String(text)) => text.clone(),
                    Some(Value::Array(parts)) => parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<String>(),
                    None => String::new(),
                    _ => return Err("thought content must be a string or array".into()),
                };
                messages.push(json!({"role":"assistant","content":"","reasoning_content":text,"reasoning_signature":step.get("thought_signature").or_else(|| step.get("signature")).or_else(|| step.get("thoughtSignature")),"reasoning_redacted":step.get("redacted")}));
            }
            Some("function_call") => {
                let id = step
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or("function_call requires id")?;
                let name = step
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .ok_or("function_call requires name")?;
                let arguments = step.get("arguments").cloned().unwrap_or(json!({}));
                let arguments = if arguments.is_string() {
                    arguments
                } else {
                    json!(arguments.to_string())
                };
                messages.push(json!({"role":"assistant","content":null,"tool_calls":[{"type":"function","id":id,"function":{"name":name,"arguments":arguments}}]}));
            }
            Some("function_result") => {
                let result = step
                    .get("result")
                    .or_else(|| step.get("output"))
                    .cloned()
                    .ok_or("function_result requires result")?;
                messages.push(json!({"role":"tool","tool_call_id":step["call_id"],"content":if result.is_string() {result} else {json!(result.to_string())},"is_error":step.get("is_error")}));
            }
            _ => return Err("unsupported Devin interaction step".into()),
        }
    }
    let mut request = json!({"model":body.get("model"),"messages":messages});
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        request["tools"] = json!(tools
            .iter()
            .map(|tool| json!({"type":"function","function":tool}))
            .collect::<Vec<_>>());
    }
    if let Some(config) = body.get("generation_config").and_then(Value::as_object) {
        for (source, target) in [
            ("temperature", "temperature"),
            ("top_p", "top_p"),
            ("max_output_tokens", "max_tokens"),
        ] {
            if let Some(value) = config.get(source) {
                request[target] = value.clone();
            }
        }
    }
    Ok(request)
}

fn devin_events(
    observer: &mut Observer,
    calls: &mut BTreeMap<u64, usize>,
    events: Vec<crate::compat::events::CodexEvent>,
) -> Result<Vec<u8>, String> {
    use crate::compat::events::CodexEvent;
    let mut out = Vec::new();
    for value in events {
        if !observer.started {
            out.extend(event("interaction.created", json!({"interaction":{"id":observer.id,"object":"interaction","model":observer.model,"status":"in_progress"}})));
            observer.started = true;
        }
        let step = match value {
            CodexEvent::Created { response_id } => {
                observer.id = response_id;
                None
            }
            CodexEvent::TextDelta(text) => {
                Some(json!({"type":"model_output","content":[{"type":"text","text":text}]}))
            }
            CodexEvent::ReasoningDelta(text) => Some(json!({"type":"thought","text":text})),
            CodexEvent::ReasoningSignature(signature) => {
                if let Some(step) = observer
                    .steps
                    .iter_mut()
                    .rev()
                    .find(|step| step["type"] == "thought")
                {
                    step["thought_signature"] = json!(signature);
                }
                None
            }
            CodexEvent::ReasoningRedacted => Some(json!({"type":"thought","redacted":true})),
            CodexEvent::ToolCallBegin {
                output_index,
                call_id,
                name,
            } => {
                calls.insert(output_index, observer.steps.len());
                Some(json!({"type":"function_call","id":call_id,"name":name,"arguments":""}))
            }
            CodexEvent::ToolArgsDelta {
                output_index,
                delta,
            } => {
                let index = *calls
                    .get(&output_index)
                    .ok_or("Devin arguments without function call")?;
                let arguments = observer.steps[index]["arguments"]
                    .as_str()
                    .unwrap_or("")
                    .to_string()
                    + &delta;
                observer.steps[index]["arguments"] = json!(arguments);
                out.extend(event(
                    "step.delta",
                    json!({"index":index,"delta":{"type":"arguments_delta","arguments":delta}}),
                ));
                None
            }
            CodexEvent::Completed { usage } => {
                observer.finished = true;
                if let Some(usage) = usage {
                    observer.usage = Some(
                        json!({"promptTokenCount":usage.prompt_tokens,"candidatesTokenCount":usage.completion_tokens,"totalTokenCount":usage.total_tokens,"thoughtsTokenCount":usage.reasoning_tokens}),
                    );
                    if usage.cached_tokens_known {
                        observer.usage.as_mut().unwrap()["cachedContentTokenCount"] =
                            json!(usage.cached_tokens);
                    }
                }
                None
            }
            CodexEvent::Failed { message } => return Err(message),
            CodexEvent::OutputLimitReached | CodexEvent::ContentFilterReached => None,
            CodexEvent::KiroRedactedReasoning(_) => {
                return Err("unexpected Devin reasoning event".into())
            }
        };
        if let Some(mut step) = step {
            let index = observer.steps.len();
            if step.get("id").is_none() {
                step["id"] = json!(format!("step_{index}"));
            }
            out.extend(event("step.start", json!({"index":index,"step":step})));
            if let Some(content) = step.pointer("/content/0") {
                out.extend(event("step.delta", json!({"index":index,"delta":content})));
            } else if step["type"] == "thought" {
                out.extend(event("step.delta", json!({"index":index,"delta":step})));
            }
            observer.steps.push(step);
        }
    }
    Ok(out)
}

fn devin_stream_body(response: reqwest::Response, observer: Observer) -> Body {
    Body::from_stream(futures::stream::unfold(
        (
            response.bytes_stream(),
            crate::compat::devin::DevinDecoder::new(),
            observer,
            BTreeMap::new(),
            false,
        ),
        |(mut source, mut decoder, mut observer, mut calls, ended)| async move {
            if ended {
                return None;
            }
            let mut events = Vec::new();
            let next = source.next().await;
            let eof = next.is_none();
            let result = match next {
                Some(Ok(bytes)) => {
                    observer.observed_bytes = observer.observed_bytes.saturating_add(bytes.len());
                    if observer.observed_bytes > MAX_JSON {
                        Err("interaction continuation exceeds size limit".to_string())
                    } else {
                        decoder.decode(&bytes, &mut events);
                        Ok(())
                    }
                }
                Some(Err(error)) => Err(error.to_string()),
                None => {
                    decoder.finish(&mut events);
                    Ok(())
                }
            };
            let result = result
                .and_then(|()| devin_events(&mut observer, &mut calls, events))
                .and_then(|mut out| {
                    if eof {
                        for &index in calls.values() {
                            let arguments =
                                observer.steps[index]["arguments"].as_str().unwrap_or("");
                            observer.steps[index]["arguments"] = serde_json::from_str(arguments)
                                .map_err(|error| error.to_string())?;
                        }
                        for (index, step) in observer.steps.iter().enumerate() {
                            out.extend(event(
                                "step.stop",
                                json!({"index":index,"step_id":step["id"]}),
                            ));
                        }
                        out.extend(observer.finish()?);
                    }
                    Ok(out)
                });
            let failed = result.is_err();
            let out = result
                .unwrap_or_else(|message| event("error", json!({"error":{"message":message}})));
            Some((
                Ok::<_, std::io::Error>(Bytes::from(out)),
                (source, decoder, observer, calls, eof || failed),
            ))
        },
    ))
}

struct Observer {
    native: bool,
    devin: bool,
    model: String,
    namespace: String,
    owner: String,
    id: String,
    environment: Option<Value>,
    history: Value,
    parts: Vec<Value>,
    steps: Vec<Value>,
    usage: Option<Value>,
    finished: bool,
    completed: bool,
    started: bool,
    observed_bytes: usize,
    function_names: BTreeMap<String, String>,
    store: SharedInteractionsStore,
}

impl Observer {
    fn new(
        native: bool,
        model: String,
        namespace: String,
        history: Value,
        store: SharedInteractionsStore,
    ) -> Self {
        Self {
            native,
            devin: false,
            model,
            owner: namespace.clone(),
            namespace,
            id: format!("interaction_{}", uuid::Uuid::new_v4()),
            environment: None,
            history,
            parts: Vec::new(),
            steps: Vec::new(),
            usage: None,
            finished: false,
            completed: false,
            started: false,
            observed_bytes: 0,
            function_names: BTreeMap::new(),
            store,
        }
    }

    fn save(&self, interaction: &Value) -> Result<(), String> {
        let status = interaction.get("status").and_then(Value::as_str);
        if !matches!(status, Some("requires_action" | "completed")) {
            return Ok(());
        }
        let calls: Vec<_> = interaction
            .get("steps")
            .and_then(Value::as_array)
            .unwrap_or(&self.steps)
            .iter()
            .filter(|step| step.get("type").and_then(Value::as_str) == Some("function_call"))
            .cloned()
            .collect();
        let key = if calls.is_empty() {
            None
        } else {
            Some(call_key(calls.iter().map(|call| {
                call.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            }))?)
        };
        let id = interaction
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or("completed interaction requires id")?;
        let mut contents = self.history.as_array().cloned().unwrap_or_default();
        if self.devin {
            let response = json!({"input": self.steps});
            let messages = interactions_to_devin(&response, None)?;
            contents.extend(messages["messages"].as_array().cloned().unwrap_or_default());
        } else if !self.parts.is_empty() {
            contents.push(json!({"role":"model","parts":self.parts}));
        }
        let now = Instant::now();
        let entry = Continuation {
            id: id.into(),
            owner: self.owner.clone(),
            namespace: self.namespace.clone(),
            environment: interaction
                .get("environment_id")
                .cloned()
                .or_else(|| self.environment.clone()),
            contents: json!(contents),
            calls,
            expires: now + TTL,
        };
        let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        store.put(
            format!("{}:interaction:{id}", self.namespace),
            entry.clone(),
            now,
        );
        if let Some(key) = key {
            store.put(format!("{}:{key}", self.namespace), entry, now);
        }
        Ok(())
    }

    fn observe_native(&mut self, value: &Value) -> Result<(), String> {
        let event = value.get("event_type").and_then(Value::as_str);
        let interaction = value.get("interaction").unwrap_or(value);
        if let Some(id) = interaction.get("id").and_then(Value::as_str) {
            self.id = id.into();
        }
        if let Some(environment) = interaction.get("environment_id") {
            self.environment = Some(environment.clone());
        }
        if event == Some("step.start")
            && value.pointer("/step/type").and_then(Value::as_str) == Some("function_call")
        {
            self.steps.push(value["step"].clone());
        }
        if event.is_none() || event == Some("interaction.completed") {
            self.save(interaction)?;
            self.completed = true;
        }
        Ok(())
    }

    fn frame(&mut self, raw: &[u8]) -> Result<Vec<u8>, String> {
        if raw == b"[DONE]" {
            return Ok(Vec::new());
        }
        self.observed_bytes = self.observed_bytes.saturating_add(raw.len());
        if self.observed_bytes > MAX_JSON {
            return Err("interaction continuation exceeds size limit".into());
        }
        let value: Value = serde_json::from_slice(raw).map_err(|error| error.to_string())?;
        if self.native {
            self.observe_native(&value)?;
            return Ok(Vec::new());
        }
        let root = value.get("response").unwrap_or(&value);
        if root.get("error").is_some() {
            return Err(root["error"].to_string());
        }
        if let Some(id) = root.get("responseId").and_then(Value::as_str) {
            self.id = id.into();
        }
        if let Some(usage) = root.get("usageMetadata") {
            self.usage = Some(usage.clone());
        }
        let mut out = Vec::new();
        if !self.started {
            out.extend(event("interaction.created", json!({"interaction":{"id":self.id,"object":"interaction","model":self.model,"status":"in_progress"}})));
            out.extend(event(
                "interaction.status_update",
                json!({"interaction":{"id":self.id,"status":"in_progress"}}),
            ));
            self.started = true;
        }
        if let Some(parts) = root
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
        {
            for part in parts {
                let step = if let Some(call) = part.get("functionCall") {
                    let id = call
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4()));
                    let mut preserved = part.clone();
                    preserved["functionCall"]["id"] = json!(id);
                    self.parts.push(preserved);
                    let mut step = json!({"type":"function_call","id":id,"name":call["name"],"arguments":call.get("args").cloned().unwrap_or_else(||json!({}))});
                    if let Some(name) = call
                        .get("name")
                        .and_then(Value::as_str)
                        .and_then(|name| self.function_names.get(name))
                    {
                        step["name"] = json!(name);
                    }
                    if let Some(signature) = part.get("thoughtSignature") {
                        step["thought_signature"] = signature.clone();
                    }
                    step
                } else {
                    self.parts.push(part.clone());
                    if let Some(text) = part.get("text") {
                        let mut content = json!({"type":if part.get("thought").and_then(Value::as_bool) == Some(true) {"thought"} else {"text"},"text":text});
                        if let Some(signature) = part.get("thoughtSignature") {
                            content["thought_signature"] = signature.clone();
                        }
                        json!({"type":"model_output","id":format!("step_{}",self.steps.len()),"content":[content]})
                    } else if let Some(media) =
                        part.get("inlineData").or_else(|| part.get("fileData"))
                    {
                        let mime = media.get("mimeType").and_then(Value::as_str).unwrap_or("");
                        let kind = if mime.starts_with("image/") {
                            "image"
                        } else if mime.starts_with("audio/") {
                            "audio"
                        } else if mime.starts_with("video/") {
                            "video"
                        } else {
                            "document"
                        };
                        let mut content = json!({"type":kind,"mime_type":mime});
                        if let Some(data) = media.get("data") {
                            content["data"] = data.clone();
                        }
                        if let Some(uri) = media.get("fileUri") {
                            content["uri"] = uri.clone();
                        }
                        json!({"type":"model_output","id":format!("step_{}",self.steps.len()),"content":[content]})
                    } else if part.get("thoughtSignature").is_some() {
                        out.extend(event("step.delta", json!({"index":self.steps.len(),"delta":{"type":"thought_signature","signature":part["thoughtSignature"]}})));
                        continue;
                    } else {
                        return Err("unsupported Antigravity response part".into());
                    }
                };
                let index = self.steps.len();
                out.extend(event("step.start", json!({"index":index,"step":step})));
                if let Some(content) = step.pointer("/content/0") {
                    out.extend(event("step.delta", json!({"index":index,"delta":content})));
                } else if step.get("type").and_then(Value::as_str) == Some("function_call") {
                    out.extend(event("step.delta", json!({"index":index,"delta":{"type":"arguments_delta","arguments":step["arguments"].to_string()}})));
                }
                out.extend(event(
                    "step.stop",
                    json!({"index":index,"step_id":step["id"]}),
                ));
                self.steps.push(step);
            }
        }
        if root.pointer("/candidates/0/finishReason").is_some() {
            self.finished = true;
        }
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<u8>, String> {
        if self.completed {
            return Ok(Vec::new());
        }
        if self.native {
            return Err("stream truncated before interaction.completed".into());
        }
        if !self.finished {
            return Err("stream truncated before finish reason".into());
        }
        let requires_action = self
            .steps
            .iter()
            .any(|step| step.get("type").and_then(Value::as_str) == Some("function_call"));
        let mut interaction = json!({"id":self.id,"object":"interaction","model":self.model,
            "status":if requires_action {"requires_action"} else {"completed"},"steps":self.steps});
        if let Some(usage) = &self.usage {
            let mut mapped = json!({"input_tokens":usage.get("promptTokenCount"),
                "output_tokens":usage.get("candidatesTokenCount"),"total_tokens":usage.get("totalTokenCount")});
            if let Some(cached) = usage.get("cachedContentTokenCount") {
                mapped["cached_tokens"] = cached.clone();
            }
            if let Some(reasoning) = usage.get("thoughtsTokenCount") {
                mapped["reasoning_tokens"] = reasoning.clone();
            }
            interaction["usage"] = mapped;
        }
        self.save(&interaction)?;
        self.completed = true;
        Ok(event(
            "interaction.completed",
            json!({"interaction":interaction}),
        ))
    }
}

fn event(kind: &str, mut value: Value) -> Vec<u8> {
    value["event_type"] = json!(kind);
    format!("event: {kind}\ndata: {value}\n\n").into_bytes()
}

fn stream_body(response: reqwest::Response, observer: Observer) -> Body {
    let source = response.bytes_stream();
    Body::from_stream(futures::stream::unfold(
        (source, SseParser::default(), observer, false),
        |(mut source, mut parser, mut observer, ended)| async move {
            if ended {
                return None;
            }
            let mut frames = Vec::new();
            let next = source.next().await;
            let eof = next.is_none();
            let mut out = Vec::new();
            let result = match &next {
                Some(Ok(bytes)) => parser.push_raw_data(bytes, &mut frames),
                Some(Err(error)) => {
                    out.extend(event(
                        "error",
                        json!({"error":{"message":error.to_string()}}),
                    ));
                    return Some((
                        Ok::<_, std::io::Error>(Bytes::from(out)),
                        (source, parser, observer, true),
                    ));
                }
                None => parser.finish_raw_data(&mut frames),
            };
            let mut ended = false;
            if let Err(error) = result {
                out.extend(event("error", json!({"error":{"message":error.message()}})));
                ended = true;
            } else {
                for frame in frames {
                    match observer.frame(&frame) {
                        Ok(bytes) => out.extend(bytes),
                        Err(error) => {
                            out.extend(event("error", json!({"error":{"message":error}})));
                            ended = true;
                            break;
                        }
                    }
                }
                if next.is_none() && !ended {
                    match observer.finish() {
                        Ok(bytes) => out.extend(bytes),
                        Err(error) => {
                            out.extend(event("error", json!({"error":{"message":error}})))
                        }
                    }
                }
                if observer.native && !ended {
                    if let Some(Ok(bytes)) = next {
                        out = bytes.to_vec();
                    }
                }
            }
            ended |= eof;
            Some((
                Ok::<_, std::io::Error>(Bytes::from(out)),
                (source, parser, observer, ended),
            ))
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    struct DevinFixture {
        state: Arc<AppState>,
        store: SharedInteractionsStore,
        requests:
            tokio::sync::mpsc::UnboundedReceiver<crate::compat::devin_proto::GetChatMessageRequest>,
        server: tokio::task::JoinHandle<()>,
        directory: std::path::PathBuf,
    }

    impl Drop for DevinFixture {
        fn drop(&mut self) {
            self.server.abort();
            // The continuation store may still be flushing this directory from
            // a spawned writer; a non-empty temp dir is not a test failure.
            if let Err(error) = std::fs::remove_dir_all(&self.directory) {
                eprintln!(
                    "devin fixture cleanup skipped for {}: {error}",
                    self.directory.display()
                );
            }
        }
    }

    impl DevinFixture {
        async fn new(tool: bool) -> Self {
            use crate::compat::{devin, devin_proto};
            use mahoquot_registry::{
                AuthorityMask, CatalogSource, CatalogVersion, DiscoveredModel, ModelCapability,
                ModelId, ProviderContribution, ProviderId, ProviderPolicy, RegistryBuilder,
            };

            let (sender, requests) = tokio::sync::mpsc::unbounded_channel();
            let app = axum::Router::new().route(
                "/exa.api_server_pb.ApiServerService/GetChatMessage",
                axum::routing::post(move |headers: HeaderMap, bytes: Bytes| {
                    let sender = sender.clone();
                    async move {
                        assert_eq!(
                            headers[header::AUTHORIZATION],
                            "Basic fixture-token-fixture-token"
                        );
                        assert_eq!(headers[header::CONTENT_TYPE], devin::STREAM_CONTENT_TYPE);
                        assert_eq!(bytes[0], 0);
                        assert_eq!(
                            u32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize,
                            bytes.len() - 5
                        );
                        sender
                            .send(devin_proto::GetChatMessageRequest::decode(&bytes[5..]).unwrap())
                            .unwrap();
                        let message = devin_proto::ChatMessageResponse {
                            delta_text: (!tool).then(|| "fixture answer".into()),
                            delta_tool_calls: if tool {
                                vec![devin_proto::ChatToolCall {
                                    id: Some("call-fixture".into()),
                                    name: Some("lookup".into()),
                                    arguments_json: Some("{\"q\":1}".into()),
                                }]
                            } else {
                                Vec::new()
                            },
                            ..Default::default()
                        };
                        let mut wire = devin::frame_data(&message.encode_to_vec());
                        wire.extend(devin::frame_end_stream("{}"));
                        ([(header::CONTENT_TYPE, devin::STREAM_CONTENT_TYPE)], wire)
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let directory =
                std::env::temp_dir().join(format!("interactions-devin-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join("devin.json"),
                json!({
                    "type":"devin", "identity_slug":"fixture", "access_token":"fixture-token",
                    "api_server_url":url, "upstream_override":url, "disabled":false
                })
                .to_string(),
            )
            .unwrap();
            let config = crate::config::GatewayConfig {
                auth_dir: directory.clone(),
                config_path: directory.join("config.yaml"),
                auth_refresh_enabled: false,
                ..Default::default()
            };
            let state = Arc::new(AppState::new(&config).unwrap());
            let mut registry =
                RegistryBuilder::new(CatalogVersion::new(1), CatalogSource::Discovered);
            registry.register_provider(ProviderId::devin(), ProviderPolicy::Discovered);
            registry
                .apply_contribution(
                    ProviderContribution::from_discovered_models(
                        ProviderId::devin(),
                        vec![
                            DiscoveredModel::new(ModelId::new("devin/fixture-model").unwrap())
                                .with_capabilities([ModelCapability::Chat, ModelCapability::Tools])
                                .with_authority(AuthorityMask::MODELS_ONLY),
                        ],
                    )
                    .with_policy(ProviderPolicy::Discovered),
                )
                .unwrap();
            let current = state.pool.load_full();
            assert_eq!(current.members.len(), 1);
            let member = &current.members[0];
            assert_eq!(member.kind(), ProviderKind::Devin);
            let key = crate::devin_catalog::DevinCacheKey::new(
                &member.id,
                &member.access_token(),
                member.effective_base_url(),
            );
            let model = crate::devin_catalog::DiscoveredDevinModel {
                model_uid: "fixture-model".into(),
                public_id: "devin/fixture-model".into(),
                label: "Fixture".into(),
                supports_images: true,
                is_premium: false,
                is_beta: false,
                is_recommended: false,
                is_new: false,
                is_capacity_limited: false,
                promo_active: false,
                max_tokens: Some(4096),
                credit_multiplier: Some(1.0),
                description: None,
            };
            member.set_devin_catalog_state(Arc::new(
                crate::devin_catalog::DevinAccountCatalogState::new_success(key, vec![model], 100),
            ));
            state
                .pool
                .store(Arc::new(crate::runtime_state::PoolSnapshot::new(
                    current.generation + 1,
                    current.members.clone(),
                    current.models.clone(),
                    Arc::new(registry.build().unwrap()),
                )));
            Self {
                state,
                store: Arc::new(Mutex::new(InteractionsStore::default())),
                requests,
                server,
                directory,
            }
        }

        async fn request(&self, body: Value) -> (StatusCode, Value) {
            let mut headers = HeaderMap::new();
            headers.insert("x-session-id", "fixture-session".parse().unwrap());
            let response = handle(
                State(self.state.clone()),
                Extension(ResolvedAuth {
                    identity: crate::inbound::AuthIdentity::Unrestricted,
                    key_identifier: None,
                }),
                Extension(self.store.clone()),
                headers,
                Bytes::from(body.to_string()),
            )
            .await;
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), MAX_JSON)
                .await
                .unwrap();
            (status, serde_json::from_slice(&bytes).unwrap())
        }

        async fn received(&mut self) -> crate::compat::devin_proto::GetChatMessageRequest {
            tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
                .await
                .unwrap()
                .unwrap()
        }
    }

    #[tokio::test]
    async fn devin_only_text_user_input_reaches_wire_and_completes() {
        let mut fixture = DevinFixture::new(false).await;
        let (status, response) = fixture.request(json!({"model":"devin/fixture-model","input":[{"type":"user_input","content":"hello Devin"}]})).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response["status"], "completed");
        assert_eq!(response["steps"][0]["content"][0]["text"], "fixture answer");
        let wire = fixture.received().await;
        assert_eq!(wire.chat_model_uid.as_deref(), Some("fixture-model"));
        assert_eq!(
            wire.chat_message_prompts[0].prompt.as_deref(),
            Some("hello Devin")
        );
        let (status, _) = fixture
            .request(json!({"agent":"fixture-agent","input":"hi"}))
            .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn devin_image_user_input_preserves_base64_bytes() {
        let mut fixture = DevinFixture::new(false).await;
        let data = "AAECA//+/w==";
        let (status, response) = fixture.request(json!({"model":"devin/fixture-model","input":[{"type":"user_input","content":[{"type":"text","text":"image"},{"type":"image","mime_type":"image/png","data":data}]}]})).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        let wire = fixture.received().await;
        let image = &wire.chat_message_prompts[0].images[0];
        assert_eq!(
            image.base64_data.as_deref().unwrap().as_bytes(),
            data.as_bytes()
        );
        assert_eq!(image.mime_type.as_deref(), Some("image/png"));
    }

    #[tokio::test]
    async fn devin_function_result_continuation_pairs_pending_call() {
        let mut fixture = DevinFixture::new(true).await;
        let (status, first) = fixture
            .request(json!({"model":"devin/fixture-model","input":"lookup"}))
            .await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["status"], "requires_action");
        fixture.received().await;
        let (status, response) = fixture.request(json!({"model":"devin/fixture-model","previous_interaction_id":first["id"],"input":[{"type":"function_result","call_id":"call-fixture","result":{"answer":42}}]})).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        let wire = fixture.received().await;
        let call = &wire.chat_message_prompts[1].tool_calls[0];
        assert_eq!(call.id.as_deref(), Some("call-fixture"));
        assert_eq!(call.name.as_deref(), Some("lookup"));
        assert_eq!(call.arguments_json.as_deref(), Some("{\"q\":1}"));
        let result = &wire.chat_message_prompts[2];
        assert_eq!(result.tool_call_id, call.id);
        assert_eq!(result.prompt.as_deref(), Some("{\"answer\":42}"));
        let (status, _) = fixture.request(json!({"model":"devin/fixture-model","previous_interaction_id":first["id"],"input":[{"type":"function_result","call_id":"unknown","result":42}]})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    fn observer(native: bool, scope: &str, store: SharedInteractionsStore) -> Observer {
        Observer::new(
            native,
            "fixture-model".into(),
            scope.into(),
            json!([{"role":"user","parts":[{"text":"hi"}]}]),
            store,
        )
    }

    #[test]
    fn request_target_when_model_agent_or_invalid_types() {
        let valid = br#"{"model":" models/fixture-model ","input":"hi","stream":true}"#;
        let (_, target, agent, stream) = parse_request(valid).unwrap();
        assert_eq!(target, "fixture-model");
        assert!(!agent && stream);
        assert!(
            parse_request(br#"{"agent":"fixture-agent","input":"hi"}"#)
                .unwrap()
                .2
        );
        for invalid in [
            json!({"model":"a","agent":"b","input":"hi"}),
            json!({"model":4,"input":"hi"}),
            json!({"model":"a","stream":1,"input":"hi"}),
            json!({"model":"a","contents":[]}),
            json!({"model":" ","input":"hi"}),
            json!({"model":"a","agent":false,"input":"hi"}),
        ] {
            assert!(parse_request(invalid.to_string().as_bytes()).is_err());
        }
    }

    #[test]
    fn continuation_when_all_scope_dimensions_and_pending_ids_match() {
        let store = Arc::new(Mutex::new(InteractionsStore::default()));
        let initial = json!({"model":"m","input":[{"type":"user_input","content":"hi"}]});
        let headers = HeaderMap::new();
        let scope = namespace(&initial, "caller-a", "credential-a", "m", &headers);
        let mut source = observer(true, &scope, store.clone());
        source.observe_native(&json!({"id":"upstream-id","environment_id":"environment","status":"requires_action",
            "steps":[{"type":"function_call","id":"call-1","name":"lookup","arguments":{},"thought_signature":"opaque"}]})).unwrap();
        let results = json!({"model":"m","input":[{"type":"user_input","content":"hi"},{"type":"function_result","call_id":"call-1","result":42}]});
        let mut request = results.clone();
        let restored = prepare(
            &mut request,
            true,
            &scope,
            &mut store.lock().unwrap(),
            Instant::now(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(restored.id, "upstream-id");
        assert_eq!(request["previous_interaction_id"], "upstream-id");
        assert_eq!(request["environment_id"], "environment");
        assert_eq!(request["input"].as_array().unwrap().len(), 1);
        for (caller, credential, model, body) in [
            ("caller-b", "credential-a", "m", initial.clone()),
            ("caller-a", "credential-b", "m", initial.clone()),
            ("caller-a", "credential-a", "other", initial.clone()),
            (
                "caller-a",
                "credential-a",
                "m",
                json!({"input":[{"type":"user_input","content":"other conversation"}]}),
            ),
        ] {
            let wrong = namespace(&body, caller, credential, model, &headers);
            assert!(prepare(
                &mut results.clone(),
                true,
                &wrong,
                &mut store.lock().unwrap(),
                Instant::now()
            )
            .is_err());
        }
        assert!(call_key(vec!["same".into(), "same".into()]).is_err());
        assert!(call_key(vec!["a\0b".into()]).is_err());
        assert_eq!(
            call_key(vec!["b".into(), "a".into()]),
            call_key(vec!["a".into(), "b".into()])
        );
    }

    #[test]
    fn cache_when_capacity_and_expiry_boundaries_are_reached() {
        let now = Instant::now();
        let mut store = InteractionsStore::default();
        for index in 0..=CAPACITY {
            store.put(
                index.to_string(),
                Continuation {
                    id: index.to_string(),
                    owner: "owner".into(),
                    namespace: "scope".into(),
                    environment: None,
                    contents: json!([]),
                    calls: vec![],
                    expires: now + TTL + Duration::from_secs(u64::try_from(index).unwrap()),
                },
                now,
            );
        }
        assert_eq!(store.entries.len(), CAPACITY);
        assert!(store.get("0", now).is_none());
        assert!(store.get("1", now + TTL).is_some());
        assert!(store.get("1", now + TTL + Duration::from_secs(1)).is_none());
    }

    #[test]
    fn antigravity_when_stop_precedes_usage_preserves_signed_continuation() {
        let store = Arc::new(Mutex::new(InteractionsStore::default()));
        let mut source = observer(false, "scope", store.clone());
        let output = source.frame(br#"{"response":{"responseId":"upstream-id","candidates":[{"content":{"parts":[{"thoughtSignature":"opaque","functionCall":{"id":"call-1","name":"lookup","args":{"q":1}}}]},"finishReason":"STOP"}]}}"#).unwrap();
        assert!(!String::from_utf8(output)
            .unwrap()
            .contains("interaction.completed"));
        assert!(store.lock().unwrap().entries.is_empty());
        source.frame(br#"{"response":{"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3,"totalTokenCount":10,"cachedContentTokenCount":0}}}"#).unwrap();
        let completed = source.finish().unwrap();
        let mut parser = SseParser::default();
        let mut frames = Vec::new();
        parser.push_raw_data(&completed, &mut frames).unwrap();
        let value: Value = serde_json::from_slice(&frames[0]).unwrap();
        assert_eq!(value["interaction"]["status"], "requires_action");
        assert_eq!(value["interaction"]["usage"]["total_tokens"], 10);
        assert_eq!(value["interaction"]["usage"]["cached_tokens"], 0);
        let mut request =
            json!({"input":[{"type":"function_result","call_id":"call-1","result":42}]});
        let saved = prepare(
            &mut request,
            false,
            "scope",
            &mut store.lock().unwrap(),
            Instant::now(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(request["input"][0]["name"], "lookup");
        assert_eq!(saved.contents[1]["parts"][0]["thoughtSignature"], "opaque");
        assert!(source.finish().unwrap().is_empty());
    }

    #[test]
    fn incomplete_and_ambiguous_streams_when_observed_never_publish_continuation() {
        let store = Arc::new(Mutex::new(InteractionsStore::default()));
        let mut source = observer(false, "scope", store.clone());
        source.frame(br#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"call-1","name":"lookup","args":{}}}]}}]}"#).unwrap();
        assert!(source.finish().is_err());
        assert!(store.lock().unwrap().entries.is_empty());
        let mut source = observer(true, "scope", store.clone());
        assert!(source
            .observe_native(&json!({"id":"id","status":"requires_action","steps":[
            {"type":"function_call","id":"duplicate"},{"type":"function_call","id":"duplicate"}]}))
            .is_err());
        assert!(store.lock().unwrap().entries.is_empty());
    }

    #[test]
    fn absent_cache_usage_when_completed_remains_absent() {
        let mut source = observer(
            false,
            "scope",
            Arc::new(Mutex::new(InteractionsStore::default())),
        );
        source.frame(br#"{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}"#).unwrap();
        let mut parser = SseParser::default();
        let mut frames = Vec::new();
        parser
            .push_raw_data(&source.finish().unwrap(), &mut frames)
            .unwrap();
        let value: Value = serde_json::from_slice(&frames[0]).unwrap();
        assert!(value.pointer("/interaction/usage/cached_tokens").is_none());
    }

    #[test]
    fn non_tool_previous_when_completed_restores_history_without_pending_ids() {
        let store = Arc::new(Mutex::new(InteractionsStore::default()));
        let mut first = observer(false, "scope", store.clone());
        first.frame(br#"{"responseId":"first","candidates":[{"content":{"parts":[{"text":"answer"}]},"finishReason":"STOP"}]}"#).unwrap();
        first.finish().unwrap();
        let mut next = json!({"previous_interaction_id":"first","input":"follow-up"});
        let saved = prepare(
            &mut next,
            false,
            "scope",
            &mut store.lock().unwrap(),
            Instant::now(),
        )
        .unwrap()
        .unwrap();
        assert!(saved.calls.is_empty());
        assert_eq!(saved.contents[1]["parts"][0]["text"], "answer");
        assert!(prepare(
            &mut next.clone(),
            false,
            "other-scope",
            &mut store.lock().unwrap(),
            Instant::now()
        )
        .is_err());
        assert!(prepare(
            &mut next.clone(),
            false,
            "scope",
            &mut store.lock().unwrap(),
            Instant::now() + TTL
        )
        .is_err());
    }
}
