//! Devin Connect codec: pure OpenAI-shaped request → protobuf builder and a
//! strict incremental Connect-frame response decoder.
//!
//! Transport contract (upstream `dsh-plugin-devin-bridge` at
//! `ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4`):
//! - `GetChatMessage` is server streaming: `Content-Type:
//!   application/connect+proto`, 5-byte envelopes (flags u8 + BE u32 length).
//!   Data frames carry flag 0 and protobuf `GetChatMessageResponse`; the
//!   terminal frame carries flag 2 and JSON `EndStreamResponse`.
//! - `GetCascadeModelConfigs` is unary: `Content-Type: application/proto`,
//!   no framing.
//! - The session token rides verbatim in `Authorization: Basic <token>-<token>`
//!   (literal concatenation, not base64 Basic) and inside protobuf
//!   `metadata.api_key = 3`.
//!
//! The decoder is strict: bounded 16 MiB frames, identity compression only,
//! no success before a valid EndStream frame, no events after the terminal.
//! Nothing here performs I/O; request IDs and random material are injected.

use prost::Message;
use serde_json::{Map, Value};

use super::devin_proto::{
    chat_message_request_type, chat_message_source, planner_mode, step_type, trajectory_type,
    ChatMessagePrompt, ChatMessageResponse, ChatToolCall, ChatToolDefinition,
    CompletionConfiguration, CortexTrajectoryReference, GetChatMessageRequest,
    GetCascadeModelConfigsRequest, ImageData, Metadata,
};
use super::events::{CodexEvent, Usage};
use super::split_data_url;

pub const STREAM_CONTENT_TYPE: &str = "application/connect+proto";
pub const UNARY_CONTENT_TYPE: &str = "application/proto";
pub const CONNECT_PROTOCOL_VERSION: &str = "1";

/// Largest accepted single response frame payload: 16 MiB.
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

const CLIENT_NAME: &str = "chisel";
const CLIENT_VERSION: &str = "3000.2.17";
const MAX_NEWLINES: u64 = 400;
const TOP_K: u64 = 40;
/// Conservative default when the client sends no output limit. This is a
/// request default, not a measured server maximum.
const DEFAULT_MAX_TOKENS: u64 = 4096;
const DEFAULT_TEMPERATURE: f64 = 1.0;
const DEFAULT_TOP_P: f64 = 0.95;

/// Literal Authorization header: `Basic <token>-<token>` — the upstream
/// contract, deliberately NOT `username:password` base64.
pub fn authorization_header(token: &str) -> String {
    format!("Basic {token}-{token}")
}

/// Everything the pure builder needs that is not in the client body.
#[derive(Clone)]
pub struct DevinRequestParams {
    /// Session token; enters protobuf `metadata.api_key` (and, at the relay
    /// layer, the literal Basic header built by [`authorization_header`]).
    pub token: String,
    /// Exact upstream model UID, without any `devin/` prefix.
    pub chat_model_uid: String,
    /// Caller-provided vision capability; images are rejected otherwise.
    pub supports_vision: bool,
    pub trajectory_id: String,
    pub cascade_id: String,
    pub execution_id: String,
    /// Reference clients send a fixed-length random fingerprint in
    /// metadata field 31; injected so the builder stays deterministic.
    pub fingerprint: String,
}

impl std::fmt::Debug for DevinRequestParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevinRequestParams")
            .field("token", &"[REDACTED]")
            .field("chat_model_uid", &self.chat_model_uid)
            .field("supports_vision", &self.supports_vision)
            .field("trajectory_id", &self.trajectory_id)
            .field("cascade_id", &self.cascade_id)
            .field("execution_id", &self.execution_id)
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

/// Standard Connect error codes recognized across Connect-protocol services.
pub fn normalize_connect_code(raw: &str) -> Option<&'static str> {
    match raw {
        "canceled" => Some("canceled"),
        "unknown" => Some("unknown"),
        "invalid_argument" => Some("invalid_argument"),
        "deadline_exceeded" => Some("deadline_exceeded"),
        "not_found" => Some("not_found"),
        "already_exists" => Some("already_exists"),
        "permission_denied" => Some("permission_denied"),
        "resource_exhausted" => Some("resource_exhausted"),
        "failed_precondition" => Some("failed_precondition"),
        "aborted" => Some("aborted"),
        "out_of_range" => Some("out_of_range"),
        "unimplemented" => Some("unimplemented"),
        "internal" => Some("internal"),
        "unavailable" => Some("unavailable"),
        "data_loss" => Some("data_loss"),
        "unauthenticated" => Some("unauthenticated"),
        _ => None,
    }
}

/// Fixed safe local descriptions for recognized Connect error codes.
/// Avoids reflecting arbitrary upstream error messages that could leak sensitive data.
pub fn default_code_description(code: &str) -> &'static str {
    match code {
        "canceled" => "operation was canceled",
        "unknown" => "unknown upstream error",
        "invalid_argument" => "invalid argument provided to upstream",
        "deadline_exceeded" => "upstream request timed out",
        "not_found" => "requested resource not found upstream",
        "already_exists" => "resource already exists upstream",
        "permission_denied" => "permission denied by upstream",
        "resource_exhausted" => "upstream quota or rate limit exhausted",
        "failed_precondition" => "precondition check failed upstream",
        "aborted" => "operation aborted upstream",
        "out_of_range" => "operation out of range upstream",
        "unimplemented" => "operation not implemented upstream",
        "internal" => "internal server error from upstream",
        "unavailable" => "upstream service unavailable",
        "data_loss" => "data loss reported by upstream",
        "unauthenticated" => "unauthenticated request to upstream",
        _ => "unknown upstream error",
    }
}

/// Builds the streaming `GetChatMessageRequest` protobuf from an OpenAI
/// chat-completions body. `new_id` supplies message IDs (one call per prompt)
/// so the transform stays pure and fixture-friendly.
pub fn build_chat_request(
    body: &Value,
    params: &DevinRequestParams,
    new_id: &mut dyn FnMut() -> String,
) -> Result<Vec<u8>, String> {
    use prost::Message;

    let obj = body.as_object().ok_or("request body must be an object")?;
    let messages = obj
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("missing messages")?;

    // Unsupported options are rejected, never silently dropped.
    if let Some(n) = obj.get("n").and_then(Value::as_u64) {
        if n > 1 {
            return Err(format!("n={n} is unsupported: Devin maps one completion per request"));
        }
    }
    if let Some(rf) = obj.get("response_format") {
        let rf_type = rf.get("type").and_then(Value::as_str);
        if rf_type == Some("json_schema") {
            return Err("response_format json_schema is unsupported".to_string());
        }
        if rf.get("strict").and_then(Value::as_bool) == Some(true) {
            return Err("response_format strict=true is unsupported".to_string());
        }
        if let Some(js) = rf.get("json_schema") {
            if js.get("strict").and_then(Value::as_bool) == Some(true) {
                return Err("response_format json_schema.strict=true is unsupported".to_string());
            }
            if rf_type.is_none() && (js.get("name").is_some() || js.get("schema").is_some()) {
                return Err("response_format json_schema is unsupported".to_string());
            }
        }
    }
    let tool_choice = obj.get("tool_choice");
    let tool_choice_str = tool_choice.and_then(Value::as_str);
    match tool_choice_str {
        None | Some("auto") => {}
        Some("none") => {}
        Some(_) => {
            return Err(format!(
                "forced tool_choice {tool_choice_str:?} is unsupported: Devin has no forced-tool mapping"
            ))
        }
    }
    if tool_choice.is_some() && tool_choice_str.is_none() && !tool_choice.unwrap().is_null() {
        return Err("forced tool_choice object is unsupported: Devin has no forced-tool mapping".to_string());
    }
    let send_tools = tool_choice_str != Some("none");
    let mut tools = Vec::new();
    if send_tools {
        for tool in obj.get("tools").and_then(Value::as_array).unwrap_or(&Vec::new()) {
            let function = tool
                .get("function")
                .or_else(|| (tool.get("name").is_some()).then_some(tool))
                .ok_or("tool entry without a function definition")?;
            if function.get("strict").and_then(Value::as_bool) == Some(true) {
                return Err("strict structured output is unsupported".to_string());
            }
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or("tool function missing name")?;
            tools.push(ChatToolDefinition {
                name: Some(name.to_string()),
                description: Some(
                    function
                        .get("description")
                        .and_then(Value::as_str)
                        .filter(|d| !d.is_empty())
                        .unwrap_or(name)
                        .to_string(),
                ),
                // Schema preserved verbatim: descriptions, enum, x-* keys.
                json_schema_string: Some(
                    function
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| Value::Object(Map::new()))
                        .to_string(),
                ),
            });
        }
    }

    // System/developer instructions become the top-level prompt verbatim.
    let mut instructions: Vec<String> = Vec::new();
    for message in messages {
        if matches!(
            message.get("role").and_then(Value::as_str),
            Some("system") | Some("developer")
        ) {
            let text = text_content(message.get("content"));
            if !text.is_empty() {
                instructions.push(text);
            }
        }
    }
    let prompt = (!instructions.is_empty()).then(|| instructions.join("\n\n"));

    let mut prompts = Vec::new();
    for message in messages {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("user");
        if role == "system" || role == "developer" {
            continue;
        }
        prompts.push(build_prompt(message, role, params, new_id)?);
    }

    let configuration = CompletionConfiguration {
        num_completions: Some(1),
        max_tokens: Some(output_limit(obj)),
        max_newlines: Some(MAX_NEWLINES),
        temperature: Some(
            obj.get("temperature")
                .and_then(Value::as_f64)
                .unwrap_or(DEFAULT_TEMPERATURE),
        ),
        top_k: Some(TOP_K),
        top_p: Some(
            obj.get("top_p")
                .and_then(Value::as_f64)
                .unwrap_or(DEFAULT_TOP_P),
        ),
    };

    let request = GetChatMessageRequest {
        metadata: Some(metadata(&params.token, Some(&params.fingerprint))),
        prompt,
        chat_message_prompts: prompts,
        request_type: Some(chat_message_request_type::CASCADE),
        configuration: Some(configuration),
        tools,
        trajectory_reference: Some(CortexTrajectoryReference {
            trajectory_id: Some(params.trajectory_id.clone()),
            trajectory_type: Some(trajectory_type::CASCADE),
            step_type: Some(step_type::USER_INPUT),
        }),
        cascade_id: Some(params.cascade_id.clone()),
        planner_mode: Some(planner_mode::DEFAULT),
        chat_model_uid: Some(params.chat_model_uid.clone()),
        execution_id: Some(params.execution_id.clone()),
    };
    let mut wire = Vec::new();
    request
        .encode(&mut wire)
        .map_err(|e| format!("request encode failed: {e}"))?;
    Ok(wire)
}

/// Unary `GetCascadeModelConfigsRequest` protobuf: framing-free, unlike the
/// streaming method.
pub fn build_model_configs_request(token: &str) -> Vec<u8> {
    use prost::Message;
    let request = GetCascadeModelConfigsRequest {
        metadata: Some(metadata(token, None)),
    };
    let mut wire = Vec::new();
    request
        .encode(&mut wire)
        .expect("model configs request encode cannot fail");
    wire
}

fn metadata(token: &str, fingerprint: Option<&str>) -> Metadata {
    Metadata {
        ide_name: Some(CLIENT_NAME.to_string()),
        extension_version: Some(CLIENT_VERSION.to_string()),
        api_key: Some(token.to_string()),
        locale: Some("en".to_string()),
        os: Some("win".to_string()),
        ide_version: Some(CLIENT_VERSION.to_string()),
        extension_name: Some(CLIENT_NAME.to_string()),
        f: fingerprint.map(str::to_string),
    }
}

fn output_limit(obj: &Map<String, Value>) -> u64 {
    obj.get("max_tokens")
        .or_else(|| obj.get("max_completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_TOKENS)
}

fn build_prompt(
    message: &Value,
    role: &str,
    params: &DevinRequestParams,
    new_id: &mut dyn FnMut() -> String,
) -> Result<ChatMessagePrompt, String> {
    let source = match role {
        "user" => chat_message_source::USER,
        "assistant" => chat_message_source::SYSTEM,
        "tool" | "function" => chat_message_source::TOOL,
        other => return Err(format!("unsupported message role: {other}")),
    };

    let mut prompt = ChatMessagePrompt {
        message_id: Some(new_id()),
        source: Some(source),
        prompt: (!text_content(message.get("content")).is_empty())
            .then(|| text_content(message.get("content"))),
        tool_calls: Vec::new(),
        tool_call_id: None,
        tool_result_is_error: None,
        images: Vec::new(),
        thinking: None,
        signature: None,
        thinking_redacted: None,
    };

    match source {
        chat_message_source::USER => {
            // Images ride their own turn, including past turns; data URLs only.
            for image in image_parts(message.get("content"))? {
                if !params.supports_vision {
                    return Err("model does not support vision input".to_string());
                }
                prompt.images.push(image);
            }
        }
        chat_message_source::SYSTEM => {
            for call in message
                .get("tool_calls")
                .and_then(Value::as_array)
                .unwrap_or(&Vec::new())
            {
                let function = call.get("function").ok_or("assistant tool call missing function")?;
                prompt.tool_calls.push(ChatToolCall {
                    id: Some(
                        call.get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    ),
                    name: Some(
                        function
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    ),
                    arguments_json: Some(
                        function
                            .get("arguments")
                            .map(|args| match args {
                                Value::String(s) => s.clone(),
                                other => other.to_string(),
                            })
                            .unwrap_or_else(|| "{}".to_string()),
                    ),
                });
            }
            if let Some(thinking) = message.get("reasoning_content").and_then(Value::as_str) {
                prompt.thinking = Some(thinking.to_string());
            }
            if let Some(signature) = message.get("reasoning_signature").and_then(Value::as_str) {
                prompt.signature = Some(signature.to_string());
            }
            if message.get("reasoning_redacted").and_then(Value::as_bool) == Some(true) {
                prompt.thinking_redacted = Some(true);
            }
        }
        _ => {
            let call_id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or("tool result message missing tool_call_id")?;
            prompt.tool_call_id = Some(call_id.to_string());
            if message.get("is_error").and_then(Value::as_bool) == Some(true) {
                prompt.tool_result_is_error = Some(true);
            }
        }
    }
    Ok(prompt)
}

fn image_parts(content: Option<&Value>) -> Result<Vec<ImageData>, String> {
    let Value::Array(parts) = content.unwrap_or(&Value::Null) else {
        return Ok(Vec::new());
    };
    parts
        .iter()
        .filter_map(|part| {
            part.get("image_url")
                .or_else(|| part.get("imageUrl"))
                .and_then(|img| img.get("url"))
                .and_then(Value::as_str)
                .map(|url| {
                    let (media_type, data) =
                        split_data_url(url).ok_or_else(|| {
                            "remote image url is unsupported".to_string()
                        })?;
                    Ok(ImageData {
                        base64_data: Some(data.to_string()),
                        mime_type: Some(media_type.to_string()),
                    })
                })
        })
        .collect()
}

fn text_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                if part.get("image_url").is_some() || part.get("imageUrl").is_some() {
                    return None;
                }
                part.get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| part.as_str().map(str::to_string))
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

// ─── Connect frame helpers (also used to build test/mock upstreams) ────────

/// Wraps a protobuf payload into a data-frame envelope (flag 0).
pub fn frame_data(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(0u8);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Wraps a JSON `EndStreamResponse` payload into the terminal envelope (flag 2).
pub fn frame_end_stream(json: &str) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + json.len());
    frame.push(2u8);
    frame.extend_from_slice(&(json.len() as u32).to_be_bytes());
    frame.extend_from_slice(json.as_bytes());
    frame
}

// ─── Response decoder ───────────────────────────────────────────────────────

/// Typed terminal metadata a relay needs after the stream ends. `usage` is
/// the final authoritative snapshot (cache read/write kept separately), or
/// None when the server sent none — never a fabricated zero.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DevinOutcome {
    pub usage: Option<Usage>,
    pub actual_model_uid: Option<String>,
    pub stop_reason: Option<i32>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    /// True once a valid EndStream frame was consumed.
    pub terminated: bool,
}

#[derive(Debug, Default)]
struct ToolState {
    call_id: Option<String>,
    name: Option<String>,
    args: String,
    pending_args: Vec<String>,
    block_index: u64,
    began: bool,
}

/// Incremental Connect decoder for `GetChatMessage`. Feed raw response bytes
/// in any chunking; call [`DevinDecoder::finish`] at EOF. Terminal success is
/// emitted only when a valid flag-2 EndStream frame arrives without an error.
#[derive(Debug, Default)]
pub struct DevinDecoder {
    buf: Vec<u8>,
    next_block: u64,
    tools: Vec<ToolState>,
    outcome: DevinOutcome,
    /// Once failed (bad frame, terminal error), stop emitting normal events.
    failed: bool,
    /// Stream finish called; ensures finish is idempotent.
    finished: bool,
}

impl DevinDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn outcome(&self) -> &DevinOutcome {
        &self.outcome
    }

    pub fn decode(&mut self, bytes: &[u8], out: &mut Vec<CodexEvent>) {
        if self.failed {
            return;
        }
        let mut cursor = 0;
        while cursor < bytes.len() && !self.failed {
            if self.outcome.terminated {
                self.fail("trailing data after EndStream frame".to_string(), out);
                return;
            }

            if self.buf.len() < 5 {
                let needed = 5 - self.buf.len();
                let take = needed.min(bytes.len() - cursor);
                self.buf.extend_from_slice(&bytes[cursor..cursor + take]);
                cursor += take;
                if self.buf.len() < 5 {
                    return;
                }
            }

            let flags = self.buf[0];
            let len = u32::from_be_bytes(self.buf[1..5].try_into().expect("5 bytes read")) as usize;
            if len > MAX_FRAME_SIZE {
                self.fail(format!("connect frame too large: {len} bytes > {MAX_FRAME_SIZE}"), out);
                return;
            }

            let needed_for_frame = (5 + len) - self.buf.len();
            let take = needed_for_frame.min(bytes.len() - cursor);
            self.buf.extend_from_slice(&bytes[cursor..cursor + take]);
            cursor += take;
            if self.buf.len() < 5 + len {
                return;
            }

            let payload: Vec<u8> = self.buf.drain(..5 + len).skip(5).collect();
            match flags {
                0 => self.decode_data(&payload, out),
                2 => {
                    let remaining_in_chunk = bytes.len() - cursor;
                    if !self.buf.is_empty() || remaining_in_chunk > 0 {
                        self.fail("trailing data after EndStream frame".to_string(), out);
                        return;
                    }
                    self.decode_end_stream(&payload, out);
                    return;
                }
                _ => {
                    self.fail(
                        "connect frame uses unnegotiated compression (flags != 0/2)".to_string(),
                        out,
                    );
                    return;
                }
            }
        }
    }

    /// EOF: a stream that never delivered a valid EndStream frame failed.
    pub fn finish(&mut self, out: &mut Vec<CodexEvent>) {
        if self.failed || self.finished {
            return;
        }
        self.finished = true;
        if !self.buf.is_empty() {
            if self.outcome.terminated {
                self.fail("trailing data after EndStream frame".to_string(), out);
            } else {
                self.fail("truncated connect frame at stream end".to_string(), out);
            }
            return;
        }
        if !self.outcome.terminated {
            self.fail("connect stream ended without EndStream frame".to_string(), out);
            return;
        }
        if self.tools.iter().any(|t| t.call_id.is_none() || t.name.is_none()) {
            self.fail("unresolved tool missing ID or name at stream end".to_string(), out);
            return;
        }
        for tool in &mut self.tools {
            if !tool.began {
                tool.began = true;
                out.push(CodexEvent::ToolCallBegin {
                    output_index: tool.block_index,
                    call_id: tool.call_id.clone().unwrap(),
                    name: tool.name.clone().unwrap(),
                });
                for pending in std::mem::take(&mut tool.pending_args) {
                    out.push(CodexEvent::ToolArgsDelta {
                        output_index: tool.block_index,
                        delta: pending,
                    });
                }
            }
        }
        // Valid EndStream at stream end: emit authoritative Completed snapshot.
        out.push(CodexEvent::Completed {
            usage: self.outcome.usage.clone(),
        });
    }

    fn fail(&mut self, message: String, out: &mut Vec<CodexEvent>) {
        self.failed = true;
        out.push(CodexEvent::Failed { message });
    }

    fn decode_data(&mut self, payload: &[u8], out: &mut Vec<CodexEvent>) {
        let message = match ChatMessageResponse::decode(payload) {
            Ok(message) => message,
            Err(err) => {
                self.fail(format!("malformed connect protobuf payload: {err}"), out);
                return;
            }
        };
        // Reasoning first, matching upstream interleaving: thinking, then its
        // signature/redaction marker, then text, then tool deltas.
        if let Some(text) = message.delta_thinking.filter(|t| !t.is_empty()) {
            out.push(CodexEvent::ReasoningDelta(text));
        }
        if let Some(redacted) = message.thinking_redacted {
            if redacted {
                out.push(CodexEvent::ReasoningRedacted);
            }
        }
        if let Some(signature) = message.delta_signature.filter(|s| !s.is_empty()) {
            out.push(CodexEvent::ReasoningSignature(signature));
        }
        if let Some(text) = message.delta_text.filter(|t| !t.is_empty()) {
            out.push(CodexEvent::TextDelta(text));
        }
        for delta in message.delta_tool_calls {
            self.decode_tool_delta(delta, out);
            if self.failed {
                return;
            }
        }
        if let Some(usage) = message.usage {
            // Last snapshot wins; never summed across frames.
            let input = usage.input_tokens.unwrap_or(0);
            let output = usage.output_tokens.unwrap_or(0);
            self.outcome.usage = Some(Usage {
                prompt_tokens: input,
                completion_tokens: output,
                total_tokens: input + output,
                cached_tokens: usage.cache_read_tokens.unwrap_or(0),
                cache_write_tokens: usage.cache_write_tokens.unwrap_or(0),
                reasoning_tokens: 0,
            });
        }
        if let Some(model_uid) = message.actual_model_uid.filter(|u| !u.is_empty()) {
            self.outcome.actual_model_uid = Some(model_uid);
        }
        if let Some(stop) = message.stop_reason.filter(|s| *s != 0) {
            self.outcome.stop_reason = Some(stop);
            match stop {
                // Output limit family: 3 max tokens, 1 incomplete, 9 partial.
                1 | 3 | 9 => out.push(CodexEvent::OutputLimitReached),
                13 => {
                    self.fail("Devin stopped with STOP_REASON_ERROR".to_string(), out);
                }
                // 10 FUNCTION_CALL and 0 UNSPECIFIED: terminal rendering waits
                // for the EndStream frame.
                _ => {}
            }
        }
    }

    fn decode_tool_delta(&mut self, delta: ChatToolCall, out: &mut Vec<CodexEvent>) {
        let id_val = delta.id.filter(|id| !id.is_empty());
        let name_val = delta.name.filter(|n| !n.is_empty());
        let args_val = delta.arguments_json.filter(|a| !a.is_empty());

        let index = if let Some(id) = id_val.as_ref() {
            if let Some(idx) = self.tools.iter().position(|t| t.call_id.as_deref() == Some(id)) {
                idx
            } else {
                let pending_indices: Vec<usize> = self
                    .tools
                    .iter()
                    .enumerate()
                    .filter_map(|(i, t)| if t.call_id.is_none() { Some(i) } else { None })
                    .collect();
                if pending_indices.len() == 1 {
                    let idx = pending_indices[0];
                    self.tools[idx].call_id = Some(id.clone());
                    idx
                } else if pending_indices.len() > 1 {
                    self.fail(
                        "ambiguous tool ID resolution: multiple pending tools missing ID".to_string(),
                        out,
                    );
                    return;
                } else {
                    let idx = self.tools.len();
                    self.tools.push(ToolState {
                        call_id: Some(id.clone()),
                        name: None,
                        args: String::new(),
                        pending_args: Vec::new(),
                        block_index: self.next_block,
                        began: false,
                    });
                    self.next_block += 1;
                    idx
                }
            }
        } else if self.tools.is_empty() {
            self.tools.push(ToolState {
                call_id: None,
                name: None,
                args: String::new(),
                pending_args: Vec::new(),
                block_index: self.next_block,
                began: false,
            });
            self.next_block += 1;
            0
        } else if self.tools.len() == 1 {
            0
        } else {
            self.fail(
                "ambiguous ID-less tool delta: active tool call cannot be identified".to_string(),
                out,
            );
            return;
        };

        let state = &mut self.tools[index];
        if let Some(name) = name_val {
            state.name = Some(name);
        }
        if let Some(ref args) = args_val {
            state.args.push_str(args);
        }

        if !state.began && state.name.is_some() && state.call_id.is_some() {
            state.began = true;
            let call_id = state.call_id.clone().unwrap();
            let name = state.name.clone().unwrap();
            out.push(CodexEvent::ToolCallBegin {
                output_index: state.block_index,
                call_id,
                name,
            });
            for pending in std::mem::take(&mut state.pending_args) {
                out.push(CodexEvent::ToolArgsDelta {
                    output_index: state.block_index,
                    delta: pending,
                });
            }
        }

        if let Some(args) = args_val {
            if state.began {
                out.push(CodexEvent::ToolArgsDelta {
                    output_index: state.block_index,
                    delta: args,
                });
            } else {
                state.pending_args.push(args);
            }
        }
    }

    fn decode_end_stream(&mut self, payload: &[u8], out: &mut Vec<CodexEvent>) {
        if self.outcome.terminated {
            self.fail("duplicate EndStream frame in connect stream".to_string(), out);
            return;
        }
        let value: Value = match serde_json::from_slice(payload) {
            Ok(value) => value,
            Err(err) => {
                self.fail(format!("malformed EndStream JSON: {err}"), out);
                return;
            }
        };
        let Some(obj) = value.as_object() else {
            self.fail("malformed EndStream JSON: expected object".to_string(), out);
            return;
        };
        self.outcome.terminated = true;
        if let Some(error) = obj.get("error").filter(|e| !e.is_null()) {
            let raw_code = error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let (code, description) = match normalize_connect_code(raw_code) {
                Some(known) => (known, default_code_description(known)),
                None => ("unknown", "unknown upstream error"),
            };
            self.outcome.error_code = Some(code.to_string());
            self.outcome.error_message = Some(description.to_string());
            self.fail(format!("{code}: {description}"), out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bounded_allocation_private_unit_seam() {
        let mut decoder = DevinDecoder::new();
        let mut events = Vec::new();
        let len_20mib: u32 = 20 * 1024 * 1024;
        let mut chunk = vec![0u8];
        chunk.extend_from_slice(&len_20mib.to_be_bytes());
        // Large same-chunk payload: 512 KiB
        chunk.resize(5 + 512 * 1024, 0xAA);
        decoder.decode(&chunk, &mut events);
        assert!(events.iter().any(|e| matches!(e, CodexEvent::Failed { .. })));
        assert!(decoder.buf.capacity() < 1024 * 1024, "capacity was: {}", decoder.buf.capacity());
        assert!(decoder.buf.len() <= 5, "retained buffer was: {}", decoder.buf.len());
    }
}
