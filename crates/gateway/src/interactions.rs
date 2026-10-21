//! Gemini Interactions: native Google pass-through and Antigravity translation.
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
    /// Full Gemini history, including opaque signed model parts.
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
            (native || (!agent && member.kind() == ProviderKind::Antigravity))
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
            protocol: if native { "interactions" } else { "gemini" },
            from_protocol: "interactions",
            headers: &headers,
            original: &original,
        },
    ) {
        return error(StatusCode::BAD_REQUEST, message);
    }
    let wire = if native {
        business
    } else {
        let Some(project) = member.project_id() else {
            return error(
                StatusCode::BAD_REQUEST,
                "Antigravity account missing project_id",
            );
        };
        crate::v1beta::wrap_for_antigravity(&model, &project, &business)
    };
    let activity = member.begin_activity();
    let started = Instant::now();
    let response = match crate::relay::send_interactions(&state, &member, &url, &wire).await {
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
    let mut observer = Observer::new(native, model, namespace, history, store);
    observer.owner = owner;
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
    let body = stream_body(response, observer);
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

struct Observer {
    native: bool,
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
        if !self.parts.is_empty() {
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
