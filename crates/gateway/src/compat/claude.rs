use bytes::Bytes;
use serde_json::{json, Map, Value};

use super::events::{CodexEvent, Usage};

/// Kiro-style upstreams mark imported tools with a single `custom_` prefix.
/// Strip at most one occurrence: `trim_start_matches` removes the substring
/// repeatedly, so a genuine `custom_custom_y` would be corrupted to `custom_y`.
fn strip_custom_prefix(name: &str) -> &str {
    name.strip_prefix("custom_").unwrap_or(name)
}

#[derive(Default)]
pub struct AnthropicDecoder {
    started: bool,
    terminated: bool,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    cache_read_tokens_known: bool,
    cache_write_tokens_known: bool,
    next_tool_index: u64,
    /// Upstream content-block index of each client `tool_use` -> its output index.
    tool_blocks: std::collections::HashMap<u64, u64>,
    /// Upstream content-block index of each `server_tool_use` -> (id, input JSON).
    /// The upstream runs these itself, so they never become client tool calls.
    server_blocks: std::collections::HashMap<u64, (String, String)>,
    diagnostics: StreamDiagnostics,
}

/// What one upstream Anthropic message actually carried. Kept so a stream that
/// ends without visible content can be told apart: an upstream that omitted
/// `output_tokens` versus one that reported zero, and the real `stop_reason`
/// (e.g. `refusal`) that the OpenAI-shaped downstream collapses into `stop`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StreamDiagnostics {
    pub message_id: Option<String>,
    pub stop_reason: Option<String>,
    pub output_tokens_reported: bool,
    pub text_deltas: u64,
    pub thinking_deltas: u64,
    pub tool_calls: u64,
}

impl StreamDiagnostics {
    pub fn has_visible_output(&self) -> bool {
        self.text_deltas > 0 || self.tool_calls > 0
    }
}

pub type ClaudeDecoder = AnthropicDecoder;

impl AnthropicDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn decode(&mut self, frame: &[u8], out: &mut Vec<CodexEvent>) {
        let Ok(value) = serde_json::from_slice::<Value>(frame) else {
            return;
        };
        if self.terminated && value["type"] != "message_start" {
            return;
        }
        match value.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                *self = Self::default();
                self.started = true;
                let usage = &value["message"]["usage"];
                self.input_tokens = usage["input_tokens"].as_u64().unwrap_or(0);
                self.cache_read_tokens = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
                self.cache_write_tokens =
                    usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                self.cache_read_tokens_known = usage["cache_read_input_tokens"].as_u64().is_some();
                self.cache_write_tokens_known =
                    usage["cache_creation_input_tokens"].as_u64().is_some();
                self.diagnostics = StreamDiagnostics {
                    message_id: value["message"]["id"].as_str().map(str::to_string),
                    ..StreamDiagnostics::default()
                };
                self.tool_blocks.clear();
                self.server_blocks.clear();
                out.push(CodexEvent::Created {
                    response_id: value["message"]["id"]
                        .as_str()
                        .unwrap_or("msg_anthropic")
                        .to_string(),
                });
            }
            Some("content_block_start") if value["content_block"]["type"] == "server_tool_use" => {
                if let Some(index) = value["index"].as_u64() {
                    let id = value["content_block"]["id"].as_str().unwrap_or_default();
                    self.server_blocks
                        .insert(index, (id.to_string(), String::new()));
                }
            }
            Some("content_block_start")
                if value["content_block"]["type"] == "web_search_tool_result" =>
            {
                let block = &value["content_block"];
                let query = self
                    .server_blocks
                    .values()
                    .find(|(id, _)| block["tool_use_id"].as_str() == Some(id.as_str()))
                    .and_then(|(_, input)| serde_json::from_str::<Value>(input).ok())
                    .and_then(|input| input["query"].as_str().map(str::to_string));
                if let Some(text) = web_search_result_text(block, query.as_deref()) {
                    self.diagnostics.text_deltas += 1;
                    out.push(CodexEvent::TextDelta(text));
                }
            }
            Some("content_block_start") if value["content_block"]["type"] == "tool_use" => {
                self.diagnostics.tool_calls += 1;
                if let Some(index) = value["index"].as_u64() {
                    self.tool_blocks.insert(index, self.next_tool_index);
                }
                out.push(CodexEvent::ToolCallBegin {
                    output_index: self.next_tool_index,
                    call_id: value["content_block"]["id"]
                        .as_str()
                        .unwrap_or("toolu_anthropic")
                        .to_string(),
                    name: value["content_block"]["name"]
                        .as_str()
                        .map(strip_custom_prefix)
                        .unwrap_or("tool")
                        .to_string(),
                });
                self.next_tool_index += 1;
            }
            Some("content_block_delta") => match value["delta"]["type"].as_str() {
                Some("text_delta") => {
                    if let Some(text) = value["delta"]["text"].as_str() {
                        if !text.trim().is_empty() {
                            self.diagnostics.text_deltas += 1;
                        }
                        out.push(CodexEvent::TextDelta(text.to_string()));
                    }
                }
                Some("thinking_delta") | Some("reasoning_delta") => {
                    if let Some(text) = value["delta"]["thinking"]
                        .as_str()
                        .or_else(|| value["delta"]["text"].as_str())
                    {
                        self.diagnostics.thinking_deltas += 1;
                        out.push(CodexEvent::ReasoningDelta(text.to_string()));
                    }
                }
                Some("signature_delta") => {
                    if let Some(signature) = value["delta"]["signature"].as_str() {
                        out.push(CodexEvent::ReasoningSignature(signature.to_string()));
                    }
                }
                Some("input_json_delta") => {
                    let Some(delta) = value["delta"]["partial_json"].as_str() else {
                        return;
                    };
                    let index = value["index"].as_u64();
                    if let Some((_, input)) = index.and_then(|i| self.server_blocks.get_mut(&i)) {
                        input.push_str(delta);
                        return;
                    }
                    // Arguments belong only to a client tool_use whose start was
                    // seen; an orphan delta would surface as a nameless tool call.
                    let output_index = match index {
                        Some(i) => self.tool_blocks.get(&i).copied(),
                        None => self.next_tool_index.checked_sub(1),
                    };
                    if let Some(output_index) = output_index {
                        out.push(CodexEvent::ToolArgsDelta {
                            output_index,
                            delta: delta.to_string(),
                        });
                    }
                }
                _ => {}
            },
            Some("message_delta") => {
                if value.pointer("/delta/stop_reason").and_then(Value::as_str) == Some("max_tokens")
                {
                    out.push(CodexEvent::OutputLimitReached);
                }
                if let Some(stop) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.diagnostics.stop_reason = Some(stop.to_string());
                }
                let usage = &value["usage"];
                self.output_tokens = usage["output_tokens"].as_u64().unwrap_or(0);
                self.diagnostics.output_tokens_reported = usage["output_tokens"].as_u64().is_some();
                // Relays such as CCAPI open Opus streams with a placeholder
                // message_start whose usage is all zero and report the real
                // input/cache counts only here; prefer them when present.
                if let Some(input) = usage["input_tokens"].as_u64() {
                    self.input_tokens = input;
                }
                if let Some(read) = usage["cache_read_input_tokens"].as_u64() {
                    self.cache_read_tokens = read;
                    self.cache_read_tokens_known = true;
                }
                if let Some(write) = usage["cache_creation_input_tokens"].as_u64() {
                    self.cache_write_tokens = write;
                    self.cache_write_tokens_known = true;
                }
            }
            Some("message_stop") => {
                self.started = false;
                self.terminated = true;
                if !self.diagnostics.has_visible_output() {
                    let d = &self.diagnostics;
                    tracing::warn!(
                        message_id = d.message_id.as_deref().unwrap_or(""),
                        stop_reason = d.stop_reason.as_deref().unwrap_or("<missing>"),
                        output_tokens = self.output_tokens,
                        output_tokens_reported = d.output_tokens_reported,
                        thinking_deltas = d.thinking_deltas,
                        "anthropic upstream message ended without text or tool calls"
                    );
                }
                let prompt_tokens =
                    self.input_tokens + self.cache_read_tokens + self.cache_write_tokens;
                out.push(CodexEvent::Completed {
                    usage: Some(Usage {
                        prompt_tokens,
                        completion_tokens: self.output_tokens,
                        total_tokens: prompt_tokens + self.output_tokens,
                        cached_tokens: self.cache_read_tokens,
                        cached_tokens_known: self.cache_read_tokens_known,
                        reasoning_tokens: 0,
                        cache_write_tokens: self.cache_write_tokens,
                        cache_write_tokens_known: self.cache_write_tokens_known,
                    }),
                });
            }
            Some("error") => {
                self.terminated = true;
                out.push(CodexEvent::Failed {
                    message: value["error"]["message"]
                        .as_str()
                        .unwrap_or("Anthropic upstream error")
                        .to_string(),
                });
            }
            _ => {}
        }
    }

    pub fn diagnostics(&self) -> &StreamDiagnostics {
        &self.diagnostics
    }

    pub fn finish(&mut self, out: &mut Vec<CodexEvent>) {
        if !self.terminated {
            self.terminated = true;
            self.started = false;
            out.push(CodexEvent::Failed {
                message: "Anthropic stream ended before message_stop".into(),
            });
        }
    }
}

pub fn anthropic_to_openai(body: &Value) -> Result<Value, String> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing model".to_string())?;

    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing messages".to_string())?;

    let mut out_messages: Vec<Value> = Vec::new();
    if let Some(system) = body.get("system") {
        if let Some(text) = system_to_text(system) {
            if !text.is_empty() {
                out_messages.push(json!({ "role": "system", "content": text }));
            }
        }
    }

    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
        // Anthropic's cache-preserving per-message effort switch, relayed to
        // the OpenAI wire as the marker `configuration_update` consumes.
        if matches!(role, "system" | "developer") {
            if let Some(effort) = msg.pointer("/output_config/effort").and_then(Value::as_str) {
                out_messages.push(json!({
                    "role": "system",
                    "content": "",
                    "x_effort_update": effort,
                }));
                continue;
            }
        }
        let content = msg.get("content");

        let mut text_parts: Vec<String> = Vec::new();
        let mut image_parts: Vec<Value> = Vec::new();
        let mut tool_calls: Vec<Value> = Vec::new();
        let mut tool_results: Vec<Value> = Vec::new();
        let mut thinking_parts: Vec<String> = Vec::new();
        let mut signature: Option<String> = None;
        let mut redacted = false;

        match content {
            Some(Value::String(s)) => text_parts.push(s.clone()),
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = block.get("text").and_then(Value::as_str) {
                                text_parts.push(t.to_string());
                            }
                        }
                        Some("image") => {
                            let source = &block["source"];
                            if source["type"] == "base64" {
                                image_parts.push(json!({
                                    "type": "image_url",
                                    "image_url": {
                                        "url": format!(
                                            "data:{};base64,{}",
                                            source["media_type"].as_str().unwrap_or("application/octet-stream"),
                                            source["data"].as_str().unwrap_or("")
                                        )
                                    }
                                }));
                            } else if source["type"] == "url" {
                                image_parts.push(json!({
                                    "type": "image_url",
                                    "image_url": {"url": source["url"]}
                                }));
                            }
                        }
                        Some("thinking") => {
                            if let Some(th) = block.get("thinking").and_then(Value::as_str) {
                                thinking_parts.push(th.to_string());
                            }
                            if let Some(sig) = block.get("signature").and_then(Value::as_str) {
                                signature = Some(sig.to_string());
                            }
                        }
                        Some("redacted_thinking") => {
                            redacted = true;
                            if let Some(data) = block.get("data").and_then(Value::as_str) {
                                signature = Some(data.to_string());
                            }
                        }
                        Some("tool_use") => {
                            let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                            let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                            let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": { "name": name, "arguments": input.to_string() }
                            }));
                        }
                        Some("tool_result") => {
                            let id = block
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let content = block
                                .get("content")
                                .cloned()
                                .unwrap_or(Value::String(String::new()));
                            let output_text = match &content {
                                Value::String(text) => Value::String(text.clone()),
                                Value::Array(items) => Value::Array(
                                    items
                                        .iter()
                                        .map(|item| match item["type"].as_str() {
                                            Some("image") => {
                                                let source = &item["source"];
                                                if source["type"] == "base64" {
                                                    json!({
                                                        "type": "image_url",
                                                        "image_url": {
                                                            "url": format!(
                                                                "data:{};base64,{}",
                                                                source["media_type"].as_str().unwrap_or("application/octet-stream"),
                                                                source["data"].as_str().unwrap_or("")
                                                            )
                                                        }
                                                    })
                                                } else if source["type"] == "url" {
                                                    json!({
                                                        "type": "image_url",
                                                        "image_url": {"url": source["url"]}
                                                    })
                                                } else {
                                                    item.clone()
                                                }
                                            }
                                            _ => item.clone(),
                                        })
                                        .collect(),
                                ),
                                other => other.clone(),
                            };
                            let is_error = block.get("is_error").and_then(Value::as_bool);
                            let mut tr = json!({
                                "role": "tool",
                                "tool_call_id": id,
                                "content": output_text,
                                "x_anthropic_tool_result": true
                            });
                            if let Some(cache_control) = block.get("cache_control") {
                                tr["cache_control"] = cache_control.clone();
                            }
                            if let Some(err) = is_error {
                                tr["is_error"] = json!(err);
                            }
                            tool_results.push(tr);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }

        out_messages.extend(tool_results);
        if role == "assistant"
            && (!tool_calls.is_empty()
                || !thinking_parts.is_empty()
                || signature.is_some()
                || redacted)
        {
            let mut m = Map::new();
            m.insert("role".to_string(), json!("assistant"));
            m.insert("content".to_string(), json!(text_parts.join("")));
            if !tool_calls.is_empty() {
                m.insert("tool_calls".to_string(), Value::Array(tool_calls));
            }
            if !thinking_parts.is_empty() {
                m.insert(
                    "reasoning_content".to_string(),
                    json!(thinking_parts.join("")),
                );
            }
            if let Some(sig) = signature {
                m.insert("reasoning_signature".to_string(), json!(sig));
            }
            if redacted {
                m.insert("reasoning_redacted".to_string(), json!(true));
            }
            out_messages.push(Value::Object(m));
        } else if !tool_calls.is_empty() {
            let mut m = Map::new();
            m.insert("role".to_string(), json!("assistant"));
            m.insert("content".to_string(), json!(text_parts.join("")));
            m.insert("tool_calls".to_string(), Value::Array(tool_calls));
            out_messages.push(Value::Object(m));
        } else if !text_parts.is_empty() || !image_parts.is_empty() {
            let content = if image_parts.is_empty() {
                json!(text_parts.join(""))
            } else {
                let mut parts = Vec::new();
                if !text_parts.is_empty() {
                    parts.push(json!({ "type": "text", "text": text_parts.join("") }));
                }
                parts.extend(image_parts);
                Value::Array(parts)
            };
            out_messages.push(json!({ "role": role, "content": content }));
        }
    }

    let mut out = Map::new();
    out.insert("model".to_string(), json!(model));
    out.insert("messages".to_string(), Value::Array(out_messages));

    if let Some(stop) = body.get("stop_sequences") {
        out.insert("stop".to_string(), stop.clone());
    }
    let thinking_type = body["thinking"]["type"].as_str();
    let effort = if thinking_type == Some("disabled") {
        Some("none".to_string())
    } else if let Some(effort) = body["output_config"]["effort"].as_str() {
        Some(effort.to_string())
    } else if let Some(budget) = body["thinking"]["budget_tokens"].as_u64() {
        Some(
            super::request::normalize_thinking_level(&budget.to_string())
                .map_err(|error| error.to_string())?,
        )
    } else if matches!(thinking_type, Some("adaptive" | "enabled")) {
        Some("auto".to_string())
    } else {
        None
    };
    if let Some(effort) = effort {
        out.insert("reasoning_effort".to_string(), json!(effort));
    }
    if let Some(m) = body.get("max_tokens").and_then(Value::as_i64) {
        out.insert("max_tokens".to_string(), json!(m));
    }
    if let Some(t) = body.get("temperature").and_then(Value::as_f64) {
        out.insert("temperature".to_string(), json!(t));
    }
    if let Some(p) = body.get("top_p").and_then(Value::as_f64) {
        out.insert("top_p".to_string(), json!(p));
    }
    if let Some(s) = body.get("stream").and_then(Value::as_bool) {
        out.insert("stream".to_string(), json!(s));
    }

    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let mapped: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name").and_then(Value::as_str).unwrap_or(""),
                        "description": t.get("description").and_then(Value::as_str).unwrap_or(""),
                        "parameters": t.get("input_schema").cloned().unwrap_or_else(|| json!({})),
                    }
                })
            })
            .collect();
        if !mapped.is_empty() {
            out.insert("tools".to_string(), Value::Array(mapped));
        }
    }

    if let Some(choice) = body.get("tool_choice") {
        let mapped = match choice["type"].as_str() {
            Some("auto") => json!("auto"),
            Some("none") => json!("none"),
            Some("any") => json!("required"),
            Some("tool") => {
                let name = choice["name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| "tool_choice tool requires a name".to_string())?;
                json!({"type":"function","function":{"name":name}})
            }
            _ => return Err("unsupported Anthropic tool_choice".to_string()),
        };
        out.insert("tool_choice".to_string(), mapped);
        if let Some(disabled) = choice.get("disable_parallel_tool_use") {
            let disabled = disabled
                .as_bool()
                .ok_or_else(|| "disable_parallel_tool_use must be boolean".to_string())?;
            out.insert("parallel_tool_calls".to_string(), json!(!disabled));
        }
    }

    Ok(Value::Object(out))
}

/// Claude models that accept the `mid-conversation-output-config-2026-07-01`
/// beta: `output_config.effort` on a `role: "system"` message placed partway
/// through the history. Mirrors the documented support list; everything else
/// must take the top-level effort (which does invalidate the message cache).
pub fn supports_mid_conversation_effort(model: &str) -> bool {
    let model = model.rsplit('/').next().unwrap_or(model);
    const SUPPORTED: [&str; 6] = [
        "claude-fable-5-1",
        "claude-mythos-5-1",
        "claude-opus-5-5",
        "claude-opus-5",
        "claude-sonnet-5-5",
        "claude-haiku-5-5",
    ];
    SUPPORTED.iter().any(|base| {
        if model == *base {
            return true;
        }
        // A variant suffix (`-high`, `-medium`, …) is the same model; a longer
        // numeric tail (`-50`) is a different id and must not match.
        const VARIANTS: [&str; 7] = ["high", "medium", "low", "xhigh", "minimal", "max", "none"];
        model
            .strip_prefix(base)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|tail| VARIANTS.contains(&tail))
    })
}

/// Beta that unlocks per-message `output_config.effort`. Without it the
/// upstream rejects `messages.N.output_config` outright.
pub const MID_CONVERSATION_EFFORT_BETA: &str = "mid-conversation-output-config-2026-07-01";

/// True when the translated body carries a mid-conversation effort message, so
/// the caller knows the beta header must ride along.
pub fn uses_mid_conversation_effort(body: &Value) -> bool {
    body.get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages.iter().any(|message| {
                message.get("role").and_then(Value::as_str) == Some("system")
                    && message.get("output_config").is_some()
            })
        })
}

/// Same check on the already-encoded upstream body.
///
/// Every Anthropic request passes here, so a byte scan rejects the common case
/// before paying for a full deserialize.
pub fn uses_mid_conversation_effort_bytes(raw: &[u8]) -> bool {
    const NEEDLE: &[u8] = b"\"output_config\"";
    if !raw.windows(NEEDLE.len()).any(|window| window == NEEDLE) {
        return false;
    }
    serde_json::from_slice::<Value>(raw)
        .map(|body| uses_mid_conversation_effort(&body))
        .unwrap_or(false)
}

/// Applies a requested thinking mode (model suffix or reasoning effort) to an
/// Anthropic request `result` per the catalog capability. Shared by the
/// OpenAI->Anthropic translation and native `/v1/messages` passthrough, so a
/// `claude-*(2048)` suffix behaves the same on both wires. Returns whether
/// thinking ended up enabled.
pub fn apply_thinking_mode(
    raw_model: &str,
    body: &Value,
    capability: &mahoquot_registry::ThinkingCapability,
    result: &mut Value,
) -> Result<bool, String> {
    let mut thinking_enabled = false;
    if let Some(mode) =
        super::request::thinking_mode(raw_model, body).map_err(|error| error.to_string())?
    {
        use super::request::ThinkingMode;
        use mahoquot_registry::ThinkingCapability;
        let adaptive = matches!(capability, ThinkingCapability::Supported(support) if !support.levels.is_empty());
        let mode = super::request::target_thinking_mode(mode, capability, true)?;
        let budget = match mode {
            ThinkingMode::None => {
                result["thinking"] = json!({"type":"disabled"});
                None
            }
            ThinkingMode::Auto => {
                result["thinking"] = json!({"type":if adaptive { "adaptive" } else { "enabled" }});
                thinking_enabled = true;
                None
            }
            ThinkingMode::Level(level) if adaptive => {
                result["thinking"] = json!({"type":"adaptive"});
                result["output_config"] = json!({"effort":level});
                thinking_enabled = true;
                None
            }
            ThinkingMode::Level(level) => Some(super::request::level_budget(&level)),
            ThinkingMode::Budget(budget) => Some(budget),
        };
        if let Some(budget) = budget {
            let (minimum, maximum) = match capability {
                ThinkingCapability::Supported(support) => (support.min, support.max),
                ThinkingCapability::Unknown => (1024, 128000),
                ThinkingCapability::Unsupported => (0, 0),
            };
            let budget =
                u64::try_from(budget.clamp(minimum, maximum)).map_err(|error| error.to_string())?;
            let max_tokens = result["max_tokens"].as_u64().unwrap_or(32_768);
            if max_tokens <= u64::try_from(minimum).map_err(|error| error.to_string())? {
                return Err("max_tokens cannot accommodate Claude minimum thinking budget: unsupported option conflict".to_string());
            }
            result["thinking"] =
                json!({"type":"enabled", "budget_tokens":budget.min(max_tokens - 1)});
            thinking_enabled = true;
        }
    }
    Ok(thinking_enabled)
}

/// Native Anthropic body: resolve a thinking suffix on the requested model into
/// `thinking`/`output_config`, then send the bare upstream model.
pub fn apply_native_thinking_suffix(
    requested_model: &str,
    upstream_model: &str,
    body: &mut Value,
) -> Result<(), String> {
    let (_, suffix) =
        super::request::parse_model_suffix(requested_model).map_err(|e| e.to_string())?;
    if suffix.is_none() {
        return Ok(());
    }
    let capability = super::request::embedded_thinking_capability(requested_model, "claude")?;
    let original = body.clone();
    if let Some(object) = body.as_object_mut() {
        object.remove("thinking");
        object.remove("output_config");
    }
    let enabled = apply_thinking_mode(requested_model, &original, &capability, body)?;
    if enabled && body.get("temperature").is_some() {
        body["temperature"] = json!(1.0);
    }
    let (bare, _) =
        super::request::parse_model_suffix(upstream_model).map_err(|e| e.to_string())?;
    body["model"] = json!(bare);
    Ok(())
}

pub fn openai_to_anthropic(body: &Value) -> Result<Value, String> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing model".to_string())?;
    let capability = super::request::embedded_thinking_capability(model, "claude")?;
    openai_to_anthropic_with_capability(body, &capability)
}

pub fn openai_to_anthropic_with_capability(
    body: &Value,
    capability: &mahoquot_registry::ThinkingCapability,
) -> Result<Value, String> {
    let raw_model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing model".to_string())?;
    let (model, _) = super::request::parse_model_suffix(raw_model).map_err(|e| e.to_string())?;

    // The pinned canonical Claude catalog advertises thinking levels, but no
    // native structured-output capability. Its translator uses system prompt
    // instructions instead; that substitute is not part of our contract.
    if let Some(rf) = body.get("response_format") {
        if !rf.is_null() {
            match rf.get("type").and_then(Value::as_str) {
                Some("text") => {}
                Some("json_schema") | Some("json_object") => return Err(format!(
                    "Claude model '{model}' has no catalog-advertised native structured-output capability: unsupported option conflict"
                )),
                _ => return Err("unsupported response_format type or shape".to_string()),
            }
        }
    }
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing messages".to_string())?;
    let mut system = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    // Beta-gated per-message effort only exists for these models; elsewhere the
    // switch is folded into the top-level `output_config` instead of emitting a
    // message the upstream would reject.
    let per_message_effort = supports_mid_conversation_effort(model);
    let mut unsupported_effort: Option<String> = None;
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        if role == "system" || role == "developer" {
            if let Some(effort) = message.get("x_effort_update").and_then(Value::as_str) {
                if !per_message_effort {
                    unsupported_effort = Some(effort.to_string());
                    continue;
                }
                // Cache-preserving effort switch: Anthropic renders a mid-stream
                // `role: system` message instead of rewriting the top-level
                // `output_config`, so every earlier block stays inside the
                // cached prefix. Only emitted when the client itself asked for
                // it with a `configuration_update` input item.
                let entry = json!({
                    "role": "system",
                    "content": [],
                    "output_config": {"effort": effort},
                });
                out.push(entry);
                continue;
            }
            if let Some(text) = message.get("content").and_then(content_text) {
                let mut block = json!({"type": "text", "text": text});
                if let Some(cache_control) = message.get("cache_control").filter(|c| c.is_object())
                {
                    block["cache_control"] = cache_control.clone();
                }
                if out.is_empty() {
                    system.push(block);
                } else {
                    // Moving a later instruction ahead of the existing history
                    // changes every cached message prefix. Keep its position.
                    out.push(json!({"role": "user", "content": [block]}));
                }
            }
            continue;
        }
        if role == "tool" {
            let content = match message.get("content") {
                Some(Value::Array(parts)) => Value::Array(
                    parts
                        .iter()
                        .map(openai_content_to_anthropic)
                        .collect::<Result<Vec<_>, _>>()?,
                ),
                Some(content) => content.clone(),
                None => Value::String(String::new()),
            };
            let block = json!({
                "type": "tool_result",
                "tool_use_id": message.get("tool_call_id").and_then(Value::as_str).unwrap_or(""),
                "content": content,
            });
            let mut block = block;
            if let Some(cache_control) = message.get("cache_control") {
                block["cache_control"] = cache_control.clone();
            }
            if let Some(last) = out.last_mut().filter(|last| last["role"] == "user") {
                if let Some(content) = last.get_mut("content").and_then(Value::as_array_mut) {
                    content.push(block);
                    continue;
                }
            }
            out.push(json!({"role": "user", "content": [block]}));
            continue;
        }
        let content = match message.get("content") {
            Some(Value::String(text)) => vec![json!({"type":"text","text":text})],
            Some(Value::Array(blocks)) => blocks
                .iter()
                .map(openai_content_to_anthropic)
                .collect::<Result<Vec<_>, _>>()?,
            _ => Vec::new(),
        };
        let mut content = content;
        if role == "assistant" {
            if let Some(signature) = message["reasoning_signature"].as_str().filter(|signature| {
                if message["reasoning_redacted"] == true {
                    !signature.is_empty()
                } else {
                    plausible_claude_signature(signature)
                }
            }) {
                if message["reasoning_redacted"] == true {
                    content.insert(0, json!({"type":"redacted_thinking","data":signature}));
                } else if let Some(thinking) = message["reasoning_content"].as_str() {
                    content.insert(
                        0,
                        json!({"type":"thinking","thinking":thinking,"signature":signature}),
                    );
                }
            }
        }
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let input = call["function"]["arguments"]
                    .as_str()
                    .and_then(|raw| serde_json::from_str(raw).ok())
                    .unwrap_or_else(|| json!({}));
                content.push(json!({
                    "type": "tool_use",
                    "id": call["id"],
                    "name": anthropic_tool_name(call["function"]["name"].as_str().unwrap_or("tool")),
                    "input": input,
                }));
            }
        }
        // Anthropic rejects both an empty text block and an empty content
        // array, so an assistant turn that carried only tool_calls (content
        // "" or null) must not contribute a blank block, and a message left
        // with no content at all is dropped rather than sent.
        content.retain(|block| {
            block.get("type").and_then(Value::as_str) != Some("text")
                || block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.is_empty())
        });
        if content.is_empty() {
            continue;
        }
        out.push(json!({"role": role, "content": content}));
    }
    let tools: Vec<Value> = body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, tool)| tool.get("function").map(|function| (index, function)))
        .map(|(index, function)| {
            // Built-in search maps to Claude's native server tool, not an
            // ordinary function replacement (Task 25).
            if function["name"].as_str() == Some("web_search") {
                return Ok(json!({"type": "web_search_20250305", "name": "web_search"}));
            }
            let name = anthropic_tool_name(function["name"].as_str().unwrap_or("tool"));
            let schema = super::tool_schema::anthropic_schema(function.get("parameters"))
                .map_err(|error| format!("tools[{index}].function.parameters: {error}"))?;
            let mut tool = json!({
                "name": name,
                "description": function["description"],
                "input_schema": schema,
            });
            if let Some(cache_control) = body["tools"][index]
                .get("cache_control")
                .filter(|c| c.is_object())
            {
                tool["cache_control"] = cache_control.clone();
            }
            Ok(tool)
        })
        .collect::<Result<Vec<_>, String>>()?;
    // Anthropic-style upstreams require max_tokens, but OpenAI-agent clients
    // often omit it. A small default truncates reasoning models mid-response
    // (thinking burns the output budget, so a tool_use block arrives cut off
    // and the agent loop stalls), so default generously — billing follows
    // actual output, not the cap.
    let mut result = json!({
        "model": model,
        "messages": out,
        "max_tokens": body.get("max_tokens").or_else(|| body.get("max_completion_tokens")).and_then(Value::as_u64).unwrap_or(32_768),
        "stream": body.get("stream").and_then(Value::as_bool).unwrap_or(false),
    });
    if !system.is_empty() {
        result["system"] = if system
            .iter()
            .any(|block| block.get("cache_control").is_some())
        {
            Value::Array(system)
        } else {
            Value::String(
                system
                    .iter()
                    .filter_map(|block| block["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            )
        };
    }
    if !tools.is_empty() {
        result["tools"] = Value::Array(tools);
    }
    let mut choice = match body.get("tool_choice") {
        None => None,
        Some(Value::String(kind)) => Some(match kind.as_str() {
            "auto" => json!({"type":"auto"}),
            "none" => json!({"type":"none"}),
            "required" => json!({"type":"any"}),
            _ => return Err("unsupported OpenAI tool_choice".to_string()),
        }),
        Some(choice) if choice["type"] == "function" => {
            let name = choice["function"]["name"]
                .as_str()
                .filter(|name| !name.is_empty())
                .ok_or_else(|| "tool_choice function requires a name".to_string())?;
            Some(json!({"type":"tool","name":anthropic_tool_name(name)}))
        }
        Some(_) => return Err("unsupported OpenAI tool_choice".to_string()),
    };
    if let Some(parallel) = body.get("parallel_tool_calls") {
        let parallel = parallel
            .as_bool()
            .ok_or_else(|| "parallel_tool_calls must be boolean".to_string())?;
        choice.get_or_insert_with(|| json!({"type":"auto"}))["disable_parallel_tool_use"] =
            json!(!parallel);
    }
    if let Some(choice) = choice {
        result["tool_choice"] = choice;
    }
    let thinking_enabled = apply_thinking_mode(raw_model, body, capability, &mut result)?;
    if let Some(effort) = unsupported_effort {
        // This model has no per-message switch; the client's latest requested
        // effort still wins, at the documented cost of the message cache.
        result["output_config"] = json!({"effort": effort});
    }

    for key in ["temperature", "top_p", "stop_sequences"] {
        if let Some(value) = body.get(key) {
            if key == "temperature" && thinking_enabled {
                // Anthropic requires temperature to be 1.0 or omitted when thinking is enabled
                result[key] = json!(1.0);
            } else {
                result[key] = value.clone();
            }
        }
    }
    match body.get("stop") {
        Some(Value::String(stop)) => result["stop_sequences"] = json!([stop]),
        Some(Value::Array(stops)) => result["stop_sequences"] = json!(stops),
        _ => {}
    }
    // Every OpenAI -> Anthropic translation leaves through here, and the
    // client's own markers are preserved, so the provider limit is enforced at
    // the translation boundary. A caller that adds path-specific breakpoints
    // afterwards must enforce it again (see the relay serialization point).
    enforce_cache_control_limit(&mut result, MAX_CACHE_BREAKPOINTS);
    Ok(result)
}

// Recognize the opaque Claude protobuf envelope, not arbitrary foreign-provider
// base64 or fabricated placeholder signatures. Preserve the original bytes.
fn plausible_claude_signature(signature: &str) -> bool {
    use base64::Engine as _;
    if signature.len() < 16 {
        return false;
    }
    let decode = |raw: &[u8]| {
        base64::engine::general_purpose::STANDARD
            .decode(raw)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(raw))
    };
    let Ok(mut bytes) = decode(signature.as_bytes()) else {
        return false;
    };
    if matches!(bytes.first(), Some(b'E' | b'C')) {
        let Ok(inner) = decode(&bytes) else {
            return false;
        };
        bytes = inner;
    }
    bytes.first() == Some(&0x12)
        || bytes.starts_with(&[0x08, 0x04, 0x12])
        || (bytes.first() == Some(&0x08)
            && bytes
                .windows(b"claude-".len())
                .any(|part| part == b"claude-"))
}

fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter(|part| part["type"] == "text")
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join(""),
        ),
        _ => None,
    }
}

const CLAUDE_IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// One OpenAI content part as an Anthropic block (Task 4): image bytes/MIME,
/// http(s) image URLs, PDF/text documents and ephemeral cache markers survive;
/// any media Claude cannot take is an explicit error before dispatch, never a
/// silent text-only downgrade.
fn openai_content_to_anthropic(part: &Value) -> Result<Value, String> {
    let mut block = match part["type"].as_str() {
        Some("text") => json!({"type": "text", "text": part["text"].as_str().unwrap_or_default()}),
        Some("image_url") => {
            let url = part["image_url"]["url"]
                .as_str()
                .or_else(|| part["image_url"].as_str())
                .ok_or("image_url.url must be a string: unsupported media")?;
            if let Some((media_type, data)) = super::split_data_url(url) {
                if !CLAUDE_IMAGE_TYPES.contains(&media_type) {
                    return Err(format!(
                        "unsupported image media type {media_type}: unsupported media"
                    ));
                }
                json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}})
            } else if url.starts_with("https://") || url.starts_with("http://") {
                json!({"type": "image", "source": {"type": "url", "url": url}})
            } else {
                return Err(
                    "image_url must be a data URL or http(s) URL: unsupported media".into(),
                );
            }
        }
        Some("file") => {
            let file = &part["file"];
            let data_url = file["file_data"]
                .as_str()
                .ok_or("file.file_data must be a data URL string: unsupported media")?;
            let (media_type, data) = super::split_data_url(data_url)
                .ok_or("file.file_data must be a base64 data URL: unsupported media")?;
            let source = match media_type {
                "application/pdf" => {
                    json!({"type": "base64", "media_type": media_type, "data": data})
                }
                "text/plain" => {
                    use base64::Engine as _;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .map_err(|_| "file.file_data is not valid base64: unsupported media")?;
                    let text = String::from_utf8(bytes)
                        .map_err(|_| "text document is not UTF-8: unsupported media")?;
                    json!({"type": "text", "media_type": media_type, "data": text})
                }
                other => {
                    return Err(format!(
                        "unsupported document media type {other}: unsupported media"
                    ))
                }
            };
            let mut document = json!({"type": "document", "source": source});
            if let Some(name) = file["filename"].as_str() {
                document["title"] = json!(name);
            }
            document
        }
        Some("image" | "document" | "tool_result" | "tool_use") => part.clone(),
        Some(other) => {
            return Err(format!(
                "unsupported content part type {other}: unsupported media"
            ))
        }
        None => return Err("content part missing type: unsupported media".into()),
    };
    if let Some(cache_control) = part.get("cache_control").filter(|c| c.is_object()) {
        block["cache_control"] = cache_control.clone();
    }
    Ok(block)
}

pub const CLAUDE_CODE_SYSTEM_INSTRUCTION: &str =
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.";

/// Ensure the Claude Code OAuth system instruction is present as the first system entry.
pub fn ensure_claude_code_system_instruction(body: &mut Value) {
    match body.get_mut("system") {
        Some(Value::Array(arr)) => {
            let already_has = arr.iter().any(|item| {
                item.get("text")
                    .and_then(Value::as_str)
                    .map(|t| t.contains(CLAUDE_CODE_SYSTEM_INSTRUCTION))
                    .unwrap_or(false)
            });
            if !already_has {
                arr.insert(
                    0,
                    json!({ "type": "text", "text": CLAUDE_CODE_SYSTEM_INSTRUCTION }),
                );
            }
        }
        Some(Value::String(s)) => {
            if !s.contains(CLAUDE_CODE_SYSTEM_INSTRUCTION) {
                *s = format!("{CLAUDE_CODE_SYSTEM_INSTRUCTION}\n\n{s}");
            }
        }
        None | Some(Value::Null) => {
            body["system"] = Value::Array(vec![
                json!({ "type": "text", "text": CLAUDE_CODE_SYSTEM_INSTRUCTION }),
            ]);
        }
        _ => {}
    }
}

/// Anthropic accepts at most four `cache_control` breakpoints per request
/// and answers a fifth with HTTP 400. Clients that manage their own markers
/// can exceed that, so excess markers are stripped before the request is
/// sent: messages first (least stable prefix), then system, then tools.
pub const MAX_CACHE_BREAKPOINTS: usize = 4;

fn breakpoint_count(body: &Value) -> usize {
    fn count(blocks: Option<&Vec<Value>>) -> usize {
        blocks
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|block| block.get("cache_control").is_some())
                    .count()
            })
            .unwrap_or(0)
    }
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .filter_map(|message| message.get("content").and_then(Value::as_array))
                .map(|blocks| count(Some(blocks)))
                .sum::<usize>()
        })
        .unwrap_or(0);
    messages
        + count(body.get("system").and_then(Value::as_array))
        + count(body.get("tools").and_then(Value::as_array))
}

fn strip_breakpoints(blocks: &mut Vec<Value>, excess: &mut usize) {
    for block in blocks {
        if *excess == 0 {
            return;
        }
        if block.get("cache_control").is_some() {
            if let Some(object) = block.as_object_mut() {
                object.remove("cache_control");
            }
            *excess -= 1;
        }
    }
}

/// Enforces [`MAX_CACHE_BREAKPOINTS`], mirroring both references.
pub fn enforce_cache_control_limit(body: &mut Value, limit: usize) {
    let total = breakpoint_count(body);
    if total <= limit {
        return;
    }
    let mut excess = total - limit;
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            if excess == 0 {
                break;
            }
            if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
                strip_breakpoints(blocks, &mut excess);
            }
        }
    }
    if excess > 0 {
        if let Some(system) = body.get_mut("system").and_then(Value::as_array_mut) {
            strip_breakpoints(system, &mut excess);
        }
    }
    if excess > 0 {
        if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
            strip_breakpoints(tools, &mut excess);
        }
    }
}

/// Anthropic only caches a prefix up to an explicit `cache_control`
/// breakpoint. Translated OpenAI requests carry none, so only the prefix the
/// relay marks itself (the system prompt) gets cached and the growing
/// conversation is re-billed as uncached input every turn. Marks the last
/// message block, which caches system + tools + history as one prefix.
///
/// `system` is deliberately left untouched: the CCAPI relay keeps a string
/// system it caches itself, but drops a system that arrives as content
/// blocks (measured 2026-10-12: block-form system cost the same input tokens
/// as no system at all). A request that already carries a breakpoint is left
/// as sent, after the provider limit is enforced.
pub fn apply_prompt_cache_breakpoints(body: &mut Value) {
    enforce_cache_control_limit(body, MAX_CACHE_BREAKPOINTS);
    if body.to_string().contains("\"cache_control\"") {
        return;
    }
    let ephemeral = || json!({"type": "ephemeral"});
    let Some(message) = body
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .and_then(|messages| {
            // A trailing effort directive is an instruction, not cached content:
            // the breakpoint belongs on the last real turn before it.
            messages
                .iter_mut()
                .rev()
                .find(|message| message.get("output_config").is_none())
        })
    else {
        return;
    };
    if let Some(Value::String(text)) = message.get("content") {
        if !text.is_empty() {
            message["content"] =
                json!([{"type": "text", "text": text, "cache_control": ephemeral()}]);
        }
        return;
    }
    // Thinking blocks cannot carry cache_control.
    if let Some(block) = message
        .get_mut("content")
        .and_then(Value::as_array_mut)
        .and_then(|blocks| {
            blocks.iter_mut().rev().find(|block| {
                !matches!(
                    block.get("type").and_then(Value::as_str),
                    Some("thinking") | Some("redacted_thinking")
                )
            })
        })
        .and_then(Value::as_object_mut)
    {
        block.insert("cache_control".to_string(), ephemeral());
    }
}

/// Upstream-executed web search surfaced as visible text: the query and the
/// result titles/URLs, so the answer that follows keeps its provenance. The
/// client never receives it as a tool call it would have to execute.
fn web_search_result_text(block: &Value, query: Option<&str>) -> Option<String> {
    let mut text = match query {
        Some(query) => format!("\n[web_search: {query}]\n"),
        None => "\n[web_search]\n".to_string(),
    };
    match &block["content"] {
        Value::Array(results) => {
            for result in results {
                let Some(url) = result["url"].as_str() else {
                    continue;
                };
                let title = result["title"].as_str().unwrap_or(url);
                text.push_str(&format!("- [{title}]({url})\n"));
            }
        }
        error => {
            let code = error["error_code"].as_str()?;
            text.push_str(&format!("- error: {code}\n"));
        }
    }
    text.push('\n');
    Some(text)
}

fn anthropic_tool_name(name: &str) -> String {
    const BUILTINS: [&str; 4] = ["web_search", "code_execution", "text_editor", "computer"];
    if name.starts_with("custom_") || BUILTINS.contains(&name) {
        name.to_string()
    } else {
        format!("custom_{name}")
    }
}

pub fn anthropic_json_to_openai(body: &Value, model: &str, created: i64) -> Value {
    let content = body
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let text = content
        .iter()
        .filter_map(|block| match block["type"].as_str() {
            Some("text") => block["text"].as_str().map(str::to_string),
            Some("web_search_tool_result") => {
                let query = content
                    .iter()
                    .find(|b| b["type"] == "server_tool_use" && b["id"] == block["tool_use_id"])
                    .and_then(|b| b["input"]["query"].as_str());
                web_search_result_text(block, query)
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    let reasoning = content
        .iter()
        .filter(|block| block["type"] == "thinking")
        .filter_map(|block| block["thinking"].as_str())
        .collect::<Vec<_>>()
        .join("");
    let tool_calls: Vec<Value> = content
        .iter()
        .filter(|block| block["type"] == "tool_use")
        .map(|block| {
            json!({
                "id": block["id"],
                "type": "function",
                "function": {
                    "name": block["name"].as_str().map(strip_custom_prefix).unwrap_or("tool"),
                    "arguments": block["input"].to_string(),
                }
            })
        })
        .collect();
    let mut message = json!({"role":"assistant","content":text});
    if !reasoning.is_empty() {
        message["reasoning_content"] = Value::String(reasoning);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let prompt_tokens = body["usage"]["input_tokens"].as_u64().unwrap_or(0);
    let completion_tokens = body["usage"]["output_tokens"].as_u64().unwrap_or(0);
    let cache_read = body["usage"]["cache_read_input_tokens"].as_u64();
    let cache_write = body["usage"]["cache_creation_input_tokens"].as_u64();
    let prompt_tokens = prompt_tokens + cache_read.unwrap_or(0) + cache_write.unwrap_or(0);

    let mut usage = json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt_tokens + completion_tokens,
    });

    if cache_read.is_some() || cache_write.is_some() {
        let mut prompt_tokens_details = serde_json::Map::new();
        if let Some(read) = cache_read {
            usage["cache_read_input_tokens"] = json!(read);
            prompt_tokens_details.insert("cached_tokens".to_string(), json!(read));
        }
        if let Some(write) = cache_write {
            usage["cache_creation_input_tokens"] = json!(write);
            prompt_tokens_details.insert("cache_creation_tokens".to_string(), json!(write));
            prompt_tokens_details.insert("cache_write_tokens".to_string(), json!(write));
        }
        usage["prompt_tokens_details"] = Value::Object(prompt_tokens_details);
    }

    json!({
        "id": body.get("id").cloned().unwrap_or_else(|| json!(format!("chatcmpl-{created}"))),
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": match body["stop_reason"].as_str() {
                Some("max_tokens") => "length",
                Some("tool_use") => "tool_calls",
                _ => "stop",
            },
        }],
        "usage": usage,
    })
}

pub use anthropic_json_to_openai as claude_json_to_openai;

fn system_to_text(system: &Value) -> Option<String> {
    match system {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => Some(
            items
                .iter()
                .filter_map(|i| i.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(""),
        ),
        _ => None,
    }
}

pub fn stop_reason_for(finish: &str) -> &'static str {
    match finish {
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        _ => "end_turn",
    }
}

pub fn messages_payload(
    id: &str,
    model: &str,
    text: &str,
    tool_calls: &[(String, String, String)],
    finish: &str,
    usage: Option<&Usage>,
    reasoning_signature: Option<&str>,
) -> Value {
    messages_payload_full(
        id,
        model,
        text,
        tool_calls,
        finish,
        usage,
        None,
        reasoning_signature,
    )
}

#[allow(clippy::too_many_arguments)] // payload mirrors upstream messages shape
pub fn messages_payload_full(
    id: &str,
    model: &str,
    text: &str,
    tool_calls: &[(String, String, String)],
    finish: &str,
    usage: Option<&Usage>,
    thinking: Option<&str>,
    reasoning_signature: Option<&str>,
) -> Value {
    let mut content: Vec<Value> = Vec::new();
    // Anthropic orders the thinking block ahead of the visible answer.
    if thinking.is_some() || reasoning_signature.is_some() {
        let mut b = json!({ "type": "thinking", "thinking": thinking.unwrap_or("") });
        if let Some(sig) = reasoning_signature {
            b["signature"] = json!(sig);
        }
        content.push(b);
    }
    if !text.is_empty() {
        content.push(json!({ "type": "text", "text": text }));
    }
    for (id, name, args) in tool_calls {
        let input = serde_json::from_str::<Value>(args).unwrap_or_else(|_| json!({}));
        let block_type = if name == "web_search" {
            "server_tool_use"
        } else {
            "tool_use"
        };
        content.push(json!({
            "type": block_type,
            "id": id,
            "name": name,
            "input": input
        }));
    }

    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason_for(finish),
        "stop_sequence": Value::Null,
        "usage": anthropic_usage(usage)
    })
}

/// Anthropic counts tokens server-side; this approximation keeps
/// /v1/messages/count_tokens answerable without spending an upstream call.
/// Deliberately an estimate, not a billing-accurate figure.
pub fn estimate_input_tokens(body: &Value) -> u64 {
    let mut chars = 0usize;
    if let Some(system) = body.get("system").and_then(system_to_text) {
        chars += system.len();
    }
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for msg in messages {
            match msg.get("content") {
                Some(Value::String(s)) => chars += s.len(),
                Some(Value::Array(blocks)) => {
                    for b in blocks {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            chars += t.len();
                        }
                    }
                }
                _ => {}
            }
        }
    }
    ((chars as f64) / 4.0).ceil() as u64
}

pub fn render_anthropic_stream(
    events: &[CodexEvent],
    id: &str,
    model: &str,
) -> (Vec<String>, Option<Usage>) {
    let mut out: Vec<String> = Vec::new();
    let mut usage: Option<Usage> = None;
    let mut opened = false;
    let mut finish = "stop".to_string();

    let final_usage = events.iter().find_map(|e| match e {
        CodexEvent::Completed { usage: Some(u) } => Some(u.clone()),
        _ => None,
    });

    out.push(sse(
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": model,
                "stop_reason": Value::Null,
                "stop_sequence": Value::Null,
                "usage": anthropic_usage(final_usage.as_ref())
            }
        }),
    ));

    for event in events {
        match event {
            CodexEvent::TextDelta(text) => {
                if !opened {
                    opened = true;
                    out.push(sse(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": 0,
                            "content_block": { "type": "text", "text": "" }
                        }),
                    ));
                }
                out.push(sse(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": { "type": "text_delta", "text": text }
                    }),
                ));
            }
            CodexEvent::Completed { usage: u } => {
                usage = u.clone();
            }
            CodexEvent::OutputLimitReached => finish = "length".to_string(),
            CodexEvent::ToolCallBegin { .. } | CodexEvent::ToolArgsDelta { .. }
                if finish != "length" =>
            {
                finish = "tool_calls".to_string();
            }
            CodexEvent::ToolCallBegin { .. } | CodexEvent::ToolArgsDelta { .. } => {}
            _ => {}
        }
    }

    if opened {
        out.push(sse(
            "content_block_stop",
            &json!({ "type": "content_block_stop", "index": 0 }),
        ));
    }

    out.push(sse(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": { "stop_reason": stop_reason_for(&finish), "stop_sequence": Value::Null },
            "usage": { "output_tokens": usage.as_ref().map(|u| u.completion_tokens).unwrap_or(0) }
        }),
    ));
    out.push(sse("message_stop", &json!({ "type": "message_stop" })));

    (out, usage)
}

fn anthropic_usage(usage: Option<&Usage>) -> Value {
    let mut result = json!({"input_tokens": 0, "output_tokens": 0});
    if let Some(usage) = usage {
        result["input_tokens"] = json!(usage
            .prompt_tokens
            .saturating_sub(usage.cached_tokens)
            .saturating_sub(usage.cache_write_tokens));
        result["output_tokens"] = json!(usage.completion_tokens);
        if usage.cached_tokens_known {
            result["cache_read_input_tokens"] = json!(usage.cached_tokens);
        }
        if usage.cache_write_tokens_known {
            result["cache_creation_input_tokens"] = json!(usage.cache_write_tokens);
        }
    }
    result
}

fn sse(event: &str, payload: &Value) -> String {
    format!("event: {event}\ndata: {payload}\n\n")
}

pub struct AnthropicStreamRenderer {
    id: String,
    model: String,
    started: bool,
    current_text_index: Option<u64>,
    terminated: bool,
    tool_index: u64,
    next_content_index: u64,
    tool_indices: std::collections::BTreeMap<u64, u64>,
    closed_tools: std::collections::HashSet<u64>,
    thinking_index: Option<u64>,
    output_limit_reached: bool,
    content_filter_reached: bool,
}

impl AnthropicStreamRenderer {
    pub fn new(model: String, created: i64) -> Self {
        Self {
            id: format!("msg_{created}"),
            model,
            started: false,
            current_text_index: None,
            terminated: false,
            tool_index: 0,
            next_content_index: 0,
            tool_indices: std::collections::BTreeMap::new(),
            closed_tools: std::collections::HashSet::new(),
            thinking_index: None,
            output_limit_reached: false,
            content_filter_reached: false,
        }
    }

    fn frame(event: &str, value: Value) -> bytes::Bytes {
        bytes::Bytes::from(sse(event, &value))
    }

    fn ensure_started(&mut self, out: &mut Vec<bytes::Bytes>) {
        if self.started {
            return;
        }
        self.started = true;
        out.push(Self::frame(
            "message_start",
            json!({
                "type":"message_start",
                "message":{
                    "id":self.id,"type":"message","role":"assistant","content":[],
                    "model":self.model,"stop_reason":Value::Null,"stop_sequence":Value::Null,
                    "usage":{"input_tokens":0,"output_tokens":0}
                }
            }),
        ));
    }

    fn close_text(&mut self, out: &mut Vec<bytes::Bytes>) {
        if let Some(index) = self.current_text_index.take() {
            out.push(Self::frame(
                "content_block_stop",
                json!({"type":"content_block_stop","index":index}),
            ));
        }
    }

    fn close_thinking(&mut self, out: &mut Vec<bytes::Bytes>) {
        if let Some(index) = self.thinking_index.take() {
            out.push(Self::frame(
                "content_block_stop",
                json!({"type":"content_block_stop","index":index}),
            ));
        }
    }

    fn close_open_tools(&mut self, out: &mut Vec<bytes::Bytes>) {
        for (output_index, index) in &self.tool_indices {
            if self.closed_tools.insert(*output_index) {
                out.push(Self::frame(
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":index}),
                ));
            }
        }
    }

    pub fn render(&mut self, event: CodexEvent) -> Vec<bytes::Bytes> {
        if self.terminated {
            return Vec::new();
        }
        let mut out = Vec::new();
        self.ensure_started(&mut out);
        match event {
            CodexEvent::Created { response_id } => {
                if !response_id.is_empty() {
                    self.id = response_id;
                }
            }
            CodexEvent::TextDelta(text) => {
                self.close_thinking(&mut out);
                self.close_open_tools(&mut out);
                let index = if let Some(index) = self.current_text_index {
                    index
                } else {
                    let index = self.next_content_index;
                    self.next_content_index += 1;
                    self.current_text_index = Some(index);
                    out.push(Self::frame(
                        "content_block_start",
                        json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
                    ));
                    index
                };
                out.push(Self::frame(
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}}),
                ));
            }
            CodexEvent::ReasoningDelta(text) => {
                let index = self.ensure_thinking_block(&mut out);
                out.push(Self::frame(
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":index,"delta":{"type":"thinking_delta","thinking":text}}),
                ));
            }
            CodexEvent::ReasoningSignature(signature) => {
                let index = self.ensure_thinking_block(&mut out);
                out.push(Self::frame(
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":index,"delta":{"type":"signature_delta","signature":signature}}),
                ));
            }
            // Redacted reasoning has no Anthropic wire representation without
            // upstream redacted data; the marker only keeps the state explicit.
            // Redacted reasoning stays out of the Claude wire, and the Kiro
            // opaque blob is replayed only through its own adapter.
            CodexEvent::ReasoningRedacted | CodexEvent::KiroRedactedReasoning(_) => {}
            CodexEvent::ToolCallBegin {
                call_id,
                name,
                output_index,
            } => {
                self.close_thinking(&mut out);
                self.close_text(&mut out);
                let index = self.next_content_index;
                self.next_content_index += 1;
                self.tool_index += 1;
                self.tool_indices.insert(output_index, index);
                out.push(Self::frame(
                    "content_block_start",
                    json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":call_id,"name":name,"input":{}}}),
                ));
            }
            CodexEvent::ToolArgsDelta {
                delta,
                output_index,
            } => {
                let index = self.tool_indices.get(&output_index).copied().unwrap_or(1);
                out.push(Self::frame(
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":delta}}),
                ));
            }
            CodexEvent::OutputLimitReached => self.output_limit_reached = true,
            CodexEvent::ContentFilterReached => self.content_filter_reached = true,
            CodexEvent::Completed { usage } => {
                self.close_thinking(&mut out);
                self.close_text(&mut out);
                self.close_open_tools(&mut out);
                let stop = if self.content_filter_reached {
                    "refusal"
                } else if self.output_limit_reached {
                    "max_tokens"
                } else if self.tool_index > 0 {
                    "tool_use"
                } else {
                    "end_turn"
                };
                out.push(Self::frame(
                    "message_delta",
                    // message_start went out before the provider reported usage, so the
                    // final input count is carried here (Anthropic clients merge
                    // message_delta usage into the message).
                    json!({"type":"message_delta","delta":{"stop_reason":stop,"stop_sequence":Value::Null},"usage":anthropic_usage(usage.as_ref())}),
                ));
                out.push(Self::frame("message_stop", json!({"type":"message_stop"})));
                self.terminated = true;
            }
            CodexEvent::Failed { message } => {
                self.close_thinking(&mut out);
                self.close_text(&mut out);
                self.close_open_tools(&mut out);
                out.push(Self::frame(
                    "error",
                    json!({"type":"error","error":{"type":"api_error","message":message}}),
                ));
                self.terminated = true;
            }
        }
        out
    }

    fn ensure_thinking_block(&mut self, out: &mut Vec<bytes::Bytes>) -> u64 {
        if let Some(index) = self.thinking_index {
            return index;
        }
        self.close_open_tools(out);
        self.close_text(out);
        let index = self.next_content_index;
        self.next_content_index += 1;
        self.thinking_index = Some(index);
        out.push(Self::frame(
            "content_block_start",
            json!({"type":"content_block_start","index":index,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        ));
        index
    }

    pub fn close_unterminated(&mut self) -> Vec<bytes::Bytes> {
        if self.terminated {
            return Vec::new();
        }
        self.render(CodexEvent::Failed {
            message: "upstream stream ended before completion".into(),
        })
    }

    pub fn terminated(&self) -> bool {
        self.terminated
    }

    pub fn tick(&mut self) -> Bytes {
        if self.terminated {
            return Bytes::new();
        }
        let mut out = Vec::new();
        self.ensure_started(&mut out);
        out.push(Bytes::from_static(
            b"event: ping\ndata: {\"type\":\"ping\"}\n\n",
        ));
        out.concat().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse_value(frame: &str) -> Value {
        serde_json::from_str(
            frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn anthropic_cache_usage_is_inclusive_json_and_stream() {
        let usage = json!({"input_tokens":100,"cache_read_input_tokens":40,"cache_creation_input_tokens":60,"output_tokens":10});
        let response = json!({"id":"msg_cache","content":[],"usage":usage});
        let converted = claude_json_to_openai(&response, "fixture", 1);
        assert_eq!(converted["usage"]["prompt_tokens"], 200);
        assert_eq!(converted["usage"]["total_tokens"], 210);
        assert_eq!(
            converted["usage"]["prompt_tokens_details"]["cached_tokens"],
            40
        );
        assert_eq!(
            converted["usage"]["prompt_tokens_details"]["cache_write_tokens"],
            60
        );
        let mut decoder = AnthropicDecoder::new();
        let mut events = Vec::new();
        for frame in [
            json!({"type":"message_start","message":{"id":"msg_cache","usage":usage}}),
            json!({"type":"message_delta","usage":{"output_tokens":10}}),
            json!({"type":"message_stop"}),
        ] {
            decoder.decode(&serde_json::to_vec(&frame).unwrap(), &mut events);
        }
        let mut renderer = super::super::render::ChunkRenderer::new("fixture".into(), 1, true);
        let frames: Vec<_> = events
            .into_iter()
            .flat_map(|event| renderer.render(event))
            .collect();
        let rendered = frames
            .iter()
            .find_map(|frame| {
                let text = std::str::from_utf8(frame).unwrap();
                text.lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    .filter(|value| value["usage"].is_object())
            })
            .unwrap();
        assert_eq!(rendered["usage"]["prompt_tokens"], 200);
        assert_eq!(rendered["usage"]["total_tokens"], 210);
        assert_eq!(
            rendered["usage"]["prompt_tokens_details"]["cached_tokens"],
            40
        );
        assert_eq!(
            rendered["usage"]["prompt_tokens_details"]["cache_write_tokens"],
            60
        );
    }

    #[test]
    fn anthropic_render_usage_excludes_known_caches() {
        let usage = Usage {
            prompt_tokens: 200,
            completion_tokens: 10,
            total_tokens: 210,
            cached_tokens: 40,
            cached_tokens_known: true,
            cache_write_tokens: 60,
            cache_write_tokens_known: true,
            reasoning_tokens: 0,
        };
        let expected = json!({"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":40,"cache_creation_input_tokens":60});
        let payload = messages_payload_full(
            "msg",
            "fixture",
            "answer",
            &[],
            "stop",
            Some(&usage),
            None,
            None,
        );
        assert_eq!(payload["usage"], expected);
        let events = vec![CodexEvent::Completed {
            usage: Some(usage.clone()),
        }];
        let (frames, _) = render_anthropic_stream(&events, "msg", "fixture");
        assert_eq!(sse_value(&frames[0])["message"]["usage"], expected);
        let mut renderer = AnthropicStreamRenderer::new("fixture".into(), 1);
        let frames = renderer.render(events.into_iter().next().unwrap());
        let terminal = frames
            .iter()
            .map(|frame| sse_value(std::str::from_utf8(frame).unwrap()))
            .find(|value| value["type"] == "message_delta")
            .unwrap();
        assert_eq!(terminal["usage"], expected);
        let zero = Usage {
            cached_tokens_known: true,
            cache_write_tokens_known: true,
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
        };
        let unknown = Usage {
            cached_tokens_known: false,
            cache_write_tokens_known: false,
            ..zero.clone()
        };
        assert_eq!(anthropic_usage(Some(&zero))["cache_read_input_tokens"], 0);
        assert_eq!(
            anthropic_usage(Some(&zero))["cache_creation_input_tokens"],
            0
        );
        assert!(anthropic_usage(Some(&unknown))
            .get("cache_read_input_tokens")
            .is_none());
        assert!(anthropic_usage(Some(&unknown))
            .get("cache_creation_input_tokens")
            .is_none());
        let over = Usage {
            prompt_tokens: 1,
            ..usage
        };
        assert_eq!(anthropic_usage(Some(&over))["input_tokens"], 0);
    }

    #[test]
    fn claude_stop_controls_cross_map() {
        for stop in [json!("END"), json!(["END", "DONE"])] {
            let body = json!({"model":"fixture","messages":[],"stop":stop});
            let converted = openai_to_anthropic(&body).unwrap();
            let expected = if stop.is_string() {
                json!([stop])
            } else {
                stop
            };
            assert_eq!(converted["stop_sequences"], expected);
            assert_eq!(anthropic_to_openai(&converted).unwrap()["stop"], expected);
        }
        let body = json!({"model":"fixture","messages":[],"stop":null,"stop_sequences":["legacy"]});
        assert_eq!(
            openai_to_anthropic(&body).unwrap()["stop_sequences"],
            json!(["legacy"])
        );
        assert!(
            openai_to_anthropic(&json!({"model":"fixture","messages":[],"stop":null}))
                .unwrap()
                .get("stop_sequences")
                .is_none()
        );
    }

    #[test]
    fn claude_honors_max_completion_tokens() {
        let mut body = json!({"model":"fixture","messages":[],"reasoning_effort":"none","max_completion_tokens":64});
        assert_eq!(openai_to_anthropic(&body).unwrap()["max_tokens"], 64);
        body["max_tokens"] = json!(32);
        assert_eq!(openai_to_anthropic(&body).unwrap()["max_tokens"], 32);
        body.as_object_mut().unwrap().remove("max_tokens");
        body.as_object_mut()
            .unwrap()
            .remove("max_completion_tokens");
        assert_eq!(openai_to_anthropic(&body).unwrap()["max_tokens"], 32768);
    }

    #[test]
    fn anthropic_url_images_survive_chat_translation() {
        let image =
            json!({"type":"image","source":{"type":"url","url":"https://example.org/a.png"}});
        let body = json!({"model":"fixture","messages":[
            {"role":"user","content":[image,{"type":"text","text":"describe"}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":[image]}]}
        ]});
        let translated = anthropic_to_openai(&body).unwrap();
        assert_eq!(
            translated["messages"][0]["content"][0],
            json!({"type":"text","text":"describe"})
        );
        let expected = json!({"type":"image_url","image_url":{"url":"https://example.org/a.png"}});
        assert_eq!(translated["messages"][0]["content"][1], expected);
        assert_eq!(translated["messages"][1]["content"][0], expected);
    }

    #[test]
    fn anthropic_tool_results_precede_user_followup() {
        let body = json!({"model":"fixture","messages":[
            {"role":"assistant","content":[{"type":"tool_use","id":"x","name":"read","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":"result"},{"type":"text","text":"continue"}]}
        ]});
        let translated = anthropic_to_openai(&body).unwrap();
        let messages = translated["messages"].as_array().unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|message| message["role"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["assistant", "tool", "user"]
        );
        assert_eq!(messages[1]["tool_call_id"], "x");
        assert_eq!(messages[1]["content"], "result");
        assert_eq!(messages[2]["content"], "continue");
    }

    #[test]
    fn anthropic_thinking_controls_survive_codex_plan() {
        for (thinking, config, expected) in [
            (json!({"type":"disabled"}), json!({"effort":"high"}), "none"),
            (json!({"type":"adaptive"}), json!({"effort":"high"}), "high"),
            (
                json!({"type":"enabled","budget_tokens":8192}),
                json!({}),
                "medium",
            ),
        ] {
            let body = json!({"model":"gpt-5","messages":[{"role":"user","content":"hi"}],"thinking":thinking,"output_config":config});
            let translated = anthropic_to_openai(&body).unwrap();
            let plan =
                super::super::request::openai_to_codex(&serde_json::to_vec(&translated).unwrap())
                    .unwrap();
            let upstream: Value = serde_json::from_slice(&plan.body).unwrap();
            assert_eq!(upstream["reasoning"]["effort"], expected);
        }
        for kind in ["adaptive", "enabled"] {
            assert_eq!(
                anthropic_to_openai(
                    &json!({"model":"fixture","messages":[],"thinking":{"type":kind}})
                )
                .unwrap()["reasoning_effort"],
                "auto"
            );
        }
    }

    #[test]
    fn claude_replays_signed_assistant_reasoning_before_tools() {
        // Captured Claude fable-5 CAIS sample from the reference signature corpus.
        let signature = "CAISqwIKiAEIEBgCKkBHRlRBsNiptQUWfPoOhuQKwi5LnncZVO9bB5jqOs76D7uBtgktML0zqJtNmLHXHHcgD6lk4MQu4QBXzFd1lbC3Mg5jbGF1ZGUtZmFibGUtNTgBQgh0aGlua2luZ1okZDk3NDM5NzUtNGJiMC00OTM2LTllMjgtZDViMGQyMWJkYzQ4EgxCGh+XVFFFeySAjtAaDL/A1LltGu6MMJ+eXSIwsN0oBpDrqLv22UBfkMnTotnIbkvkOyb9xZHgigG6OZVHaI3gThm+maLKmgO5PrFLKlDFYp+YZksy/wKwszJlnLTPzAK+NUlfzagOE1ymtZTXhAYK260XyFYmg/te/C231+Fr/hoX+EJoUBnrn0gD7hqMISOT+TaFEuOXYsN517GfaxgB";
        let response = json!({"model":"fixture","input":[
            {"type":"message","role":"assistant","content":[],"reasoning_content":"reason","reasoning_signature":signature},
            {"type":"function_call","call_id":"x","name":"read","arguments":"{}"}
        ]});
        let chat = super::super::responses::responses_to_openai(&response).unwrap();
        let replay = openai_to_anthropic(&chat).unwrap();
        assert_eq!(
            replay["messages"][0]["content"][0],
            json!({"type":"thinking","thinking":"reason","signature":signature})
        );
        let blocks: Vec<_> = replay["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| message["content"].as_array().unwrap())
            .collect();
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[1]["type"], "tool_use");
        for sig in [
            Value::Null,
            json!("gAAAAforeignEncryptedContent"),
            json!("call_not_a_claude_signature"),
            json!("unsigned"),
        ] {
            let request = json!({"model":"fixture","messages":[{"role":"assistant","content":"","reasoning_content":"reason","reasoning_signature":sig}]});
            assert!(openai_to_anthropic(&request).unwrap()["messages"]
                .as_array()
                .unwrap()
                .is_empty());
        }
        let thinking_only = json!({"model":"fixture","messages":[{"role":"assistant","content":"","reasoning_content":"reason","reasoning_signature":signature}]});
        assert_eq!(
            openai_to_anthropic(&thinking_only).unwrap()["messages"][0]["content"][0]["type"],
            "thinking"
        );
        let redacted = json!({"model":"fixture","messages":[{"role":"assistant","content":"","reasoning_redacted":true,"reasoning_signature":"opaque-redacted-data"}]});
        assert_eq!(
            openai_to_anthropic(&redacted).unwrap()["messages"][0]["content"][0],
            json!({"type":"redacted_thinking","data":"opaque-redacted-data"})
        );
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    #[test]
    fn later_system_instructions_preserve_the_cached_history_prefix() {
        let initial = json!({
            "model": "fixture",
            "messages": [
                {"role": "system", "content": "base instruction"},
                {"role": "developer", "content": "initial policy"},
                {"role": "user", "content": "question"},
                {"role": "assistant", "content": "answer"},
            ],
        });
        let before = openai_to_anthropic(&initial).unwrap();
        for role in ["system", "developer"] {
            let mut next = initial.clone();
            next["messages"].as_array_mut().unwrap().push(json!({
                "role": role,
                "content": "new instruction",
            }));
            let after = openai_to_anthropic(&next).unwrap();
            assert_eq!(after["system"], before["system"]);
            assert_eq!(
                &after["messages"].as_array().unwrap()[..2],
                before["messages"].as_array().unwrap()
            );
            assert_eq!(after["messages"][2]["role"], "user");
            assert_eq!(
                after["messages"][2]["content"][0]["text"],
                "new instruction"
            );
        }
    }

    #[test]
    fn mid_conversation_effort_emits_a_system_message_for_supported_models() {
        let body = json!({
            "model": "claude-opus-5-5",
            "messages": [
                {"role": "user", "content": "question"},
                {"role": "assistant", "content": "answer"},
                {"role": "system", "content": "", "x_effort_update": "high"},
                {"role": "user", "content": "follow up"},
            ],
        });
        let out = openai_to_anthropic(&body).unwrap();
        let messages = out["messages"].as_array().unwrap();
        // The top-level effort is untouched: the switch rides in-band so the
        // cached history prefix survives.
        assert!(out.get("output_config").is_none());
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[2]["role"], "system");
        assert_eq!(messages[2]["content"], json!([]));
        assert_eq!(messages[2]["output_config"]["effort"], "high");
        assert!(uses_mid_conversation_effort(&out));
    }

    #[test]
    fn mid_conversation_effort_without_a_supported_model_uses_top_level_effort() {
        let body = json!({
            "model": "claude-sonnet-4-5-2",
            "messages": [
                {"role": "user", "content": "question"},
                {"role": "system", "content": "", "x_effort_update": "high"},
                {"role": "user", "content": "follow up"},
            ],
        });
        let out = openai_to_anthropic(&body).unwrap();
        assert_eq!(out["output_config"]["effort"], "high");
        assert!(
            !uses_mid_conversation_effort(&out),
            "no beta-gated message may reach an unsupported model"
        );
        assert!(out["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|message| message["role"] != "system"));
    }

    #[test]
    fn mid_conversation_effort_model_gate_matches_documented_ids() {
        for model in [
            "claude-fable-5-1",
            "claude-mythos-5-1",
            "claude-opus-5-5",
            "claude-opus-5",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
            "anthropic/claude-opus-5-5",
        ] {
            assert!(supports_mid_conversation_effort(model), "{model}");
        }
        for model in [
            "claude-opus-5-50",
            "claude-sonnet-4-5-2",
            "claude-haiku-4-5-2",
            "gpt-6-astra",
        ] {
            assert!(!supports_mid_conversation_effort(model), "{model}");
        }
    }

    #[test]
    fn byte_scan_matches_the_parsed_check() {
        let with_effort = json!({
            "messages": [
                {"role": "user", "content": "q"},
                {"role": "system", "content": [], "output_config": {"effort": "high"}},
            ],
        });
        assert!(uses_mid_conversation_effort_bytes(
            with_effort.to_string().as_bytes()
        ));
        let without_effort = json!({
            "messages": [{"role": "user", "content": "q"}],
            "output_config": {"effort": "high"},
        });
        assert!(!uses_mid_conversation_effort_bytes(
            without_effort.to_string().as_bytes()
        ));
        assert!(!uses_mid_conversation_effort_bytes(b"not json"));
    }

    #[test]
    fn cache_breakpoint_lands_before_a_trailing_effort_message() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": "question"},
                {"role": "assistant", "content": [{"type": "text", "text": "answer"}]},
                {"role": "system", "content": [], "output_config": {"effort": "high"}},
            ],
        });
        apply_prompt_cache_breakpoints(&mut body);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(
            messages[1]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert!(messages[2].get("cache_control").is_none());
    }

    #[test]
    fn r23_blocks_allocate_from_zero_and_never_reuse_indices() {
        for events in [
            vec![
                CodexEvent::ReasoningDelta("think".into()),
                CodexEvent::TextDelta("answer".into()),
                CodexEvent::ToolCallBegin {
                    output_index: 0,
                    call_id: "a".into(),
                    name: "lookup".into(),
                },
            ],
            vec![CodexEvent::ToolCallBegin {
                output_index: 0,
                call_id: "a".into(),
                name: "lookup".into(),
            }],
            vec![
                CodexEvent::TextDelta("before".into()),
                CodexEvent::ToolCallBegin {
                    output_index: 0,
                    call_id: "a".into(),
                    name: "lookup".into(),
                },
                CodexEvent::TextDelta("after".into()),
                CodexEvent::ReasoningSignature("sig".into()),
                CodexEvent::TextDelta("end".into()),
            ],
        ] {
            let expected_blocks = events.len();
            let mut renderer = AnthropicStreamRenderer::new("fixture".into(), 1);
            let mut frames = Vec::new();
            for event in events {
                let tool = matches!(&event, CodexEvent::ToolCallBegin { .. });
                frames.extend(renderer.render(event));
                if tool {
                    frames.extend(renderer.render(CodexEvent::ToolArgsDelta {
                        output_index: 0,
                        delta: "{}".into(),
                    }));
                }
            }
            frames.extend(renderer.close_unterminated());
            assert!(renderer.close_unterminated().is_empty());
            let mut next = 0;
            let mut open = None;
            let mut stops = 0;
            for frame in frames {
                let raw = std::str::from_utf8(&frame).unwrap();
                let payload: Value =
                    serde_json::from_str(raw.split_once("data: ").unwrap().1.trim()).unwrap();
                match payload["type"].as_str().unwrap() {
                    "content_block_start" => {
                        assert_eq!(open, None);
                        assert_eq!(payload["index"], next);
                        open = Some(next);
                        next += 1;
                    }
                    "content_block_delta" => assert_eq!(payload["index"].as_u64(), open),
                    "content_block_stop" => {
                        assert!(open.is_some());
                        assert_eq!(payload["index"].as_u64(), open.take());
                        stops += 1;
                    }
                    _ => {}
                }
            }
            assert_eq!(next as usize, expected_blocks);
            assert_eq!(stops, expected_blocks);
            assert_eq!(open, None);
        }
    }

    #[test]
    fn r24_history_names_match_definitions_without_double_prefix() {
        for (name, expected) in [
            ("lookup", "custom_lookup"),
            ("custom_lookup", "custom_lookup"),
            ("web_search", "web_search"),
        ] {
            let out = openai_to_anthropic(&json!({
                "model":"fixture", "tools":[{"type":"function","function":{"name":name}}],
                "messages":[{"role":"assistant","tool_calls":[{"id":"a","function":{"name":name,"arguments":"{}"}}]},
                    {"role":"tool","tool_call_id":"a","content":"result"}]
            })).unwrap();
            assert_eq!(out["tools"][0]["name"], expected);
            assert_eq!(out["messages"][0]["content"][0]["name"], expected);
            assert_eq!(out["messages"][1]["content"][0]["tool_use_id"], "a");
        }
    }

    #[test]
    fn r28_bidirectional_tool_choice_and_parallel_semantics() {
        for (openai, anthropic, native_choice) in [
            (json!("auto"), json!({"type":"auto"}), json!("auto")),
            (json!("none"), json!({"type":"none"}), json!("none")),
            (json!("required"), json!({"type":"any"}), json!("required")),
            (
                json!({"type":"function","function":{"name":"lookup"}}),
                json!({"type":"tool","name":"custom_lookup"}),
                json!({"type":"function","function":{"name":"custom_lookup"}}),
            ),
            (
                json!({"type":"function","function":{"name":"web_search"}}),
                json!({"type":"tool","name":"web_search"}),
                json!({"type":"function","function":{"name":"web_search"}}),
            ),
        ] {
            for parallel in [None, Some(false), Some(true)] {
                let mut input = json!({"model":"fixture","messages":[],"tool_choice":openai});
                let mut expected = anthropic.clone();
                if let Some(parallel) = parallel {
                    input["parallel_tool_calls"] = json!(parallel);
                    expected["disable_parallel_tool_use"] = json!(!parallel);
                }
                let out = openai_to_anthropic(&input).unwrap();
                assert_eq!(out["tool_choice"], expected);
                let back = anthropic_to_openai(&out).unwrap();
                assert_eq!(back["tool_choice"], native_choice);
                assert_eq!(
                    back.get("parallel_tool_calls"),
                    input.get("parallel_tool_calls")
                );
            }
        }
        let out = openai_to_anthropic(
            &json!({"model":"fixture","messages":[],"parallel_tool_calls":false}),
        )
        .unwrap();
        assert_eq!(
            out["tool_choice"],
            json!({"type":"auto","disable_parallel_tool_use":true})
        );
        let out = openai_to_anthropic(&json!({"model":"fixture","messages":[],"tool_choice":{"type":"function","function":{"name":"custom_lookup"}}})).unwrap();
        assert_eq!(out["tool_choice"]["name"], "custom_lookup");
        assert!(openai_to_anthropic(
            &json!({"model":"fixture","messages":[],"tool_choice":"invalid"})
        )
        .is_err());
        assert!(anthropic_to_openai(
            &json!({"model":"fixture","messages":[],"tool_choice":{"type":"invalid"}})
        )
        .is_err());
    }

    #[test]
    fn omitted_max_tokens_defaults_generously() {
        // Agent clients omit max_tokens; a small default truncates reasoning
        // models mid tool_use and stalls the agent loop (gateway.log showed
        // output pinned at exactly 4096 with 200 KB requests).
        let translated = openai_to_anthropic(&json!({
            "model":"claude-sonnet-4-6",
            "messages":[{"role":"user","content":"Hello"}]
        }))
        .expect("translation");
        assert_eq!(translated["max_tokens"], 32_768);

        // Explicit client values still win.
        let explicit = openai_to_anthropic(&json!({
            "model":"claude-sonnet-4-6",
            "max_tokens": 128,
            "messages":[{"role":"user","content":"Hello"}]
        }))
        .expect("translation");
        assert_eq!(explicit["max_tokens"], 128);
    }

    #[test]
    fn system_content_parts_are_preserved() {
        let translated = openai_to_anthropic(&json!({
            "model":"claude-sonnet-4-6",
            "messages":[
                {"role":"system","content":[{"type":"text","text":"Be concise."}]},
                {"role":"user","content":"Hello"}
            ]
        }))
        .expect("translation");
        assert_eq!(translated["system"], "Be concise.");
    }

    #[test]
    fn consecutive_tool_results_share_one_user_turn() {
        let translated = openai_to_anthropic(&json!({
            "model":"claude-sonnet-4-6",
            "messages":[
                {"role":"assistant","content":"","tool_calls":[
                    {"id":"call_1","type":"function","function":{"name":"one","arguments":"{}"}},
                    {"id":"call_2","type":"function","function":{"name":"two","arguments":"{}"}}
                ]},
                {"role":"tool","tool_call_id":"call_1","content":"first"},
                {"role":"tool","tool_call_id":"call_2","content":"second"}
            ]
        }))
        .expect("translation");
        let messages = translated["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"].as_array().expect("blocks").len(), 2);
    }

    #[test]
    fn image_parts_translate_to_anthropic_blocks() {
        let translated = openai_to_anthropic(&json!({
            "model":"claude-sonnet-4-6",
            "messages":[{"role":"user","content":[
                {"type":"text","text":"Describe"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo="}}
            ]}]
        }))
        .expect("translation");
        let image = &translated["messages"][0]["content"][1];
        assert_eq!(image["type"], "image");
        assert_eq!(image["source"]["type"], "base64");
        assert_eq!(image["source"]["media_type"], "image/png");
        assert_eq!(image["source"]["data"], "iVBORw0KGgo=");
    }

    #[test]
    fn anthropic_tool_result_images_survive_codex_request_normalization() {
        let anthropic = json!({
            "model":"claude-sonnet-4-6",
            "messages":[
                {"role":"user","content":[
                    {"type":"text","text":"first"},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":"FIRST"}}
                ]},
                {"role":"assistant","content":[{"type":"tool_use","id":"toolu_native_1","name":"take_screenshot","input":{}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_native_1","content":[
                    {"type":"text","text":"Native tool result text"},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":"SECOND"}}
                ]}]}
            ]
        });
        let openai = anthropic_to_openai(&anthropic).expect("Anthropic normalization");
        let openai_bytes = serde_json::to_vec(&openai).expect("OpenAI JSON");
        let request =
            super::super::request::openai_to_codex(&openai_bytes).expect("Codex normalization");
        let body: Value = serde_json::from_slice(&request.body).expect("Codex JSON");
        let input = body["input"].as_array().expect("input");
        assert_eq!(input.len(), 4);
        assert_eq!(input[0]["content"][1]["type"], "input_image");
        assert_eq!(
            input[0]["content"][1]["image_url"],
            "data:image/png;base64,FIRST"
        );
        assert_eq!(input[1]["call_id"], "toolu_native_1");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], input[1]["call_id"]);
        assert_eq!(input[2]["output"], "Native tool result text");
        // The relayed tool image follows as a user turn whose image part sits
        // after a leading text part (the multimodal wire contract, Task 4).
        assert_eq!(input[3]["role"], "user");
        assert_eq!(input[3]["content"][0]["type"], "input_text");
        assert_eq!(input[3]["content"][1]["type"], "input_image");
        assert_eq!(
            input[3]["content"][1]["image_url"],
            "data:image/png;base64,SECOND"
        );
    }

    #[test]
    fn thinking_delta_is_not_visible_text() {
        let mut decoder = AnthropicDecoder::new();
        let mut events = Vec::new();
        decoder.decode(
            br#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"internal"}}"#,
            &mut events,
        );
        assert!(!events
            .iter()
            .any(|event| matches!(event, CodexEvent::TextDelta(text) if text == "internal")));
    }

    #[test]
    fn signature_delta_opens_a_thinking_block() {
        let mut renderer = AnthropicStreamRenderer::new("claude-sonnet-4-6".into(), 1);
        let output = renderer.render(CodexEvent::ReasoningSignature("opaque".into()));
        let joined = output
            .iter()
            .map(|chunk| String::from_utf8_lossy(chunk))
            .collect::<String>();
        assert!(joined.contains("\"type\":\"thinking\""), "{joined}");
        assert!(joined.contains("\"type\":\"signature_delta\""), "{joined}");
    }

    #[test]
    fn custom_prefix_is_stripped_exactly_once() {
        assert_eq!(strip_custom_prefix("custom_search"), "search");
        assert_eq!(strip_custom_prefix("custom_custom_y"), "custom_y");
        assert_eq!(strip_custom_prefix("customer_search"), "customer_search");
        assert_eq!(strip_custom_prefix("mcp_tool"), "mcp_tool");
        assert_eq!(strip_custom_prefix("web_search"), "web_search");
    }

    #[test]
    fn stream_blocks_pair_start_and_stop_across_transitions() {
        // End-to-end: an upstream Anthropic SSE stream (with a kiro-style
        // double-prefixed tool name) is decoded and re-rendered; the downstream
        // stream must stay well-paired and preserve the tool names.
        let mut decoder = AnthropicDecoder::default();
        let mut events: Vec<CodexEvent> = Vec::new();
        for frame in [
            json!({"type":"message_start","message":{"id":"msg_up","usage":{"input_tokens":3}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"pondering"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hello"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call_1","name":"custom_custom_y","input":{}}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"call_2","name":"customer_search","input":{}}}),
            json!({"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
            json!({"type":"message_delta","usage":{"output_tokens":2}}),
            json!({"type":"message_stop"}),
        ] {
            decoder.decode(frame.to_string().as_bytes(), &mut events);
        }
        assert!(matches!(events.last(), Some(CodexEvent::Completed { .. })));

        let mut renderer = AnthropicStreamRenderer::new("claude-sonnet-4-6".into(), 1);
        let mut joined = String::new();
        for event in events {
            for frame in renderer.render(event) {
                joined.push_str(&String::from_utf8_lossy(&frame));
            }
        }
        for frame in renderer.close_unterminated() {
            joined.push_str(&String::from_utf8_lossy(&frame));
        }

        // Walk the SSE stream and assert every content block is opened once,
        // written only while open, and stopped before the next start.
        let mut open: std::collections::BTreeMap<u64, String> = std::collections::BTreeMap::new();
        let mut started_names: Vec<String> = Vec::new();
        let mut stop_reason = String::new();
        for frame in joined.split("\n\n") {
            let mut lines = frame.lines();
            let event = lines.next().unwrap_or_default().strip_prefix("event: ");
            let payload: Value = match lines.next().and_then(|line| line.strip_prefix("data: ")) {
                Some(body) => serde_json::from_str(body).expect("valid frame payload"),
                None => continue,
            };
            match event {
                Some("content_block_start") => {
                    let index = payload["index"].as_u64().expect("block index");
                    let kind = payload["content_block"]["type"]
                        .as_str()
                        .expect("block type");
                    assert!(
                        open.insert(index, kind.to_string()).is_none(),
                        "index {index} started while already open ({kind})"
                    );
                    if let Some(name) = payload["content_block"]["name"].as_str() {
                        started_names.push(name.to_string());
                    }
                }
                Some("content_block_delta") => {
                    let index = payload["index"].as_u64().expect("block index");
                    assert!(
                        open.contains_key(&index),
                        "delta for unopened index {index}"
                    );
                }
                Some("content_block_stop") => {
                    let index = payload["index"].as_u64().expect("block index");
                    assert!(
                        open.remove(&index).is_some(),
                        "stop for unopened index {index}"
                    );
                }
                Some("message_delta") => {
                    stop_reason = payload["delta"]["stop_reason"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                }
                _ => {}
            }
        }
        assert!(open.is_empty(), "blocks never stopped: {open:?}");
        assert_eq!(stop_reason, "tool_use");
        assert_eq!(started_names, vec!["custom_y", "customer_search"]);
    }

    #[test]
    fn tool_prefixing_is_idempotent_and_preserves_builtins() {
        let translated = openai_to_anthropic(&json!({
            "model":"claude-sonnet-4-6",
            "messages":[{"role":"user","content":"search"}],
            "tools":[
                {"type":"function","function":{"name":"web_search","description":"search"}},
                {"type":"function","function":{"name":"custom_editor","description":"edit"}}
            ]
        }))
        .expect("translation");
        assert_eq!(translated["tools"][0]["name"], "web_search");
        assert_eq!(translated["tools"][1]["name"], "custom_editor");
    }

    #[test]
    fn reasoning_effort_maps_to_the_upstream_thinking_budget() {
        let body = json!({
            "model": "glm-5.3-flash",
            "reasoning_effort": "low",
            "max_tokens": 4096,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = openai_to_anthropic(&body).unwrap();
        assert_eq!(
            out["thinking"],
            json!({"type": "enabled", "budget_tokens": 1024})
        );
        assert_eq!(out["max_tokens"], json!(4096));

        let body = json!({
            "model": "glm-5.3-flash",
            "reasoning_effort": "max",
            "max_tokens": 4096,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = openai_to_anthropic(&body).unwrap();
        assert_eq!(
            out["thinking"],
            json!({"type": "enabled", "budget_tokens": 4095})
        );
        assert_eq!(out["max_tokens"], json!(4096));
    }

    #[test]
    fn a_tiny_max_tokens_rejects_incompatible_thinking() {
        let body = json!({
            "model": "glm-5.3-flash",
            "reasoning_effort": "high",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hi"}]
        });
        assert!(openai_to_anthropic(&body).is_err());
    }

    #[test]
    fn anthropic_json_to_openai_preserves_thinking_as_reasoning_content() {
        let body = json!({
            "id": "msg_123",
            "content": [
                {"type": "thinking", "thinking": "Let me ponder."},
                {"type": "text", "text": "Here is the answer."}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 20}
        });
        let out = anthropic_json_to_openai(&body, "glm-5.3-flash", 1234567890);
        let msg = &out["choices"][0]["message"];
        assert_eq!(msg["content"], "Here is the answer.");
        assert_eq!(msg["reasoning_content"], "Let me ponder.");
    }

    #[test]
    fn empty_stream_keeps_upstream_stop_reason_and_missing_output_tokens() {
        let mut decoder = ClaudeDecoder::new();
        let mut events = Vec::new();
        decoder.decode(
            br#"{"type":"message_start","message":{"id":"msg_empty","usage":{"input_tokens":4}}}"#,
            &mut events,
        );
        decoder.decode(
            br#"{"type":"message_delta","delta":{"stop_reason":"refusal"},"usage":{}}"#,
            &mut events,
        );
        decoder.decode(br#"{"type":"message_stop"}"#, &mut events);

        let d = decoder.diagnostics();
        assert_eq!(d.message_id.as_deref(), Some("msg_empty"));
        assert_eq!(d.stop_reason.as_deref(), Some("refusal"));
        assert!(!d.output_tokens_reported);
        assert!(!d.has_visible_output());
    }

    #[test]
    fn diagnostics_count_content_and_reset_per_message() {
        let mut decoder = ClaudeDecoder::new();
        let mut events = Vec::new();
        for frame in [
            json!({"type":"message_start","message":{"id":"msg_a","usage":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hi"}}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t","name":"x","input":{}}}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":0}}),
            json!({"type":"message_stop"}),
        ] {
            decoder.decode(frame.to_string().as_bytes(), &mut events);
        }
        let d = decoder.diagnostics().clone();
        assert_eq!((d.text_deltas, d.thinking_deltas, d.tool_calls), (1, 1, 1));
        assert!(d.output_tokens_reported);
        assert!(d.has_visible_output());

        decoder.decode(
            br#"{"type":"message_start","message":{"id":"msg_b","usage":{}}}"#,
            &mut events,
        );
        assert_eq!(
            decoder.diagnostics(),
            &StreamDiagnostics {
                message_id: Some("msg_b".into()),
                ..StreamDiagnostics::default()
            }
        );
    }

    #[test]
    fn server_web_search_never_surfaces_as_a_client_tool_call() {
        // CCAPI streams the server tool block's start before its arguments;
        // the block runs upstream, so the client must see only text.
        let mut decoder = ClaudeDecoder::new();
        let mut events = Vec::new();
        for frame in [
            json!({"type":"message_start","message":{"id":"msg_ws","usage":{}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\": \"rust"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":" stable\"}"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[{"type":"web_search_result","title":"Rust 1.99.1","url":"https://blog.rust-lang.org/"}]}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"1.99.1"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}),
            json!({"type":"message_stop"}),
        ] {
            decoder.decode(frame.to_string().as_bytes(), &mut events);
        }
        assert!(!events.iter().any(|e| matches!(
            e,
            CodexEvent::ToolCallBegin { .. } | CodexEvent::ToolArgsDelta { .. }
        )));
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                CodexEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.contains("[web_search: rust stable]"));
        assert!(text.contains("[Rust 1.99.1](https://blog.rust-lang.org/)"));
        assert!(text.ends_with("1.99.1"));

        let mut chat = super::super::render::ChunkRenderer::new("m".into(), 1, false);
        let frames: Vec<u8> = events
            .into_iter()
            .flat_map(|e| chat.render(e))
            .flat_map(|b| b.to_vec())
            .collect();
        let raw = String::from_utf8(frames).unwrap();
        assert!(!raw.contains("tool_calls"));
        assert!(raw.contains("\"finish_reason\":\"stop\""));
    }

    #[test]
    fn client_tool_args_follow_their_block_after_a_server_tool() {
        let mut decoder = ClaudeDecoder::new();
        let mut events = Vec::new();
        for frame in [
            json!({"type":"message_start","message":{"id":"msg_mix","usage":{}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_a","name":"custom_lookup","input":{}}}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"server_tool_use","id":"srvtoolu_b","name":"web_search","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"k\":1}"}}),
            // An argument delta for a block whose start never arrived.
            json!({"type":"content_block_delta","index":7,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
        ] {
            decoder.decode(frame.to_string().as_bytes(), &mut events);
        }
        let tool_events: Vec<_> = events
            .into_iter()
            .filter(|e| !matches!(e, CodexEvent::Created { .. }))
            .collect();
        assert_eq!(
            tool_events,
            vec![
                CodexEvent::ToolCallBegin {
                    output_index: 0,
                    call_id: "toolu_a".into(),
                    name: "lookup".into(),
                },
                CodexEvent::ToolArgsDelta {
                    output_index: 0,
                    delta: "{\"k\":1}".into(),
                },
            ]
        );
    }

    #[test]
    fn non_stream_web_search_result_becomes_text_not_a_tool_call() {
        let out = anthropic_json_to_openai(
            &json!({
                "stop_reason":"end_turn",
                "content":[
                    {"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{"query":"q"}},
                    {"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":{"type":"web_search_tool_result_error","error_code":"max_uses_exceeded"}},
                    {"type":"text","text":"answer"}
                ],
                "usage":{"input_tokens":1,"output_tokens":1}
            }),
            "m",
            1,
        );
        let message = &out["choices"][0]["message"];
        assert!(message.get("tool_calls").is_none());
        assert_eq!(
            message["content"],
            "\n[web_search: q]\n- error: max_uses_exceeded\n\nanswer"
        );
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn relay_usage_reported_only_on_message_delta_is_kept() {
        let mut decoder = ClaudeDecoder::new();
        let mut events = Vec::new();
        decoder.decode(
            br#"{"type":"message_start","message":{"id":"msg_early","usage":{"input_tokens":0,"output_tokens":0}}}"#,
            &mut events,
        );
        decoder.decode(
            br#"{"type":"message_delta","usage":{"input_tokens":3,"cache_read_input_tokens":12705,"cache_creation_input_tokens":9,"output_tokens":4}}"#,
            &mut events,
        );
        decoder.decode(br#"{"type":"message_stop"}"#, &mut events);

        let completed = events
            .iter()
            .find_map(|e| match e {
                CodexEvent::Completed { usage } => usage.as_ref(),
                _ => None,
            })
            .expect("completed usage");
        assert_eq!(completed.prompt_tokens, 3 + 12705 + 9);
        assert_eq!(completed.cached_tokens, 12705);
        assert!(completed.cached_tokens_known);
        assert_eq!(completed.cache_write_tokens, 9);
        assert!(completed.cache_write_tokens_known);
        assert_eq!(completed.completion_tokens, 4);
    }

    #[test]
    fn anthropic_usage_with_cache_creation_input_tokens_parses_into_cache_write_tokens() {
        let mut decoder = ClaudeDecoder::new();
        let mut events = Vec::new();
        decoder.decode(
            br#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":100,"cache_read_input_tokens":40,"cache_creation_input_tokens":60}}}"#,
            &mut events,
        );
        decoder.decode(
            br#"{"type":"message_delta","usage":{"output_tokens":25}}"#,
            &mut events,
        );
        decoder.decode(br#"{"type":"message_stop"}"#, &mut events);

        let completed = events
            .iter()
            .find_map(|e| match e {
                CodexEvent::Completed { usage } => usage.as_ref(),
                _ => None,
            })
            .expect("completed usage");

        assert_eq!(completed.prompt_tokens, 200);
        assert_eq!(completed.completion_tokens, 25);
        assert_eq!(completed.total_tokens, 225);
        assert_eq!(completed.cached_tokens, 40);
        assert_eq!(completed.cache_write_tokens, 60);

        let response = json!({
            "id": "msg_2",
            "content": [{"type": "text", "text": "hello"}],
            "usage": {
                "input_tokens": 100,
                "output_tokens": 25,
                "cache_read_input_tokens": 40,
                "cache_creation_input_tokens": 60
            }
        });
        let openai = claude_json_to_openai(&response, "claude-3-5-sonnet", 1000);
        assert_eq!(openai["usage"]["prompt_tokens"], 200);
        assert_eq!(openai["usage"]["cache_creation_input_tokens"], 60);
        assert_eq!(openai["usage"]["cache_read_input_tokens"], 40);
        assert_eq!(
            openai["usage"]["prompt_tokens_details"]["cached_tokens"],
            40
        );
        assert_eq!(
            openai["usage"]["prompt_tokens_details"]["cache_write_tokens"],
            60
        );
    }

    #[test]
    fn unknown_efforts_are_rejected() {
        let body = json!({
            "model": "glm-5.3-flash",
            "reasoning_effort": "turbo",
            "max_tokens": 4096,
            "messages": [{"role": "user", "content": "hi"}]
        });
        assert!(openai_to_anthropic(&body).is_err());
    }
}

#[cfg(test)]
mod openai_to_anthropic_tests {
    use super::openai_to_anthropic;
    use serde_json::{json, Value};

    #[test]
    fn empty_content_is_not_sent_as_a_blank_block() {
        // Anthropic rejects "text content blocks must be non-empty" and
        // "all messages must have non-empty content".
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "", "tool_calls": [{
                    "id": "call_1",
                    "function": {"name": "get_time", "arguments": "{}"}
                }]},
                {"role": "user", "content": "thanks"}
            ]
        });
        let out = openai_to_anthropic(&body).expect("translates");
        let messages = out["messages"].as_array().expect("messages array");
        for message in messages {
            let content = message["content"].as_array().expect("content array");
            assert!(
                !content.is_empty(),
                "message sent with empty content array: {message}"
            );
            for block in content {
                if block["type"] == "text" {
                    assert!(
                        !block["text"].as_str().unwrap_or("").is_empty(),
                        "empty text block emitted: {message}"
                    );
                }
            }
        }
        // The tool_call must survive the empty-text cleanup.
        let has_tool_use = messages.iter().any(|message| {
            message["content"]
                .as_array()
                .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_use"))
        });
        assert!(has_tool_use, "tool_use block was dropped: {messages:?}");
    }

    #[test]
    fn message_with_only_null_content_is_dropped_entirely() {
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": null}
            ]
        });
        let out = openai_to_anthropic(&body).expect("translates");
        let messages = out["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 1, "empty message not dropped: {messages:?}");
    }

    #[test]
    fn test_ensure_claude_code_system_instruction() {
        use super::{ensure_claude_code_system_instruction, CLAUDE_CODE_SYSTEM_INSTRUCTION};

        // Case 1: Empty / missing system
        let mut body = json!({"messages": []});
        ensure_claude_code_system_instruction(&mut body);
        let sys = body["system"].as_array().unwrap();
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["text"], CLAUDE_CODE_SYSTEM_INSTRUCTION);

        // Case 2: String system
        let mut body = json!({"system": "You are helpful."});
        ensure_claude_code_system_instruction(&mut body);
        let s = body["system"].as_str().unwrap();
        assert!(s.starts_with(CLAUDE_CODE_SYSTEM_INSTRUCTION));
        assert!(s.ends_with("You are helpful."));

        // Case 3: Array system
        let mut body = json!({"system": [{"type": "text", "text": "Original prompt"}]});
        ensure_claude_code_system_instruction(&mut body);
        let arr = body["system"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["text"], CLAUDE_CODE_SYSTEM_INSTRUCTION);
        assert_eq!(arr[1]["text"], "Original prompt");

        // Case 4: Already present, should not duplicate
        ensure_claude_code_system_instruction(&mut body);
        let arr2 = body["system"].as_array().unwrap();
        assert_eq!(arr2.len(), 2);
    }

    #[test]
    fn suffix_overrides_body_reasoning_and_strips_suffix() {
        let body = json!({
            "model": "claude-3-7-sonnet(high)",
            "reasoning_effort": "low",
            "messages": [{"role": "user", "content": "solve this"}]
        });
        let out = openai_to_anthropic(&body).expect("translates");
        assert_eq!(out["model"], "claude-3-7-sonnet");
        assert_eq!(out["thinking"]["type"], "enabled");
        assert_eq!(out["thinking"]["budget_tokens"], 24576);
    }

    #[test]
    fn suffix_none_disables_thinking_even_if_body_sets_high() {
        let body = json!({
            "model": "claude-3-7-sonnet(none)",
            "reasoning_effort": "high",
            "messages": [{"role": "user", "content": "solve this"}]
        });
        let out = openai_to_anthropic(&body).expect("translates");
        assert_eq!(out["model"], "claude-3-7-sonnet");
        assert_eq!(out["thinking"]["type"], "disabled");
    }

    #[test]
    fn thinking_normalizes_temperature_to_one() {
        let body = json!({
            "model": "claude-3-7-sonnet(medium)",
            "temperature": 0.3,
            "messages": [{"role": "user", "content": "solve this"}]
        });
        let out = openai_to_anthropic(&body).expect("translates");
        assert_eq!(out["temperature"], 1.0);
    }

    #[test]
    fn response_format_rejected_as_unsupported_conflict() {
        let body = json!({
            "model": "claude-3-7-sonnet",
            "response_format": {
                "type": "json_schema",
                "json_schema": {"name": "test", "schema": {}}
            },
            "messages": [{"role": "user", "content": "solve this"}]
        });
        let err = openai_to_anthropic(&body).expect_err("should reject response_format");
        assert!(err.contains("unsupported option conflict"));
    }

    #[test]
    fn explicit_text_format_is_a_noop_for_claude() {
        let body = json!({"model":"claude-sonnet-4-6", "messages":[],
            "response_format":{"type":"text"}});
        let out = openai_to_anthropic(&body).unwrap();
        assert!(out.get("system").is_none());
        assert!(out.get("output_config").is_none());
    }

    #[test]
    fn adaptive_effort_uses_canonical_levels_and_auto_omits_effort() {
        for (raw, expected) in [
            ("minimal", Some("low")),
            ("medium", Some("medium")),
            ("high", Some("high")),
            ("xhigh", Some("high")),
            ("max", Some("max")),
            ("auto", None),
            ("-1", None),
        ] {
            let body = json!({"model":format!("claude-sonnet-4-6({raw})"),
                "reasoning_effort":"low", "messages":[]});
            let out = openai_to_anthropic(&body).unwrap();
            assert_eq!(out["thinking"]["type"], "adaptive");
            assert_eq!(
                out.pointer("/output_config/effort").and_then(Value::as_str),
                expected
            );
            assert!(out["thinking"].get("budget_tokens").is_none());
        }
    }

    #[test]
    fn manual_budget_preserves_numeric_input_and_respects_output_limit() {
        for (raw, max_tokens, expected) in [
            ("medium", 32768, 8192),
            ("high", 32768, 24576),
            ("2048", 32768, 2048),
            ("max", 4096, 4095),
        ] {
            let body = json!({"model":format!("claude-3-7-sonnet({raw})"),
                "max_tokens":max_tokens, "messages":[]});
            let out = openai_to_anthropic(&body).unwrap();
            assert_eq!(out["thinking"]["budget_tokens"], expected);
            assert_eq!(out["max_tokens"], max_tokens);
        }
    }

    #[test]
    fn adaptive_catalog_numeric_suffix_retains_budget_mode() {
        let body = json!({"model":"claude-sonnet-4-6(2048)", "messages":[]});
        let out = openai_to_anthropic(&body).unwrap();
        assert_eq!(
            out["thinking"],
            json!({"type":"enabled","budget_tokens":2048})
        );
        assert!(out.get("output_config").is_none());
    }

    #[test]
    fn explicitly_supported_xhigh_is_not_rewritten_to_max() {
        let body = json!({"model":"claude-opus-4-7(xhigh)", "messages":[]});
        let out = openai_to_anthropic(&body).unwrap();
        assert_eq!(out["thinking"]["type"], "adaptive");
        assert_eq!(out["output_config"]["effort"], "xhigh");
    }

    #[test]
    fn runtime_profile_not_model_name_selects_adaptive_and_range() {
        let capability =
            mahoquot_registry::ThinkingCapability::Supported(mahoquot_registry::ThinkingSupport {
                min: 128,
                max: 4096,
                levels: vec!["low".into(), "high".into()],
                ..Default::default()
            });
        let body = json!({"model":"operator-custom(2048)", "messages":[]});
        let out = super::openai_to_anthropic_with_capability(&body, &capability).unwrap();
        assert_eq!(out["thinking"]["budget_tokens"], 2048);
        let body = json!({"model":"operator-custom(medium)", "messages":[]});
        let out = super::openai_to_anthropic_with_capability(&body, &capability).unwrap();
        assert_eq!(out["output_config"]["effort"], "low");
    }

    #[test]
    fn unsupported_profile_rejects_thinking_but_unknown_custom_keeps_manual_contract() {
        let body = json!({"model":"operator-custom(high)", "messages":[]});
        assert!(super::openai_to_anthropic_with_capability(
            &body,
            &mahoquot_registry::ThinkingCapability::Unsupported
        )
        .is_err());
        let out = openai_to_anthropic(&body).unwrap();
        assert_eq!(out["thinking"]["budget_tokens"], 24576);
    }

    #[test]
    fn invalid_effort_and_insufficient_manual_output_budget_are_conflicts() {
        for extra in [
            json!({"reasoning_effort":true}),
            json!({"reasoning_effort":"high", "max_tokens":1024}),
        ] {
            let mut body = json!({"model":"claude-3-7-sonnet", "messages":[]});
            body.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(openai_to_anthropic(&body).is_err());
        }
    }

    #[test]
    fn translated_requests_mark_only_the_conversation_tail_for_caching() {
        let mut body = openai_to_anthropic(&json!({
            "model": "claude-opus-5-5",
            "messages": [
                {"role": "system", "content": "rules"},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "read", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "ok"},
            ],
        }))
        .unwrap();
        super::ensure_claude_code_system_instruction(&mut body);
        super::apply_prompt_cache_breakpoints(&mut body);

        // The relay drops block-form system prompts, so it must stay a string.
        assert!(body["system"]
            .as_str()
            .expect("system stays a string")
            .ends_with("rules"));
        let messages = body["messages"].as_array().unwrap();
        let tail = messages.last().unwrap()["content"].as_array().unwrap();
        assert_eq!(tail.last().unwrap()["type"], "tool_result");
        assert_eq!(tail.last().unwrap()["cache_control"]["type"], "ephemeral");
        assert_eq!(body.to_string().matches("\"cache_control\"").count(), 1);
    }

    #[test]
    fn excess_client_breakpoints_are_capped_at_the_provider_limit() {
        // Anthropic rejects a fifth breakpoint with HTTP 400. Messages are
        // stripped before system, and system before tools.
        let block = |text: &str| json!({"type": "text", "text": text, "cache_control": {"type": "ephemeral"}});
        let mut body = json!({
            "tools": [block("t1"), block("t2")],
            "system": [block("s1"), block("s2")],
            "messages": [
                {"role": "user", "content": [block("m1"), block("m2")]},
                {"role": "assistant", "content": [block("m3")]},
            ],
        });
        super::apply_prompt_cache_breakpoints(&mut body);

        // Seven markers against a limit of four: the three message markers
        // absorb the whole excess, so system and tools survive untouched.
        assert_eq!(super::breakpoint_count(&body), super::MAX_CACHE_BREAKPOINTS);
        assert!(body["messages"][0]["content"][0].get("cache_control").is_none());
        assert!(body["messages"][0]["content"][1].get("cache_control").is_none());
        assert!(body["messages"][1]["content"][0].get("cache_control").is_none());
        assert!(body["system"][0].get("cache_control").is_some());
        assert!(body["system"][1].get("cache_control").is_some());
        assert!(body["tools"][0].get("cache_control").is_some());
        assert!(body["tools"][1].get("cache_control").is_some());

        // One message marker, three system markers and two tool markers make
        // six: the message marker absorbs one and the excess reaches system.
        let mut body = json!({
            "tools": [block("t1"), block("t2")],
            "system": [block("s1"), block("s2"), block("s3")],
            "messages": [{"role": "user", "content": [block("m1")]}],
        });
        super::apply_prompt_cache_breakpoints(&mut body);
        assert_eq!(super::breakpoint_count(&body), super::MAX_CACHE_BREAKPOINTS);
        assert!(body["messages"][0]["content"][0].get("cache_control").is_none());
        assert!(body["system"][0].get("cache_control").is_none());
        assert!(body["system"][1].get("cache_control").is_some());
        assert!(body["system"][2].get("cache_control").is_some());
        assert!(body["tools"][0].get("cache_control").is_some());
        assert!(body["tools"][1].get("cache_control").is_some());
    }

    #[test]
    fn breakpoints_within_the_limit_are_untouched() {
        let block = |text: &str| json!({"type": "text", "text": text, "cache_control": {"type": "ephemeral"}});
        let sent = json!({
            "system": [block("s1")],
            "messages": [{"role": "user", "content": [block("m1"), block("m2")]}],
            "tools": [block("t1")],
        });
        let mut body = sent.clone();
        super::apply_prompt_cache_breakpoints(&mut body);
        assert_eq!(body, sent);
    }

    #[test]
    fn client_breakpoints_are_left_untouched_and_thinking_is_skipped() {
        let sent = json!({
            "system": [{"type": "text", "text": "s", "cache_control": {"type": "ephemeral"}}],
            "messages": [{"role": "user", "content": "hi"}],
        });
        let mut body = sent.clone();
        super::apply_prompt_cache_breakpoints(&mut body);
        assert_eq!(body, sent);

        let mut body = json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "text", "text": "a"},
                {"type": "thinking", "thinking": "t", "signature": "x"},
            ]}],
        });
        super::apply_prompt_cache_breakpoints(&mut body);
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
        assert!(blocks[1].get("cache_control").is_none());
    }
}
