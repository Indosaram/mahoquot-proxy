use serde_json::{json, Map, Value};

use super::events::{CodexEvent, Usage};

pub fn openai_to_kiro(body: &Value) -> Result<Value, String> {
    openai_to_kiro_with_profile(body, None)
}

fn stable_kiro_conversation_id(session_id: Option<&str>) -> String {
    match session_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(sid) => uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_OID,
            format!("kiro-conv:{sid}").as_bytes(),
        )
        .to_string(),
        None => uuid::Uuid::new_v4().to_string(),
    }
}

pub fn openai_to_kiro_with_profile(
    body: &Value,
    profile_arn: Option<&str>,
) -> Result<Value, String> {
    openai_to_kiro_with_profile_and_session(body, profile_arn, None)
}

pub fn openai_to_kiro_with_profile_and_session(
    body: &Value,
    profile_arn: Option<&str>,
    session_id: Option<&str>,
) -> Result<Value, String> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing model".to_string())?;
    let model = model.strip_prefix("kiro/").unwrap_or(model);
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing messages".to_string())?;

    let mut system = Vec::new();
    let mut conversational = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        let content = text_content(message.get("content").unwrap_or(&Value::Null));
        if role == "system" || role == "developer" {
            if !content.is_empty() {
                system.push(content);
            }
            continue;
        }
        conversational.push((role, content, message));
    }
    if conversational.is_empty() {
        return Err("messages contain no user turn".to_string());
    }

    let mut history = Vec::new();
    for (role, content, message) in conversational {
        if role == "assistant" {
            let mut assistant = json!({ "content": content });
            if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                assistant["toolUses"] = Value::Array(
                    calls
                        .iter()
                        .map(|call| {
                            let arguments = call["function"]["arguments"]
                                .as_str()
                                .and_then(|raw| serde_json::from_str(raw).ok())
                                .unwrap_or_else(|| json!({}));
                            json!({
                                "toolUseId": normalize_tool_id(call["id"].as_str().unwrap_or_default()),
                                "name": call["function"]["name"],
                                "input": arguments,
                            })
                        })
                        .collect(),
                );
            }
            if let Some(reasoning) = message
                .get("kiroRedactedReasoning")
                .or_else(|| message.get("kiro_redacted_reasoning"))
                .and_then(Value::as_str)
            {
                assistant["reasoningContent"] = json!({"redactedContent": reasoning});
            }
            history.push(json!({ "assistantResponseMessage": assistant }));
        } else {
            let mut user = json!({"content": content, "modelId": model, "origin": mahoquot_providers::KIRO_CLI_ORIGIN,
                "images": images_content(message.get("content").unwrap_or(&Value::Null)),
                "userInputMessageContext": {"toolResults": []}});
            if role == "tool" {
                let id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "Kiro tool result missing tool_call_id".to_string())?;
                user["content"] = json!("");
                user["userInputMessageContext"]["toolResults"] = json!([{
                    "toolUseId": normalize_tool_id(id),
                    "content": [{"text": if content.trim().is_empty() { "Tool completed without textual output" } else { &content }}],
                    "status": if message["is_error"].as_bool() == Some(true) { "error" } else { "success" }
                }]);
            }
            if let Some(previous) = history
                .last_mut()
                .and_then(|entry: &mut Value| entry.get_mut("userInputMessage"))
            {
                let text = [
                    previous["content"].as_str().unwrap_or_default(),
                    user["content"].as_str().unwrap_or_default(),
                ]
                .into_iter()
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n");
                previous["content"] = json!(text);
                previous["images"]
                    .as_array_mut()
                    .unwrap()
                    .extend(user["images"].as_array().unwrap().iter().cloned());
                previous["userInputMessageContext"]["toolResults"]
                    .as_array_mut()
                    .unwrap()
                    .extend(
                        user["userInputMessageContext"]["toolResults"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .cloned(),
                    );
            } else {
                history.push(json!({"userInputMessage": user}));
            }
        }
    }

    let mut context = Map::new();
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let specifications: Vec<Value> = tools
            .iter()
            .filter_map(|tool| tool.get("function"))
            .map(|function| {
                let mut schema = function
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                sanitize_schema(&mut schema);
                json!({
                    "toolSpecification": {
                        "name": function["name"],
                        "description": function["description"],
                        "inputSchema": { "json": schema },
                    }
                })
            })
            .collect();
        if !specifications.is_empty() {
            context.insert("tools".to_string(), Value::Array(specifications));
        }
    }

    let mut user_input = if history
        .last()
        .is_some_and(|entry| entry.get("userInputMessage").is_some())
    {
        history.pop().unwrap()["userInputMessage"].take()
    } else {
        json!({"content": "", "modelId": model, "origin": mahoquot_providers::KIRO_CLI_ORIGIN, "userInputMessageContext": {}})
    };
    let current = user_input["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if !system.is_empty() {
        user_input["content"] = json!([system.join("\n\n"), current]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"));
    } else if current.is_empty()
        && user_input
            .pointer("/userInputMessageContext/toolResults")
            .and_then(Value::as_array)
            .is_some_and(|results| !results.is_empty())
    {
        user_input["content"] = json!("Tool results are available in the message context");
    }
    user_input["userInputMessageContext"]
        .as_object_mut()
        .unwrap()
        .extend(context);

    let mut payload = json!({
        "conversationState": {
            "chatTriggerType": "MANUAL",
            "conversationId": stable_kiro_conversation_id(session_id),
            "currentMessage": {
                "userInputMessage": user_input
            },
            "history": history,
        }
    });
    if let Some(profile_arn) = profile_arn.filter(|value| !value.is_empty()) {
        payload["profileArn"] = Value::String(profile_arn.to_string());
    }
    Ok(payload)
}

pub fn openai_to_kiro_responses_with_profile(
    body: &Value,
    profile_arn: Option<&str>,
) -> Result<Value, String> {
    openai_to_kiro_responses_with_profile_and_session(body, profile_arn, None)
}

pub fn openai_to_kiro_responses_with_profile_and_session(
    body: &Value,
    profile_arn: Option<&str>,
    session_id: Option<&str>,
) -> Result<Value, String> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing model".to_string())?;
    let model = model.strip_prefix("kiro/").unwrap_or(model);
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing messages".to_string())?;

    let mut system = Vec::new();
    let mut input = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        let content = text_content(message.get("content").unwrap_or(&Value::Null));
        if role == "system" || role == "developer" {
            if !content.is_empty() {
                system.push(content);
            }
            continue;
        }
        if role == "assistant" {
            if let Some(reasoning) = message
                .get("kiroRedactedReasoning")
                .or_else(|| message.get("kiro_redacted_reasoning"))
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
            {
                let encrypted = encode_kiro_redacted_reasoning_envelope(reasoning);
                input.push(json!({
                    "type": "reasoning",
                    "summary": [],
                    "encrypted_content": encrypted,
                }));
            } else if let Some(text) = message
                .get("reasoning_content")
                .or_else(|| message.get("thinking"))
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
            {
                let signature = message
                    .get("reasoning_signature")
                    .or_else(|| message.get("signature"))
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty());
                let encrypted = encode_kiro_text_reasoning_envelope(text, signature);
                input.push(json!({
                    "type": "reasoning",
                    "summary": [],
                    "encrypted_content": encrypted,
                }));
            }
            if !content.is_empty() {
                input.push(json!({
                    "role": "assistant",
                    "content": content,
                }));
            }
            if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let arguments = match &call["function"]["arguments"] {
                        Value::String(raw) => raw.clone(),
                        Value::Null => "{}".to_string(),
                        other => other.to_string(),
                    };
                    input.push(json!({
                        "type": "function_call",
                        "call_id": normalize_tool_id(call["id"].as_str().unwrap_or_default()),
                        "name": call["function"]["name"],
                        "arguments": arguments,
                    }));
                }
            }
        } else if role == "tool" {
            let id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "Kiro tool result missing tool_call_id".to_string())?;
            let output = if content.trim().is_empty() {
                "Tool completed without textual output".to_string()
            } else {
                content
            };
            input.push(json!({
                "type": "function_call_output",
                "call_id": normalize_tool_id(id),
                "output": output,
            }));
        } else {
            let raw_content = message.get("content").unwrap_or(&Value::Null);
            let imgs = images_content(raw_content);
            let docs = documents_content(raw_content);
            if !imgs.is_empty() || !docs.is_empty() {
                let mut parts = Vec::new();
                if !content.is_empty() {
                    parts.push(json!({"type": "input_text", "text": content}));
                }
                for img in imgs {
                    let fmt = img["format"].as_str().unwrap_or("png");
                    let bytes = img["source"]["bytes"].as_str().unwrap_or_default();
                    parts.push(json!({
                        "type": "input_image",
                        "image_url": format!("data:image/{fmt};base64,{bytes}"),
                    }));
                }
                parts.extend(docs);
                if !parts.is_empty() {
                    input.push(json!({"role": "user", "content": parts}));
                }
            } else if !content.is_empty() {
                input.push(json!({"role": "user", "content": content}));
            }
        }
    }
    if input.is_empty() {
        return Err("messages contain no user turn".to_string());
    }

    let mut payload = json!({
        "model": model,
        "input": input,
        "stream": true,
        "origin": mahoquot_providers::KIRO_CLI_ORIGIN,
        "metadata": {
            "kiro:agent_mode": "vibe",
            "kiro:conversation_id": stable_kiro_conversation_id(session_id),
        },
    });
    if !system.is_empty() {
        payload["instructions"] = Value::String(system.join("

"));
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let converted: Vec<Value> = tools
            .iter()
            .filter_map(|tool| tool.get("function"))
            .filter_map(|function| {
                let name = function.get("name")?.as_str()?;
                if name.is_empty() {
                    return None;
                }
                let mut schema = function
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                sanitize_schema(&mut schema);
                Some(json!({
                    "type": "function",
                    "name": name,
                    "description": function.get("description").and_then(Value::as_str).unwrap_or(""),
                    "parameters": schema,
                }))
            })
            .collect();
        if !converted.is_empty() {
            payload["tools"] = Value::Array(converted);
        }
    }
    let mut reasoning_obj = Map::new();
    if let Some(kind) = body
        .pointer("/reasoning/type")
        .or_else(|| body.pointer("/thinking/type"))
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        reasoning_obj.insert("type".to_string(), Value::String(kind.to_string()));
    }
    if let Some(effort) = body
        .get("reasoning_effort")
        .or_else(|| body.pointer("/reasoning/effort"))
        .or_else(|| body.pointer("/output_config/effort"))
        .and_then(Value::as_str)
        .filter(|e| !e.is_empty())
    {
        reasoning_obj.insert("effort".to_string(), Value::String(effort.to_string()));
    }
    if let Some(summary) = body
        .pointer("/reasoning/summary")
        .or_else(|| body.pointer("/thinking/display"))
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        reasoning_obj.insert("summary".to_string(), Value::String(summary.to_string()));
    }
    if !reasoning_obj.is_empty() {
        payload["reasoning"] = Value::Object(reasoning_obj);
    }
    if let Some(profile_arn) = profile_arn.filter(|value| !value.is_empty()) {
        payload["profileArn"] = Value::String(profile_arn.to_string());
    }
    Ok(payload)
}

fn normalize_tool_id(id: &str) -> String {
    id.replace('|', "_")
}

fn text_content(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn documents_content(value: &Value) -> Vec<Value> {
    let Value::Array(blocks) = value else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for block in blocks {
        let Some(kind) = block.get("type").and_then(Value::as_str) else {
            continue;
        };
        match kind {
            "input_file" => {
                if let Some(file_data) = block.get("file_data").and_then(Value::as_str) {
                    if !file_data.is_empty() {
                        let filename = block
                            .get("filename")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .unwrap_or("document");
                        out.push(json!({
                            "type": "input_file",
                            "filename": filename,
                            "file_data": file_data,
                        }));
                    }
                }
            }
            "file" => {
                let file = block.get("file").unwrap_or(block);
                if let Some(file_data) = file.get("file_data").and_then(Value::as_str) {
                    if !file_data.is_empty() {
                        let filename = file
                            .get("filename")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .unwrap_or("document");
                        out.push(json!({
                            "type": "input_file",
                            "filename": filename,
                            "file_data": file_data,
                        }));
                    }
                }
            }
            "document" => {
                if let Some(source) = block.get("source") {
                    let media = source
                        .get("media_type")
                        .and_then(Value::as_str)
                        .unwrap_or("application/pdf");
                    let filename = block
                        .get("title")
                        .or_else(|| block.get("filename"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .unwrap_or("document");
                    let data_uri = match source.get("type").and_then(Value::as_str) {
                        Some("base64") => source
                            .get("data")
                            .and_then(Value::as_str)
                            .map(|b64| format!("data:{media};base64,{b64}")),
                        Some("text") => source.get("data").and_then(Value::as_str).map(|txt| {
                            use base64::Engine as _;
                            let b64 = base64::engine::general_purpose::STANDARD.encode(txt.as_bytes());
                            format!("data:{media};base64,{b64}")
                        }),
                        _ => None,
                    };
                    if let Some(file_data) = data_uri {
                        out.push(json!({
                            "type": "input_file",
                            "filename": filename,
                            "file_data": file_data,
                        }));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

pub fn infer_kiro_responses_stream_error_status(err_type: Option<&str>, code: Option<&str>) -> u16 {
    let lower_type = err_type.unwrap_or_default().to_ascii_lowercase();
    let raw_code = code.unwrap_or_default();
    let norm_code = raw_code
        .strip_prefix("kiro:")
        .or_else(|| raw_code.strip_prefix("KIRO:"))
        .unwrap_or(raw_code)
        .to_ascii_uppercase();
    if raw_code == "context_length_exceeded" || norm_code == "CONTENT_LENGTH_EXCEEDS_THRESHOLD" {
        return 413;
    }
    if raw_code == "model_not_found" || lower_type == "not_found" {
        return 404;
    }
    if lower_type == "too_many_requests"
        || raw_code == "rate_limit_exceeded"
        || matches!(
            norm_code.as_str(),
            "USER_REQUEST_RATE_EXCEEDED"
                | "SERVICE_REQUEST_RATE_EXCEEDED"
                | "CREDIT_CONSUMPTION_RATE_EXCEEDED"
                | "INSUFFICIENT_MODEL_CAPACITY"
                | "HOURLY_REQUEST_COUNT"
                | "DAILY_REQUEST_COUNT"
                | "WEEKLY_REQUEST_COUNT"
        )
    {
        return 429;
    }
    if lower_type == "insufficient_quota"
        || raw_code == "credit_balance_exhausted"
        || matches!(
            norm_code.as_str(),
            "MONTHLY_REQUEST_COUNT"
                | "OVERAGE_REQUEST_LIMIT_EXCEEDED"
                | "CONVERSATION_LIMIT_EXCEEDED"
                | "SEAT_UNAVAILABLE"
                | "USAGE_LIMIT_REACHED"
        )
    {
        return 402;
    }
    if raw_code == "access_denied"
        || raw_code == "kiro:model_requires_user_opt_in"
        || matches!(
            norm_code.as_str(),
            "TEMPORARILY_SUSPENDED" | "FEATURE_NOT_SUPPORTED" | "NEW_USER_ACCESS_PAUSED"
        )
    {
        return 403;
    }
    if lower_type == "invalid_request"
        || raw_code.starts_with("invalid_")
        || raw_code.starts_with("unsupported_")
        || raw_code.starts_with("missing_")
    {
        return 400;
    }
    if raw_code == "server_is_overloaded" || norm_code == "MODEL_TEMPORARILY_UNAVAILABLE" {
        return 503;
    }
    500
}

fn images_content(value: &Value) -> Vec<Value> {
    let Value::Array(blocks) = value else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|block| {
            let url = block
                .get("image_url")
                .and_then(|image| image.get("url"))
                .or_else(|| block.get("imageUrl"))
                .and_then(Value::as_str)?;
            let encoded = url.strip_prefix("data:image/")?;
            let (format, bytes) = encoded.split_once(";base64,")?;
            let format = if format == "jpg" { "jpeg" } else { format };
            matches!(format, "jpeg" | "png" | "gif" | "webp")
                .then(|| json!({"format": format, "source": {"bytes": bytes}}))
        })
        .collect()
}

fn sanitize_schema(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("additionalProperties");
            if map
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
            {
                map.remove("required");
            }
            for child in map.values_mut() {
                sanitize_schema(child);
            }
        }
        Value::Array(values) => values.iter_mut().for_each(sanitize_schema),
        _ => {}
    }
}

/// Upper bound on a single declared AWS event-stream frame. The four-byte
/// length prefix is authoritative before any payload arrives, so an oversized
/// declaration is refused as soon as it is read instead of buffering the
/// payload it promises.
pub const MAX_KIRO_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Upper bound on the JSON retained between chunks. A payload that never
/// closes its object would otherwise keep growing for the lifetime of the
/// request.
pub const MAX_KIRO_JSON_BUFFER_BYTES: usize = 8 * 1024 * 1024;

/// Context window Kiro charges `contextUsagePercentage` against, in tokens.
///
/// The models advertise 1,000,000, but the runtime charges 1.50x per token
/// against that number while every other model charges 1.00x. kiro-lb inverted
/// the measured slopes and landed on 665,853 / 667,824 / 668,300 / 667,364
/// across four models - all within 0.3% of 666,667 - so the percentage is of
/// 666,667, not of the advertised window. Using 1,000,000 here would inflate
/// every derived count by 1.50x.
///
/// Only `claude-opus-5.5` is enabled for Kiro, so one constant covers the
/// closed catalog. Replace with a per-model table before enabling more.
pub const KIRO_CONTEXT_WINDOW_TOKENS: u64 = 666_667;

/// Chars per token used when splitting the context total into input/output.
/// Upstream never reports the split, so the output side is approximated from
/// the text it streamed and the input side is the remainder.
const KIRO_CHARS_PER_TOKEN: u64 = 4;

#[derive(Default)]
pub struct KiroDecoder {
    wire: Vec<u8>,
    framed: Option<bool>,
    buffer: String,
    /// Bytes of a character split across chunk boundaries, awaiting completion.
    pending: Vec<u8>,
    completed: bool,
    /// Why upstream said the turn ended. Kept separate from `completed`
    /// because a Kiro turn is not finished when this arrives: the context
    /// percentage ships in its own later frame.
    stop_reason: Option<String>,
    usage: Option<Usage>,
    /// Authoritative context occupancy reported by upstream. Kiro never sends
    /// `tokenUsage`, but it does send this, and it is the only real signal of
    /// how much context a turn occupied.
    context_usage_percentage: Option<f64>,
    /// Characters of assistant text streamed this turn, used to split the
    /// context total into input and output.
    output_chars: u64,
    current_tool: Option<(String, String, u64)>,
}

impl KiroDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds the usage to report when upstream sent `contextUsagePercentage`
    /// but no `tokenUsage`.
    ///
    /// The total is upstream's own context occupancy, so it is measured, not
    /// invented. The input/output split is not reported by upstream: the output
    /// side is approximated from the streamed text and the input side is what
    /// remains, which mirrors how kiro-lb derives `prompt_tokens`.
    fn derived_usage(&self) -> Option<Usage> {
        let percentage = self.context_usage_percentage?;
        if !(percentage.is_finite() && percentage > 0.0) {
            return None;
        }
        let capped = percentage.min(100.0);
        let total_tokens = ((capped / 100.0) * KIRO_CONTEXT_WINDOW_TOKENS as f64).ceil() as u64;
        if total_tokens == 0 {
            return None;
        }
        let completion_tokens = (self.output_chars / KIRO_CHARS_PER_TOKEN).min(total_tokens);
        Some(Usage {
            prompt_tokens: total_tokens.saturating_sub(completion_tokens),
            completion_tokens,
            total_tokens,
            cached_tokens: 0,
            cached_tokens_known: false,
            reasoning_tokens: 0,
            cache_write_tokens: 0,
            cache_write_tokens_known: false,
        })
    }

    /// Usage to report: whatever upstream sent verbatim, else the context
    /// derivation, else nothing.
    fn take_usage(&mut self) -> Option<Usage> {
        self.usage.take().or_else(|| self.derived_usage())
    }

    pub fn decode(&mut self, bytes: &[u8], out: &mut Vec<CodexEvent>) {
        if self.completed {
            return;
        }
        self.wire.extend_from_slice(bytes);
        if self.framed.is_none() {
            let first = self
                .wire
                .iter()
                .position(|byte| !byte.is_ascii_whitespace());
            let Some(index) = first else {
                // Whitespace alone never selects a framing, so cap what is
                // otherwise an unbounded run of prefix padding.
                if self.wire.len() > MAX_KIRO_FRAME_BYTES {
                    self.fail("Kiro stream exceeds the frame limit".to_string(), out);
                }
                return;
            };
            self.framed = Some(self.wire[index] != b'{');
        }
        if self.framed == Some(false) {
            let bytes = std::mem::take(&mut self.wire);
            self.decode_json(&bytes, out);
            return;
        }
        while self.wire.len() >= 12 && !self.completed {
            let total = u32::from_be_bytes(self.wire[..4].try_into().unwrap()) as usize;
            let headers = u32::from_be_bytes(self.wire[4..8].try_into().unwrap()) as usize;
            if total > MAX_KIRO_FRAME_BYTES {
                self.fail("Kiro frame exceeds the size limit".to_string(), out);
                return;
            }
            if total < 16 || headers > total - 16 {
                self.fail("Invalid Kiro event-stream framing".to_string(), out);
                return;
            }
            if self.wire.len() < total {
                return;
            }
            let frame: Vec<u8> = self.wire.drain(..total).collect();
            let mut cursor = 12;
            let header_end = 12 + headers;
            let mut error_type = None;
            while cursor < header_end {
                let name_len = frame[cursor] as usize;
                cursor += 1;
                if cursor + name_len + 3 > header_end || frame[cursor + name_len] != 7 {
                    self.fail("Invalid Kiro event-stream headers".to_string(), out);
                    return;
                }
                let name = &frame[cursor..cursor + name_len];
                cursor += name_len + 1;
                let len =
                    u16::from_be_bytes(frame[cursor..cursor + 2].try_into().unwrap()) as usize;
                cursor += 2;
                if cursor + len > header_end {
                    self.fail("Invalid Kiro event-stream headers".to_string(), out);
                    return;
                }
                let value = &frame[cursor..cursor + len];
                if name == b":message-type" && (value == b"exception" || value == b"error") {
                    error_type = Some(String::from_utf8_lossy(value).into_owned());
                }
                cursor += len;
            }
            let payload = &frame[header_end..total - 4];
            if let Some(error_type) = error_type {
                let value = serde_json::from_slice(payload).unwrap_or(Value::Null);
                self.fail(kiro_error_message(&value).unwrap_or(error_type), out);
            } else {
                self.decode_json(payload, out);
            }
        }
    }

    fn fail(&mut self, message: String, out: &mut Vec<CodexEvent>) {
        self.completed = true;
        out.push(CodexEvent::Failed { message });
    }

    /// Publishes this turn's single `Completed` event.
    fn complete(&mut self, out: &mut Vec<CodexEvent>) {
        self.completed = true;
        let usage = self.take_usage();
        out.push(CodexEvent::Completed { usage });
    }

    fn decode_json(&mut self, bytes: &[u8], out: &mut Vec<CodexEvent>) {
        // Chunks arrive unaligned to UTF-8 boundaries, so decode from the
        // accumulated bytes: converting each chunk on its own would replace a
        // trailing partial character with U+FFFD before the next chunk can
        // complete it.
        self.pending.extend_from_slice(bytes);
        let decoded = match std::str::from_utf8(&self.pending) {
            Ok(text) => {
                let text = text.to_string();
                self.pending.clear();
                text
            }
            Err(error) => {
                let valid_upto = error.valid_up_to();
                // A genuine invalid sequence (not a split character) would
                // otherwise wedge the buffer forever, so only a trailing
                // incomplete character is held back.
                if error.error_len().is_some() {
                    let text = String::from_utf8_lossy(&self.pending).to_string();
                    self.pending.clear();
                    text
                } else {
                    let text = String::from_utf8_lossy(&self.pending[..valid_upto]).to_string();
                    self.pending.drain(..valid_upto);
                    text
                }
            }
        };
        self.buffer.push_str(&decoded);
        while let Some((start, end)) = next_json_object(&self.buffer) {
            let candidate = self.buffer[start..=end].to_string();
            self.buffer.drain(..=end);
            let Ok(value) = serde_json::from_str::<Value>(&candidate) else {
                self.fail("Invalid Kiro JSON payload".to_string(), out);
                return;
            };
            if let Some(tokens) = value.get("tokenUsage") {
                let count = |key: &str| tokens.get(key).and_then(Value::as_u64).unwrap_or(0);
                let prompt = count("uncachedInputTokens")
                    + count("cacheReadInputTokens")
                    + count("cacheWriteInputTokens");
                let output = count("outputTokens");
                self.usage = Some(Usage {
                    prompt_tokens: prompt,
                    completion_tokens: output,
                    total_tokens: tokens
                        .get("totalTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(prompt + output),
                    cached_tokens: count("cacheReadInputTokens"),
                    cached_tokens_known: tokens
                        .get("cacheReadInputTokens")
                        .and_then(Value::as_u64)
                        .is_some(),
                    cache_write_tokens: count("cacheWriteInputTokens"),
                    cache_write_tokens_known: tokens
                        .get("cacheWriteInputTokens")
                        .and_then(Value::as_u64)
                        .is_some(),
                    reasoning_tokens: 0,
                });
            }
            if let Some(percentage) = value
                .get("contextUsagePercentage")
                .and_then(Value::as_f64)
                .filter(|value| value.is_finite() && *value > 0.0)
            {
                self.context_usage_percentage = Some(percentage);
            }
            if let Some(redacted) = value.get("redactedContent").and_then(Value::as_str) {
                out.push(CodexEvent::KiroRedactedReasoning(redacted.to_string()));
            }
            if value.get("__type").is_some() || value.get("error").is_some() {
                self.fail(
                    kiro_error_message(&value).unwrap_or_else(|| "Kiro upstream error".to_string()),
                    out,
                );
                return;
            } else if let Some(content) = value.get("content").and_then(Value::as_str) {
                self.output_chars = self
                    .output_chars
                    .saturating_add(content.chars().count() as u64);
                out.push(CodexEvent::TextDelta(content.to_string()));
            } else if let (Some(name), Some(id)) = (
                value.get("name").and_then(Value::as_str),
                value.get("toolUseId").and_then(Value::as_str),
            ) {
                let index = self.current_tool.as_ref().map_or(0, |tool| tool.2 + 1);
                self.current_tool = Some((id.to_string(), name.to_string(), index));
                out.push(CodexEvent::ToolCallBegin {
                    output_index: index,
                    call_id: id.to_string(),
                    name: name.to_string(),
                });
                if let Some(input) = value.get("input") {
                    out.push(CodexEvent::ToolArgsDelta {
                        output_index: index,
                        delta: input
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| input.to_string()),
                    });
                }
            } else if let Some(input) = value.get("input").and_then(Value::as_str) {
                if let Some((_, _, index)) = &self.current_tool {
                    out.push(CodexEvent::ToolArgsDelta {
                        output_index: *index,
                        delta: input.to_string(),
                    });
                }
            } else if let Some(text) = value.get("text").and_then(Value::as_str) {
                self.output_chars = self
                    .output_chars
                    .saturating_add(text.chars().count() as u64);
                out.push(CodexEvent::ReasoningDelta(text.to_string()));
            } else if let Some(signature) = value.get("signature").and_then(Value::as_str) {
                out.push(CodexEvent::ReasoningSignature(signature.to_string()));
            }
            if let Some(reason) = value.get("stopReason") {
                self.stop_reason = Some(reason.as_str().unwrap_or_default().to_string());
            }
            // Ending the turn here used to be wrong: live CodeWhisperer sends
            // `contextUsagePercentage` in a separate `contextUsageEvent` frame
            // that arrives after the `metadataEvent` carrying `stopReason`, so
            // terminating on `stopReason` discarded the only usage signal a
            // Kiro turn ever carries. Only finish once a usage signal is in
            // hand; otherwise keep draining, and let `finish` close the turn.
            if self.stop_reason.is_some()
                && (self.usage.is_some() || self.context_usage_percentage.is_some())
            {
                self.complete(out);
                return;
            }
        }
        // Checked after draining every complete object so a large but valid
        // payload is never mistaken for stuck accumulation; only the
        // unterminated remainder is bounded.
        if self.buffer.len() > MAX_KIRO_JSON_BUFFER_BYTES {
            self.fail("Kiro JSON buffer exceeds the size limit".to_string(), out);
        }
    }

    pub fn finish(&mut self, out: &mut Vec<CodexEvent>) {
        if !self.completed {
            if !self.wire.is_empty() || !self.pending.is_empty() || !self.buffer.trim().is_empty() {
                self.fail("Truncated Kiro stream".to_string(), out);
                return;
            }
            self.complete(out);
        }
    }
}

#[derive(Default)]
pub struct KiroResponsesDecoder {
    completed: bool,
    next_tool_index: u64,
    tool_indices: std::collections::HashMap<String, u64>,
    argued_tools: std::collections::HashSet<u64>,
    last_tool_index: Option<u64>,
    refusal_streamed: bool,
    output_chars: u64,
}

impl KiroResponsesDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    fn derive_usage(&self, response: Option<&Value>) -> Option<Usage> {
        let usage = response?.get("usage")?;
        let input_tokens = usage.get("input_tokens").and_then(Value::as_u64);
        let output_tokens = usage.get("output_tokens").and_then(Value::as_u64);
        let total_tokens = usage.get("total_tokens").and_then(Value::as_u64);
        if input_tokens.unwrap_or(0) > 0 || output_tokens.unwrap_or(0) > 0 {
            let prompt = input_tokens.unwrap_or(0);
            let completion = output_tokens.unwrap_or(0);
            let cached = usage
                .pointer("/input_tokens_details/cached_tokens")
                .and_then(Value::as_u64);
            let reasoning = usage
                .pointer("/output_tokens_details/reasoning_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            return Some(Usage {
                prompt_tokens: prompt,
                completion_tokens: completion,
                total_tokens: total_tokens.unwrap_or_else(|| prompt.saturating_add(completion)),
                cached_tokens: cached.unwrap_or(0),
                cached_tokens_known: cached.is_some(),
                reasoning_tokens: reasoning,
                cache_write_tokens: 0,
                cache_write_tokens_known: false,
            });
        }
        let percentage = usage
            .get("kiro:context_usage_percentage")
            .and_then(Value::as_f64)
            .filter(|pct| pct.is_finite() && *pct > 0.0)?;
        let total = ((percentage / 100.0) * KIRO_CONTEXT_WINDOW_TOKENS as f64).round() as u64;
        if total == 0 {
            return None;
        }
        let completion = if self.output_chars == 0 {
            0
        } else {
            self.output_chars.div_ceil(KIRO_CHARS_PER_TOKEN).min(total)
        };
        Some(Usage {
            prompt_tokens: total.saturating_sub(completion),
            completion_tokens: completion,
            total_tokens: total,
            cached_tokens: 0,
            cached_tokens_known: false,
            reasoning_tokens: 0,
            cache_write_tokens: 0,
            cache_write_tokens_known: false,
        })
    }

    pub fn decode_frame(&mut self, frame: &[u8], out: &mut Vec<CodexEvent>) {
        if self.completed {
            return;
        }
        let Ok(raw) = std::str::from_utf8(frame) else {
            return;
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            return;
        };
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or_default();
        match event_type {
            "kiro:keep_alive" => {}
            "response.created" => {
                if let Some(id) = value.pointer("/response/id").and_then(Value::as_str) {
                    out.push(CodexEvent::Created {
                        response_id: id.to_string(),
                    });
                }
            }
            "response.output_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    if !delta.is_empty() {
                        self.output_chars = self
                            .output_chars
                            .saturating_add(delta.chars().count() as u64);
                        out.push(CodexEvent::TextDelta(delta.to_string()));
                    }
                }
            }
            "response.reasoning_text.delta" | "response.reasoning.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    if !delta.is_empty() {
                        self.output_chars = self
                            .output_chars
                            .saturating_add(delta.chars().count() as u64);
                        out.push(CodexEvent::ReasoningDelta(delta.to_string()));
                    }
                }
            }
            "response.refusal.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    if !delta.is_empty() {
                        self.refusal_streamed = true;
                        self.output_chars = self
                            .output_chars
                            .saturating_add(delta.chars().count() as u64);
                        out.push(CodexEvent::TextDelta(delta.to_string()));
                    }
                }
            }
            "response.refusal.done" => {
                if !self.refusal_streamed {
                    if let Some(refusal) = value.get("refusal").and_then(Value::as_str) {
                        if !refusal.is_empty() {
                            self.output_chars = self
                                .output_chars
                                .saturating_add(refusal.chars().count() as u64);
                            out.push(CodexEvent::TextDelta(refusal.to_string()));
                        }
                    }
                }
            }
            "response.output_item.added" => {
                let Some(item) = value.get("item") else {
                    return;
                };
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let call_id = item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let output_index = self.next_tool_index;
                    self.next_tool_index += 1;
                    if let Some(item_id) = item.get("id").and_then(Value::as_str) {
                        self.tool_indices.insert(item_id.to_string(), output_index);
                    }
                    if !call_id.is_empty() {
                        self.tool_indices.insert(call_id.clone(), output_index);
                    }
                    self.last_tool_index = Some(output_index);
                    out.push(CodexEvent::ToolCallBegin {
                        output_index,
                        call_id,
                        name,
                    });
                    if let Some(args) = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                    {
                        self.argued_tools.insert(output_index);
                        out.push(CodexEvent::ToolArgsDelta {
                            output_index,
                            delta: args.to_string(),
                        });
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let output_index = value
                    .get("item_id")
                    .or_else(|| value.get("call_id"))
                    .and_then(Value::as_str)
                    .and_then(|id| self.tool_indices.get(id).copied())
                    .or(self.last_tool_index);
                if let (Some(output_index), Some(delta)) = (
                    output_index,
                    value.get("delta").and_then(Value::as_str),
                ) {
                    if !delta.is_empty() {
                        self.argued_tools.insert(output_index);
                        out.push(CodexEvent::ToolArgsDelta {
                            output_index,
                            delta: delta.to_string(),
                        });
                    }
                }
            }
            "response.output_item.done" => {
                let Some(item) = value.get("item") else {
                    return;
                };
                match item.get("type").and_then(Value::as_str) {
                    Some("reasoning") => {
                        if let Some(encrypted) = item
                            .get("encrypted_content")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                        {
                            if let Some(event) = decode_kiro_reasoning_envelope(encrypted) {
                                out.push(event);
                            }
                        }
                    }
                    Some("function_call") => {
                        let output_index = item
                            .get("id")
                            .or_else(|| item.get("call_id"))
                            .and_then(Value::as_str)
                            .and_then(|id| self.tool_indices.get(id).copied())
                            .or(self.last_tool_index);
                        if let Some(output_index) = output_index {
                            if !self.argued_tools.contains(&output_index) {
                                if let Some(args) = item
                                    .get("arguments")
                                    .and_then(Value::as_str)
                                    .filter(|s| !s.is_empty())
                                {
                                    self.argued_tools.insert(output_index);
                                    out.push(CodexEvent::ToolArgsDelta {
                                        output_index,
                                        delta: args.to_string(),
                                    });
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = value.get("response");
                let incomplete = event_type == "response.incomplete"
                    || response
                        .and_then(|r| r.get("status"))
                        .and_then(Value::as_str)
                        == Some("incomplete");
                if incomplete {
                    match response
                        .and_then(|r| r.pointer("/incomplete_details/reason"))
                        .and_then(Value::as_str)
                    {
                        Some("max_output_tokens") => out.push(CodexEvent::OutputLimitReached),
                        Some("content_filter") => out.push(CodexEvent::ContentFilterReached),
                        _ => {}
                    }
                }
                self.completed = true;
                out.push(CodexEvent::Completed {
                    usage: self.derive_usage(response),
                });
            }
            "error" | "response.failed" => {
                let err_obj = if event_type == "error" {
                    value.get("error").unwrap_or(&value)
                } else {
                    value.pointer("/response/error").unwrap_or(&value)
                };
                let message = kiro_error_message(err_obj)
                    .unwrap_or_else(|| "Kiro CreateResponse stream error".to_string());
                self.completed = true;
                out.push(CodexEvent::Failed { message });
            }
            _ => {}
        }
    }

    pub fn finish(&mut self, out: &mut Vec<CodexEvent>) {
        if !self.completed {
            self.completed = true;
            out.push(CodexEvent::Completed { usage: None });
        }
    }
}

fn encode_kiro_redacted_reasoning_envelope(raw: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
        if parsed.get("redactedContent").and_then(Value::as_str).is_some()
            || parsed.pointer("/reasoningText/text").and_then(Value::as_str).is_some()
        {
            return raw.to_string();
        }
    }
    json!({"redactedContent": raw}).to_string()
}

fn encode_kiro_text_reasoning_envelope(text: &str, signature: Option<&str>) -> String {
    let mut inner = Map::new();
    inner.insert("text".to_string(), Value::String(text.to_string()));
    if let Some(sig) = signature {
        inner.insert("signature".to_string(), Value::String(sig.to_string()));
    }
    json!({"reasoningText": Value::Object(inner)}).to_string()
}

fn decode_kiro_reasoning_envelope(raw: &str) -> Option<CodexEvent> {
    let parsed = serde_json::from_str::<Value>(raw).ok()?;
    if let Some(redacted) = parsed
        .get("redactedContent")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        return Some(CodexEvent::KiroRedactedReasoning(redacted.to_string()));
    }
    if parsed
        .pointer("/reasoningText/text")
        .and_then(Value::as_str)
        .is_some()
    {
        if let Some(sig) = parsed
            .pointer("/reasoningText/signature")
            .and_then(Value::as_str)
        {
            return Some(CodexEvent::ReasoningSignature(sig.to_string()));
        }
    }
    None
}

fn kiro_error_message(value: &Value) -> Option<String> {
    [
        value.get("message"),
        value.get("Message"),
        value.pointer("/error/message"),
        value.get("__type"),
        value.get("error"),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_str)
    .map(str::to_string)
}

fn next_json_object(input: &str) -> Option<(usize, usize)> {
    let start = input.find('{')?;
    let mut depth = 0u32;
    let mut string = false;
    let mut escaped = false;
    for (offset, ch) in input[start..].char_indices() {
        if string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                string = false;
            }
            continue;
        }
        match ch {
            '"' => string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((start, start + offset));
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kiro_prior_tool_result_survives_new_user() {
        let body = json!({"model":"kiro/claude-opus-5.5", "messages":[
            {"role":"user","content":"look"},
            {"role":"assistant","content":"", "tool_calls":[{"id":"a|b","function":{"name":"read","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"a|b","is_error":true,"content":[{"type":"text","text":"done"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AQID"}}]},
            {"role":"user","content":"next"}
        ]});
        let wire = openai_to_kiro(&body).unwrap();
        let state = &wire["conversationState"];
        assert_eq!(state["history"].as_array().unwrap().len(), 2);
        assert_eq!(
            state["history"][1]["assistantResponseMessage"]["toolUses"][0]["toolUseId"],
            "a_b"
        );
        let user = &state["currentMessage"]["userInputMessage"];
        assert_eq!(user["content"], "next");
        let results = user["userInputMessageContext"]["toolResults"]
            .as_array()
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["toolUseId"], "a_b");
        assert_eq!(results[0]["status"], "error");
        assert_eq!(user["images"][0]["source"]["bytes"], "AQID");
    }

    #[test]
    fn kiro_redacted_reasoning_roundtrips() {
        for with_tool in [false, true] {
            let mut decoder = KiroDecoder::new();
            let mut events = Vec::new();
            decoder.decode(
                &aws_frame("event", br#"{"redactedContent":"opaque-1"}"#),
                &mut events,
            );
            if with_tool {
                decoder.decode(
                    &aws_frame(
                        "event",
                        br#"{"name":"lookup","toolUseId":"call_1","input":"{}","stop":true}"#,
                    ),
                    &mut events,
                );
            }
            decoder.decode(
                &aws_frame("event", br#"{"stopReason":"end_turn"}"#),
                &mut events,
            );
            decoder.finish(&mut events);
            assert!(events.iter().any(|event| matches!(
                event, CodexEvent::KiroRedactedReasoning(blob) if blob == "opaque-1"
            )));
            let mut renderer = super::super::responses::ResponsesStreamRenderer::new(
                "kiro/claude-opus-5.5".into(),
                0,
            );
            let frames: Vec<_> = events
                .into_iter()
                .flat_map(|event| renderer.render(event))
                .collect();
            let completed = frames
                .iter()
                .find_map(|frame| {
                    let text = std::str::from_utf8(frame).unwrap();
                    text.lines().find_map(|line| {
                        let value: Value =
                            serde_json::from_str(line.strip_prefix("data: ")?).ok()?;
                        (value["type"] == "response.completed").then_some(value)
                    })
                })
                .expect("completed Responses SSE event");
            let mut input = vec![json!({"role":"user","content":"hi"})];
            input.extend(
                completed["response"]["output"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .cloned(),
            );
            if with_tool {
                input.push(
                    json!({"type":"function_call_output","call_id":"call_1","output":"found"}),
                );
            } else {
                input.push(json!({"role":"user","content":"next"}));
            }
            let chat = super::super::responses::responses_to_openai(
                &json!({"model":"kiro/claude-opus-5.5","input":input}),
            )
            .unwrap();
            let replay = openai_to_kiro(&chat).unwrap();
            let assistant = &replay["conversationState"]["history"][1]["assistantResponseMessage"];
            assert_eq!(assistant["reasoningContent"]["redactedContent"], "opaque-1");
            if with_tool {
                assert_eq!(assistant["toolUses"][0]["toolUseId"], "call_1");
                assert_eq!(assistant["toolUses"][0]["name"], "lookup");
                assert_eq!(assistant["toolUses"][0]["input"], json!({}));
                assert_eq!(
                    replay["conversationState"]["currentMessage"]["userInputMessage"]
                        ["userInputMessageContext"]["toolResults"][0]["toolUseId"],
                    "call_1"
                );
            }
        }
    }

    #[test]
    fn kiro_metadata_usage_survives_stop_and_eof() {
        for stop in [false, true] {
            let mut metadata = json!({"tokenUsage":{"uncachedInputTokens":10,"cacheReadInputTokens":3,"cacheWriteInputTokens":2,"outputTokens":4,"totalTokens":19}});
            if stop {
                metadata["stopReason"] = json!("end_turn");
            }
            let mut decoder = KiroDecoder::new();
            let mut events = Vec::new();
            decoder.decode(
                &aws_frame("event", &serde_json::to_vec(&metadata).unwrap()),
                &mut events,
            );
            decoder.finish(&mut events);
            let CodexEvent::Completed { usage: Some(usage) } = &events[0] else {
                panic!("missing usage: {events:?}")
            };
            assert_eq!(
                (
                    usage.prompt_tokens,
                    usage.completion_tokens,
                    usage.total_tokens,
                    usage.cached_tokens,
                    usage.cache_write_tokens
                ),
                (15, 4, 19, 3, 2)
            );
            assert!(usage.cached_tokens_known && usage.cache_write_tokens_known);
        }
        let mut decoder = KiroDecoder::new();
        let mut events = Vec::new();
        decoder.decode(br#"{"tokenUsage":{"cacheReadInputTokens":0,"cacheWriteInputTokens":0},"stopReason":"end_turn"}"#, &mut events);
        assert!(
            matches!(&events[0], CodexEvent::Completed { usage: Some(usage) } if usage.cached_tokens_known && usage.cache_write_tokens_known)
        );
    }

    #[test]
    fn kiro_context_usage_fills_tokens_when_upstream_sends_no_token_usage() {
        // The shape live CodeWhisperer sends: contextUsagePercentage, no
        // tokenUsage. Without this path the turn reports zero tokens.
        let mut decoder = KiroDecoder::new();
        let mut events = Vec::new();
        decoder.decode(
            &aws_frame("event", br#"{"content":"abcdefgh"}"#),
            &mut events,
        );
        decoder.decode(
            &aws_frame(
                "event",
                br#"{"contextUsagePercentage":42.0,"stopReason":"end_turn"}"#,
            ),
            &mut events,
        );
        let CodexEvent::Completed { usage: Some(usage) } = &events[1] else {
            panic!("missing usage: {events:?}")
        };
        // 42% of the 666,667 window the runtime actually charges against.
        assert_eq!(usage.total_tokens, 280_001);
        // 8 streamed chars at 4 chars/token.
        assert_eq!(usage.completion_tokens, 2);
        assert_eq!(usage.prompt_tokens, 279_999);
        assert_eq!(
            usage.prompt_tokens + usage.completion_tokens,
            usage.total_tokens
        );
        // Upstream reported no cache detail, so none is invented.
        assert!(!usage.cached_tokens_known && !usage.cache_write_tokens_known);
        assert_eq!((usage.cached_tokens, usage.cache_write_tokens), (0, 0));
    }

    #[test]
    fn kiro_context_usage_is_capped_at_a_full_window() {
        let mut decoder = KiroDecoder::new();
        let mut events = Vec::new();
        decoder.decode(
            &aws_frame(
                "event",
                br#"{"contextUsagePercentage":180.0,"stopReason":"end_turn"}"#,
            ),
            &mut events,
        );
        let CodexEvent::Completed { usage: Some(usage) } = &events[0] else {
            panic!("missing usage: {events:?}")
        };
        assert_eq!(usage.total_tokens, KIRO_CONTEXT_WINDOW_TOKENS);
    }

    #[test]
    fn kiro_token_usage_wins_over_context_usage() {
        // Both signals present: upstream's own tokenUsage is exact, so the
        // context derivation must not overwrite it.
        let mut decoder = KiroDecoder::new();
        let mut events = Vec::new();
        decoder.decode(
            &aws_frame(
                "event",
                br#"{"tokenUsage":{"uncachedInputTokens":10,"outputTokens":4,"totalTokens":14},"contextUsagePercentage":42.0,"stopReason":"end_turn"}"#,
            ),
            &mut events,
        );
        let CodexEvent::Completed { usage: Some(usage) } = &events[0] else {
            panic!("missing usage: {events:?}")
        };
        assert_eq!(
            (
                usage.prompt_tokens,
                usage.completion_tokens,
                usage.total_tokens
            ),
            (10, 4, 14)
        );
    }

    #[test]
    fn kiro_without_any_usage_signal_reports_none() {
        let mut decoder = KiroDecoder::new();
        let mut events = Vec::new();
        decoder.decode(
            &aws_frame("event", br#"{"content":"hello","stopReason":"end_turn"}"#),
            &mut events,
        );
        // A turn is not closed on `stopReason` alone: the usage frame follows
        // it, so only EOF proves no usage signal is coming.
        decoder.finish(&mut events);
        assert!(
            matches!(events.last(), Some(CodexEvent::Completed { usage: None })),
            "a turn with no usage signal must not fabricate one: {events:?}"
        );
    }

    #[test]
    fn kiro_context_usage_after_stop_reason_still_records_usage() {
        // Production order, captured from live CodeWhisperer: the metadata
        // frame carries only `stopReason`, and the percentage arrives in its
        // own `contextUsageEvent` frame afterwards. Terminating on
        // `stopReason` discarded it, which is why every kiro row recorded zero
        // tokens.
        let mut decoder = KiroDecoder::new();
        let mut events = Vec::new();
        decoder.decode(
            &aws_frame("event", br#"{"content":"abcdefgh"}"#),
            &mut events,
        );
        decoder.decode(
            &aws_frame("event", br#"{"stopReason":"end_turn"}"#),
            &mut events,
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, CodexEvent::Completed { .. })),
            "stopReason must not close the turn before the usage frame arrives: {events:?}"
        );
        decoder.decode(
            &aws_frame("event", br#"{"contextUsagePercentage":42.0}"#),
            &mut events,
        );
        let Some(CodexEvent::Completed { usage: Some(usage) }) = events.last() else {
            panic!("the trailing usage frame must close the turn: {events:?}")
        };
        assert_eq!(usage.total_tokens, 280_001);
        assert_eq!(usage.completion_tokens, 2);
        decoder.finish(&mut events);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, CodexEvent::Completed { .. }))
                .count(),
            1,
            "exactly one Completed event: {events:?}"
        );
    }

    #[test]
    fn kiro_zero_context_usage_is_ignored() {
        // 0% means "not reported" in this field, not "an empty context".
        let mut decoder = KiroDecoder::new();
        let mut events = Vec::new();
        decoder.decode(
            &aws_frame(
                "event",
                br#"{"contextUsagePercentage":0.0,"stopReason":"end_turn"}"#,
            ),
            &mut events,
        );
        decoder.finish(&mut events);
        assert!(matches!(
            events.last(),
            Some(CodexEvent::Completed { usage: None })
        ));
    }

    fn aws_frame(message_type: &str, payload: &[u8]) -> Vec<u8> {
        fn crc(bytes: &[u8]) -> u32 {
            let mut crc = !0u32;
            for byte in bytes {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (0xedb88320 & (0u32.wrapping_sub(crc & 1)));
                }
            }
            !crc
        }
        let mut headers = vec![13];
        headers.extend_from_slice(b":message-type");
        headers.push(7);
        headers.extend_from_slice(&(message_type.len() as u16).to_be_bytes());
        headers.extend_from_slice(message_type.as_bytes());
        let mut frame = ((16 + headers.len() + payload.len()) as u32)
            .to_be_bytes()
            .to_vec();
        frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        frame.extend_from_slice(&crc(&frame).to_be_bytes());
        frame.extend(headers);
        frame.extend_from_slice(payload);
        frame.extend_from_slice(&crc(&frame).to_be_bytes());
        frame
    }

    #[test]
    fn kiro_decoder_fails_on_an_oversized_declared_frame() {
        // Only the twelve-byte prelude is delivered: the decoder must refuse
        // the declared length without waiting for (or buffering) the payload
        // it promises.
        let mut prelude = (MAX_KIRO_FRAME_BYTES as u32 + 1).to_be_bytes().to_vec();
        prelude.extend_from_slice(&0u32.to_be_bytes());
        prelude.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(prelude.len(), 12);

        let mut decoder = KiroDecoder::new();
        let mut events = Vec::new();
        decoder.decode(&prelude, &mut events);
        decoder.finish(&mut events);
        assert_failure_terminates(&events);
    }

    #[test]
    fn kiro_decoder_fails_when_the_json_buffer_exceeds_the_cap() {
        let mut payload = br#"{"content":""#.to_vec();
        payload.extend(std::iter::repeat_n(b'a', MAX_KIRO_JSON_BUFFER_BYTES + 1));
        assert!(payload.len() > MAX_KIRO_JSON_BUFFER_BYTES);

        let mut decoder = KiroDecoder::new();
        let mut events = Vec::new();
        decoder.decode(&payload, &mut events);
        decoder.finish(&mut events);
        assert_failure_terminates(&events);
    }

    /// A limit breach must surface as an error frame and leave the renderer
    /// terminated, so the stream ends instead of silently truncating (§9).
    fn assert_failure_terminates(events: &[CodexEvent]) {
        assert!(
            matches!(events, [.., CodexEvent::Failed { .. }]),
            "expected a terminal failure, got {events:?}"
        );
        assert!(!events
            .iter()
            .any(|event| matches!(event, CodexEvent::Completed { .. })));
        let mut renderer =
            super::super::render::ChunkRenderer::new("kiro/test".to_string(), 0, false);
        let frames: Vec<_> = events
            .iter()
            .cloned()
            .flat_map(|event| renderer.render(event))
            .collect();
        assert!(!frames.is_empty());
        assert!(renderer.terminated());
        assert!(renderer.close_unterminated().is_empty());
    }

    #[test]
    fn clean_aws_eof_completes_without_stop_reason_at_every_split() {
        let mut wire = aws_frame("event", br#"{"content":"hello"}"#);
        wire.extend(aws_frame(
            "event",
            br#"{"name":"lookup","toolUseId":"clean-eof","input":"{}"}"#,
        ));
        for split in 0..=wire.len() {
            let mut decoder = KiroDecoder::new();
            let mut events = Vec::new();
            decoder.decode(&wire[..split], &mut events);
            decoder.decode(&wire[split..], &mut events);
            decoder.finish(&mut events);
            decoder.finish(&mut events);
            assert_eq!(events.first(), Some(&CodexEvent::TextDelta("hello".into())));
            assert_eq!(events.last(), Some(&CodexEvent::Completed { usage: None }));
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, CodexEvent::Completed { .. }))
                    .count(),
                1
            );
            assert!(!events
                .iter()
                .any(|event| matches!(event, CodexEvent::Failed { .. })));
        }
    }

    #[test]
    fn error_payload_emits_failed_event_and_prevents_completed() {
        for (payload, expected) in [
            (
                r#"{"__type":"ValidationException","message":"Token limit exceeded"}"#,
                "Token limit exceeded",
            ),
            (
                r#"{"error":"AccessDeniedException","Message":"Denied"}"#,
                "Denied",
            ),
            (r#"{"error":{"message":"Nested denial"}}"#, "Nested denial"),
            (r#"{"__type":"ValidationException"}"#, "ValidationException"),
            (
                r#"{"error":"AccessDeniedException"}"#,
                "AccessDeniedException",
            ),
        ] {
            let mut decoder = KiroDecoder::new();
            let mut events = Vec::new();
            decoder.decode(payload.as_bytes(), &mut events);
            decoder.decode(
                br#"{"stopReason":"end_turn"}{"content":"late"}"#,
                &mut events,
            );
            decoder.finish(&mut events);
            assert!(
                matches!(events.as_slice(), [CodexEvent::Failed { message }] if message == expected),
                "{payload}: {events:?}"
            );
        }
    }

    #[test]
    fn aws_error_frames_fail_at_every_chunk_boundary_after_text() {
        for message_type in ["exception", "error"] {
            let text = aws_frame("event", br#"{"content":"committed"}"#);
            let error = aws_frame(message_type, br#"{"message":"Denied"}"#);
            let stop = aws_frame("event", br#"{"stopReason":"end_turn"}"#);
            let wire = [text, error, stop].concat();
            for split in 0..=wire.len() {
                let mut decoder = KiroDecoder::new();
                let mut events = Vec::new();
                decoder.decode(&wire[..split], &mut events);
                decoder.decode(&wire[split..], &mut events);
                decoder.finish(&mut events);
                assert!(
                    matches!(events.as_slice(), [CodexEvent::TextDelta(text), CodexEvent::Failed { message }] if text == "committed" && message == "Denied"),
                    "{message_type} split {split}: {events:?}"
                );
                let mut renderer =
                    super::super::render::ChunkRenderer::new("kiro/test".to_string(), 0, false);
                let frames: Vec<_> = events
                    .into_iter()
                    .flat_map(|event| renderer.render(event))
                    .collect();
                let payloads: Vec<Value> = frames
                    .iter()
                    .filter(|frame| frame.as_ref() != super::super::render::DONE_FRAME)
                    .map(|frame| serde_json::from_slice(&frame[6..frame.len() - 2]).unwrap())
                    .collect();
                assert_eq!(payloads.last().unwrap()["error"]["message"], "Denied");
                assert!(payloads
                    .iter()
                    .all(|payload| payload["choices"][0]["finish_reason"].is_null()));
                assert!(renderer.terminated());
                assert!(renderer.close_unterminated().is_empty());
            }
        }
    }

    #[test]
    fn normal_message_and_partial_aws_frames_are_not_errors() {
        let wire = [
            aws_frame("event", br#"{"message":"metadata"}"#),
            aws_frame("event", br#"{"content":"hello {world}"}"#),
        ]
        .concat();
        for split in 0..=wire.len() {
            let mut decoder = KiroDecoder::new();
            let mut events = Vec::new();
            decoder.decode(&wire[..split], &mut events);
            decoder.decode(&wire[split..], &mut events);
            decoder.finish(&mut events);
            assert!(
                matches!(events.as_slice(), [CodexEvent::TextDelta(text), CodexEvent::Completed { .. }] if text == "hello {world}"),
                "split {split}: {events:?}"
            );
        }
    }

    #[test]
    fn truncated_aws_frame_fails_instead_of_completing() {
        let wire = aws_frame("event", br#"{"content":"hello"}"#);
        let mut decoder = KiroDecoder::new();
        let mut events = Vec::new();
        decoder.decode(&wire[..wire.len() - 1], &mut events);
        decoder.finish(&mut events);
        assert!(
            matches!(events.as_slice(), [CodexEvent::Failed { .. }]),
            "{events:?}"
        );
    }

    #[test]
    fn multibyte_split_across_chunks_survives_reassembly() {
        // Network chunks are not aligned to UTF-8 boundaries. Converting each
        // chunk with from_utf8_lossy replaces the trailing partial sequence
        // with U+FFFD before the buffer can rejoin it, corrupting CJK/emoji
        // text and tool-argument JSON.
        let full: Vec<u8> = br#"{"content":""#
            .iter()
            .copied()
            .chain("あい".bytes())
            .chain(br#""}"#.iter().copied())
            .collect();
        // Split inside the first multi-byte character.
        let split = full
            .iter()
            .position(|b| *b == 0xe3)
            .expect("multibyte start")
            + 1;
        let mut decoder = KiroDecoder::new();
        let mut out = Vec::new();
        decoder.decode(&full[..split], &mut out);
        decoder.decode(&full[split..], &mut out);
        let text: String = out
            .iter()
            .filter_map(|event| match event {
                CodexEvent::TextDelta(delta) => Some(delta.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "あい", "chunk split corrupted the text");
        assert!(!text.contains('�'), "decoder emitted replacement chars");
    }

    #[test]
    fn decodes_streamed_tool_call_into_codex_events() {
        let mut decoder = KiroDecoder::new();
        let mut events = Vec::new();
        decoder.decode(
            br#"{"name":"lookup","toolUseId":"call_1"}{"input":"{\"q\":\"x\"}","stop":true}"#,
            &mut events,
        );
        assert!(events.iter().any(|event| matches!(event, CodexEvent::ToolCallBegin { call_id, name, .. } if call_id == "call_1" && name == "lookup")));
        assert!(events.iter().any(
            |event| matches!(event, CodexEvent::ToolArgsDelta { delta, .. } if delta.contains("q"))
        ));
    }

    #[test]
    fn replays_tool_results_images_and_reasoning_on_kiro_wire() {
        let payload = openai_to_kiro(&json!({
            "model": "kiro/claude-opus-5.5",
            "messages": [
                {"role":"user","content":"look"},
                {"role":"assistant","content":"", "kiroRedactedReasoning":"blob", "tool_calls":[{
                    "id":"call|1", "type":"function",
                    "function":{"name":"inspect","arguments":"{}"}
                }]},
                {"role":"tool","tool_call_id":"call|1","content":[
                    {"type":"text","text":"done"},
                    {"type":"image_url","image_url":{"url":"data:image/jpg;base64,abc"}}
                ]}
            ]
        }))
        .unwrap();
        let state = &payload["conversationState"];
        assert_eq!(
            state["history"][1]["assistantResponseMessage"]["toolUses"][0]["toolUseId"],
            "call_1"
        );
        assert_eq!(
            state["history"][1]["assistantResponseMessage"]["reasoningContent"]["redactedContent"],
            "blob"
        );
        let current = &state["currentMessage"]["userInputMessage"];
        assert_eq!(
            current["userInputMessageContext"]["toolResults"][0]["toolUseId"],
            "call_1"
        );
        assert_eq!(
            current["userInputMessageContext"]["toolResults"][0]["content"][0]["text"],
            "done"
        );
        assert_eq!(current["images"][0]["format"], "jpeg");
        assert_eq!(current["images"][0]["source"]["bytes"], "abc");
    }

    #[test]
    fn profile_reasoning_and_object_tool_input_follow_reference_wire() {
        let payload = openai_to_kiro_with_profile(
            &json!({
                "model":"kiro/claude-opus-5.5",
                "messages":[{"role":"user","content":"hi"}]
            }),
            Some("arn:aws:codewhisperer:us-east-1:123:profile/abc"),
        )
        .expect("translation");
        assert_eq!(
            payload["profileArn"],
            "arn:aws:codewhisperer:us-east-1:123:profile/abc"
        );

        let mut decoder = KiroDecoder::new();
        let mut events = Vec::new();
        decoder.decode(
            br#"{"text":"internal"}{"name":"bash","toolUseId":"call_1","input":{"cmd":"ls"}}"#,
            &mut events,
        );
        assert!(!events
            .iter()
            .any(|event| matches!(event, CodexEvent::TextDelta(text) if text == "internal")));
        assert!(events.iter().any(
            |event| matches!(event, CodexEvent::ToolArgsDelta { delta, .. } if delta.contains("ls"))
        ));
    }
}
