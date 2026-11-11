//! The Gemini-native `/v1beta` surface.
//!
//! Clients that already speak Gemini (Gemini CLI, the Antigravity IDE) post
//! `{"contents":[...]}` with the model in the URL path rather than the body,
//! so this surface wraps the request instead of translating it - the antigravity
//! upstream envelope is `{model, project, request}` and `request` is exactly the
//! Gemini body the client sent.
//!
//! Two shape details were captured from CLIProxyAPI v7.2.140 and are load-bearing:
//! the collection response omits the OpenAI-style `object` field and nests under
//! `models`, and the single-model response drops `supportedGenerationMethods`
//! that the collection includes.

use serde_json::{json, Value};

use crate::models_route::ModelEntry;

#[derive(Debug, PartialEq, Eq)]
pub enum GeminiAction {
    Generate,
    StreamGenerate,
    CountTokens,
    Unknown(String),
}

/// Model ids never contain a colon, so the first one splits id from verb.
pub fn parse_action(action: &str) -> (String, Option<GeminiAction>) {
    let mut trimmed = action.trim_start_matches('/');
    if let Some(stripped) = trimmed.strip_prefix("models/") {
        trimmed = stripped;
    }
    match trimmed.split_once(':') {
        None => (trimmed.to_string(), None),
        Some((model, verb)) => {
            let parsed = match verb {
                "generateContent" => GeminiAction::Generate,
                "streamGenerateContent" => GeminiAction::StreamGenerate,
                "countTokens" => GeminiAction::CountTokens,
                other => GeminiAction::Unknown(other.to_string()),
            };
            (model.to_string(), Some(parsed))
        }
    }
}

fn modalities_for(id: &str) -> (Vec<&'static str>, Vec<&'static str>) {
    let out = if id.contains("image") {
        vec!["text", "image"]
    } else {
        vec!["text"]
    };
    (vec!["text", "image", "audio", "video"], out)
}

fn model_object(entry: &ModelEntry, with_methods: bool) -> Value {
    let (inputs, outputs) = modalities_for(&entry.id);
    let mut obj = json!({
        "name": format!("models/{}", entry.id),
        "displayName": entry.id,
        "description": entry.id,
        "supportedInputModalities": inputs,
        "supportedOutputModalities": outputs,
    });
    if with_methods {
        obj.as_object_mut()
            .expect("model_object builds a map")
            .insert(
                "supportedGenerationMethods".into(),
                json!(["generateContent"]),
            );
    }
    obj
}

pub fn models_payload(entries: &[ModelEntry]) -> Value {
    json!({
        "models": entries
            .iter()
            .map(|e| model_object(e, true))
            .collect::<Vec<_>>(),
    })
}

pub fn single_model_payload(entry: &ModelEntry) -> Value {
    model_object(entry, false)
}

/// Gemini's own 404 shape, which nests `error` with a numeric `code`.
pub fn model_not_found(name: &str) -> Value {
    json!({"error": {
        "code": 404,
        "message": format!("models/{name} is not found or your account does not have access to it."),
        "status": "NOT_FOUND",
    }})
}

pub fn contents_not_specified() -> Value {
    json!({"error": {
        "code": 400,
        "message": "* GenerateContentRequest.contents: contents is not specified\n",
        "status": "INVALID_ARGUMENT",
    }})
}

/// Wrapper `requestType` (proto field 7, TYPE_STRING) sent on the
/// v1internal GenerateContentRequest. Local captures of the client (1.4.12 and
/// 1.5.0; 1.5.1 not captured) show the main agent turn sends `"agent"`, and
/// computeRequestID uses the same field as the requestId label.
pub const ANTIGRAVITY_REQUEST_TYPE: &str = "agent";

/// The client shapes these ids as random v4 UUIDs. They are derived from the
/// session id so one conversation keeps the same cascade and trajectory.
fn conversation_uuid(kind: &str, session_id: &str) -> uuid::Uuid {
    let seed = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        format!("{kind}:{session_id}").as_bytes(),
    );
    uuid::Builder::from_random_bytes(*seed.as_bytes()).into_uuid()
}

/// Per-request label context. The client derives these from the live
/// conversation, so each field is supplied by the caller that owns the state;
/// nothing here is inferred from the model name.
#[derive(Debug, Clone, Default)]
pub struct AntigravityLabels {
    /// `model_enum` from the catalog entry for the exact variant id. `None`
    /// drops the label rather than inventing a placeholder.
    pub model_enum: Option<String>,
    /// Zero-based index of the step this request produces, matching the
    /// `request_id` suffix the client uses (`{trajectory}-{step}`).
    pub step: u64,
    /// Present from the second turn of a cascade onward.
    pub last_execution_id: Option<String>,
    pub used_claude: bool,
    pub used_claude_conservative: bool,
    pub used_non_gemini_model: bool,
}

/// Inner `GenerateContentRequest` fields this build understands, in the
/// declaration order of the upstream proto (`google.cloud.aiplatform.v1`).
/// `labels` (field 10) is synthesized by the wrapper, and `sessionId` is
/// appended after it; both are positioned by the writer below.
const INNER_FIELD_ORDER: &[&str] = &[
    "model",
    "contents",
    "systemInstruction",
    "cachedContent",
    "tools",
    "toolConfig",
    "safetySettings",
    "modelArmorConfig",
    "generationConfig",
];

/// Gemini's `labels` is `map<string, string>`, so every value must be a JSON
/// string. Booleans and integers sent raw are rejected by the upstream proto
/// parser with `Invalid value at 'request.labels[N].value' (TYPE_STRING)`.
fn proto_string<T: ToString>(value: T) -> String {
    value.to_string()
}

fn ordered_labels(
    cascade: &uuid::Uuid,
    trajectory: &uuid::Uuid,
    labels: &AntigravityLabels,
) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    out.insert("cascade_id".into(), json!(cascade.to_string()));
    out.insert("root_cascade_id".into(), json!(cascade.to_string()));
    // The catalog is authoritative. An unknown enum is omitted rather than
    // replaced with a placeholder, because a wrong value is worse than none.
    if let Some(model_enum) = labels.model_enum.as_deref().filter(|v| !v.is_empty()) {
        out.insert("model_enum".into(), json!(model_enum));
    }
    out.insert("used_claude".into(), json!(proto_string(labels.used_claude)));
    out.insert(
        "used_non_gemini_model".into(),
        json!(proto_string(labels.used_non_gemini_model)),
    );
    // The client reports twice the turn index here, not the content count.
    out.insert(
        "last_step_index".into(),
        json!(proto_string(labels.step.saturating_mul(2))),
    );
    if let Some(execution) = labels.last_execution_id.as_deref() {
        out.insert("last_execution_id".into(), json!(execution));
    }
    out.insert(
        "used_claude_conservative".into(),
        json!(proto_string(labels.used_claude_conservative)),
    );
    out.insert("trajectory_id".into(), json!(trajectory.to_string()));
    out.insert(
        "request_id".into(),
        json!(format!("{trajectory}-{}", labels.step)),
    );
    out
}

/// Rebuilds the inner request with the proto's field order while preserving
/// every field the caller sent. Ordering matters because the client emits the
/// protobuf declaration order, and the gateway previously emitted alphabetical
/// order from `serde_json`'s default map.
fn inner_with_labels(
    body: &Value,
    cascade: &uuid::Uuid,
    trajectory: &uuid::Uuid,
    labels: &AntigravityLabels,
    session_id: Option<&str>,
) -> Value {
    let Some(map) = body.as_object() else {
        return body.clone();
    };
    let mut out = serde_json::Map::new();
    for key in INNER_FIELD_ORDER {
        if let Some(value) = map.get(*key) {
            out.insert((*key).to_string(), value.clone());
        }
    }
    out.insert(
        "labels".into(),
        Value::Object(ordered_labels(cascade, trajectory, labels)),
    );
    if let Some(session_id) = session_id {
        out.insert("sessionId".into(), json!(session_id));
    }
    for (key, value) in map {
        if INNER_FIELD_ORDER.contains(&key.as_str()) || key == "labels" || key == "sessionId" {
            continue;
        }
        out.insert(key.clone(), value.clone());
    }
    Value::Object(out)
}

pub fn wrap_for_antigravity(model: &str, project_id: &str, body: &Value) -> Value {
    let labels = AntigravityLabels {
        step: completed_turns(body),
        ..Default::default()
    };
    wrap_for_antigravity_with_labels(model, project_id, body, &labels)
}

/// Number of model turns already in the request. The client numbers the
/// current request from this count: a fresh conversation is step 0, and
/// `last_step_index` reports twice the step.
pub fn completed_turns(body: &Value) -> u64 {
    body.get("contents")
        .and_then(Value::as_array)
        .map(|contents| {
            contents
                .iter()
                .filter(|content| {
                    content
                        .get("role")
                        .and_then(Value::as_str)
                        .is_some_and(|role| role == "model")
                })
                .count() as u64
        })
        .unwrap_or(0)
}

/// Fields of the `v1internal` wrapper in proto declaration order:
/// `project` (1), `requestId` (2), `request` (3), `model` (4), `userAgent` (6),
/// `requestType` (7). `userPromptId` (5) is not derivable from gateway state
/// and is therefore omitted rather than fabricated.
pub fn wrap_for_antigravity_with_labels(
    model: &str,
    project_id: &str,
    body: &Value,
    labels: &AntigravityLabels,
) -> Value {
    let session_id = body
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    let (cascade, trajectory) = match session_id {
        Some(id) => (
            conversation_uuid("cascade", id),
            conversation_uuid("trajectory", id),
        ),
        None => (uuid::Uuid::new_v4(), uuid::Uuid::new_v4()),
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    // computeRequestID: {requestType}/{cascadeId}/{epoch ms}/{trajectoryId}/{step}
    let request_id = format!(
        "{ANTIGRAVITY_REQUEST_TYPE}/{cascade}/{now_ms}/{trajectory}/{step}",
        step = labels.step
    );
    let mut out = serde_json::Map::new();
    out.insert("project".into(), json!(project_id));
    out.insert("requestId".into(), json!(request_id));
    out.insert(
        "request".into(),
        inner_with_labels(body, &cascade, &trajectory, labels, session_id),
    );
    out.insert("model".into(), json!(model));
    // CLIAuthProvider.GetUserAgentName: the wire value is the product name,
    // not the HTTP User-Agent string.
    out.insert("userAgent".into(), json!("antigravity"));
    out.insert("requestType".into(), json!(ANTIGRAVITY_REQUEST_TYPE));
    Value::Object(out)
}

/// Builds the labels for one upstream Antigravity request from the account's
/// live catalog entry for `model`.
///
/// The enum is looked up by the exact variant id because the catalog assigns a
/// distinct enum per thinking level. `uses_claude` describes the conversation
/// content; the vendor flags additionally follow the catalog, so a model the
/// catalog reports as non-Google is labelled as such even before its first
/// Claude turn.
pub fn labels_for(
    member: &crate::account::AccountMember,
    model: &str,
    uses_claude: bool,
) -> AntigravityLabels {
    let entry = member.antigravity_catalog_entry(model);
    let vendor = entry.as_ref().and_then(|entry| {
        entry
            .api_provider
            .as_deref()
            .or(entry.model_provider.as_deref())
    });
    let is_non_gemini = match vendor {
        Some(vendor) => !vendor.eq_ignore_ascii_case("google"),
        None => uses_claude,
    };
    AntigravityLabels {
        model_enum: entry.and_then(|entry| entry.model_enum),
        step: 0,
        last_execution_id: None,
        used_claude: uses_claude,
        used_claude_conservative: uses_claude,
        used_non_gemini_model: is_non_gemini,
    }
}

/// Re-applies labels to an envelope that was wrapped before the account's
/// catalog entry was known.
pub fn apply_catalog_labels(
    mut envelope: Value,
    member: &crate::account::AccountMember,
    model: &str,
    uses_claude: bool,
) -> Value {
    let Some(request) = envelope.get("request").cloned() else {
        return envelope;
    };
    let mut labels = labels_for(member, model, uses_claude);
    labels.step = completed_turns(&request);
    let session_id = request
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let (cascade, trajectory) = match session_id.as_deref() {
        Some(id) => (
            conversation_uuid("cascade", id),
            conversation_uuid("trajectory", id),
        ),
        None => (uuid::Uuid::new_v4(), uuid::Uuid::new_v4()),
    };
    let rebuilt = inner_with_labels(
        &request,
        &cascade,
        &trajectory,
        &labels,
        session_id.as_deref(),
    );
    if let Some(map) = envelope.as_object_mut() {
        map.insert("request".into(), rebuilt);
        if let Some(request_id) = envelope_request_id(&cascade, &trajectory, labels.step) {
            map.insert("requestId".into(), json!(request_id));
        }
    }
    envelope
}

fn envelope_request_id(cascade: &uuid::Uuid, trajectory: &uuid::Uuid, step: u64) -> Option<String> {
    Some(format!(
        "{ANTIGRAVITY_REQUEST_TYPE}/{cascade}/{}/{trajectory}/{step}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> ModelEntry {
        ModelEntry {
            id: id.to_string(),
            owned_by: "google".to_string(),
        }
    }

    #[test]
    fn action_splits_on_the_verb_colon() {
        let (m, a) = parse_action("/gemini-3.7-flash-high:generateContent");
        assert_eq!(m, "gemini-3.7-flash-high");
        assert_eq!(a, Some(GeminiAction::Generate));

        let (m, a) = parse_action("gemini-3-flash:streamGenerateContent");
        assert_eq!(m, "gemini-3-flash");
        assert_eq!(a, Some(GeminiAction::StreamGenerate));

        let (m, a) = parse_action("/gemini-3-flash:countTokens");
        assert_eq!(m, "gemini-3-flash");
        assert_eq!(a, Some(GeminiAction::CountTokens));
    }

    #[test]
    fn a_bare_model_path_has_no_action() {
        let (m, a) = parse_action("/gemini-3.7-flash-high");
        assert_eq!(m, "gemini-3.7-flash-high");
        assert_eq!(a, None);
    }

    #[test]
    fn parse_action_strips_models_prefix() {
        let (m, a) = parse_action("models/gemini-2.0-flash:generateContent");
        assert_eq!(m, "gemini-2.0-flash");
        assert_eq!(a, Some(GeminiAction::Generate));

        let (m, a) = parse_action("/models/gemini-2.0-flash:streamGenerateContent");
        assert_eq!(m, "gemini-2.0-flash");
        assert_eq!(a, Some(GeminiAction::StreamGenerate));

        let (m, a) = parse_action("models/gemini-2.0-flash");
        assert_eq!(m, "gemini-2.0-flash");
        assert_eq!(a, None);

        let (m, a) = parse_action("/models/gemini-2.0-flash");
        assert_eq!(m, "gemini-2.0-flash");
        assert_eq!(a, None);
    }

    #[test]
    fn collection_nests_under_models_and_keeps_methods() {
        let payload = models_payload(&[entry("gemini-3.7-flash-high")]);
        assert!(payload.get("object").is_none());
        let first = &payload["models"][0];
        assert_eq!(first["name"], "models/gemini-3.7-flash-high");
        assert_eq!(first["supportedGenerationMethods"][0], "generateContent");
    }

    #[test]
    fn single_model_drops_generation_methods() {
        let payload = single_model_payload(&entry("gemini-3.7-flash-high"));
        assert_eq!(payload["name"], "models/gemini-3.7-flash-high");
        assert!(payload.get("supportedGenerationMethods").is_none());
    }

    #[test]
    fn image_models_advertise_image_output() {
        let payload = single_model_payload(&entry("gemini-3.1-flash-image"));
        let outputs = payload["supportedOutputModalities"].as_array().unwrap();
        assert!(outputs.iter().any(|v| v == "image"));
    }

    #[test]
    fn envelope_carries_the_antigravity_client_identity_fields() {
        let body = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let wrapped = wrap_for_antigravity("gemini-3-flash", "proj-1", &body);
        assert_eq!(wrapped["model"], "gemini-3-flash");
        assert_eq!(wrapped["project"], "proj-1");
        // The client body passes through untouched; the labels the client adds
        // are now present beside it.
        assert_eq!(wrapped["request"]["contents"], body["contents"]);
        assert_eq!(wrapped["request"]["labels"]["last_step_index"], "0");
        assert_eq!(wrapped["request"]["labels"]["used_claude"], "false");
        assert_eq!(
            wrapped["request"]["labels"]["request_id"],
            format!(
                "{}-0",
                wrapped["request"]["labels"]["trajectory_id"]
                    .as_str()
                    .unwrap()
            )
        );
        assert_eq!(
            wrapped["request"]["labels"]["cascade_id"],
            wrapped["request"]["labels"]["root_cascade_id"]
        );
        // An unknown catalog entry drops the label instead of inventing one.
        assert!(wrapped["request"]["labels"].get("model_enum").is_none());
        assert_eq!(wrapped["userAgent"], "antigravity");
        assert_eq!(wrapped["requestType"], "agent");
        let id = wrapped["requestId"].as_str().unwrap();
        let parts: Vec<&str> = id.split('/').collect();
        assert_eq!(parts.len(), 5, "{id}");
        assert_eq!(parts[0], "agent");
        assert_eq!(
            uuid::Uuid::parse_str(parts[1]).unwrap().get_version_num(),
            4
        );
        assert!(parts[2].parse::<u64>().unwrap() > 1_700_000_000_000, "{id}");
        assert_eq!(
            uuid::Uuid::parse_str(parts[3]).unwrap().get_version_num(),
            4
        );
        assert_eq!(parts[4], "0");
    }

    #[test]
    fn request_id_keeps_conversation_ids_stable_per_session() {
        let turn = |session: &str, contents: Value| {
            let body = json!({"contents": contents, "sessionId": session});
            let wrapped = wrap_for_antigravity("gemini-3-flash", "proj-1", &body);
            wrapped["requestId"]
                .as_str()
                .unwrap()
                .split('/')
                .map(String::from)
                .collect::<Vec<_>>()
        };
        let user = json!({"role": "user", "parts": [{"text": "hi"}]});
        let model = json!({"role": "model", "parts": [{"text": "ok"}]});
        let first = turn("-3750763034362895579", json!([user]));
        let second = turn(
            "-3750763034362895579",
            json!([user.clone(), model, user.clone(), user.clone()]),
        );
        let other = turn("other-session", json!([user]));
        assert_eq!(first[1], second[1]);
        assert_eq!(first[3], second[3]);
        assert_ne!(first[1], first[3]);
        assert_ne!(first[1], other[1]);
        // The step is the number of model turns already in the conversation,
        // so a fresh conversation is 0 and the second user turn is 1.
        assert_eq!(first[4], "0");
        assert_eq!(second[4], "1");
    }

    #[test]
    fn labels_are_emitted_in_the_captured_key_order() {
        let body = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let labels = AntigravityLabels {
            model_enum: Some("MODEL_PLACEHOLDER_M419".into()),
            step: 1,
            last_execution_id: Some("exec-1".into()),
            used_claude: true,
            used_claude_conservative: true,
            used_non_gemini_model: true,
        };
        let wrapped = wrap_for_antigravity_with_labels("m", "p", &body, &labels);
        let request = wrapped["request"].to_string();
        // The captured client order, verbatim from the conversation records.
        // `last_execution_id` is not in the base set; it is emitted beside the
        // other step bookkeeping once a cascade has a prior execution.
        let expected = [
            "cascade_id",
            "root_cascade_id",
            "model_enum",
            "used_claude",
            "used_non_gemini_model",
            "last_step_index",
            "last_execution_id",
            "used_claude_conservative",
            "trajectory_id",
            "request_id",
        ];
        let mut last = 0usize;
        for key in expected {
            let at = request
                .find(&format!("\"{key}\""))
                .unwrap_or_else(|| panic!("missing {key} in {request}"));
            assert!(at > last, "{key} out of order in {request}");
            last = at;
        }
        assert!(request.contains("\"last_step_index\":\"2\""), "{request}");
        assert!(request.contains("\"request_id\":\""), "{request}");
    }

    #[test]
    fn every_label_value_is_a_json_string() {
        // Gemini declares `labels` as map<string, string>. Emitting a boolean or
        // an integer makes the upstream proto parser reject the whole request
        // with `Invalid value at 'request.labels[N].value' (TYPE_STRING)`,
        // which fails every call rather than one field.
        let body = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let labels = AntigravityLabels {
            step: 3,
            used_claude: true,
            used_non_gemini_model: false,
            used_claude_conservative: false,
            model_enum: Some("MODEL_PLACEHOLDER_M36".into()),
            last_execution_id: Some("exec-1".into()),
        };
        let wrapped = wrap_for_antigravity_with_labels("gemini-3-flash", "proj-1", &body, &labels);
        let map = wrapped["request"]["labels"]
            .as_object()
            .expect("labels must be an object");
        for (key, value) in map {
            assert!(
                value.is_string(),
                "label {key} must be a JSON string for map<string,string>, got {value:?}"
            );
        }
        assert_eq!(map["used_claude"], "true");
        assert_eq!(map["used_claude_conservative"], "false");
        assert_eq!(map["last_step_index"], "6");
    }

    #[test]
    fn wrapper_fields_follow_proto_declaration_order() {
        let body = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let wrapped = wrap_for_antigravity_with_labels(
            "gemini-3-flash",
            "proj-1",
            &body,
            &AntigravityLabels::default(),
        );
        let text = wrapped.to_string();
        let order = [
            "project",
            "requestId",
            "request",
            "model",
            "userAgent",
            "requestType",
        ];
        let mut last = 0usize;
        for key in order {
            let at = text
                .find(&format!("\"{key}\""))
                .unwrap_or_else(|| panic!("missing {key} in {text}"));
            assert!(at >= last, "{key} out of order in {text}");
            last = at;
        }
        // `request` carries the rebuilt object, not a JSON-encoded string.
        assert!(wrapped["request"].is_object());
    }

    #[test]
    fn unknown_model_enum_is_dropped_not_invented() {
        let body = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let wrapped = wrap_for_antigravity_with_labels(
            "gemini-3-flash",
            "p",
            &body,
            &AntigravityLabels::default(),
        );
        assert!(wrapped["request"]["labels"].get("model_enum").is_none());
    }
}
