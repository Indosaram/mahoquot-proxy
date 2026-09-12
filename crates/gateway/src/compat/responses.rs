//! OpenAI Responses API request and event adapter for Devin.
//!
//! Provides two-way translation between the OpenAI Responses API surface and
//! Devin's underlying representation:
//! 1. Inbound request normalization ([`responses_to_openai`]): parses Responses
//!    payloads (`input`, `instructions`, function tools, multi-turn history)
//!    into OpenAI chat-completions shape for downstream Devin Connect dispatch.
//!    Enforces explicit rejection of unsupported server-side state, remote URLs,
//!    and malformed tool histories without silent coercion.
//! 2. Streaming SSE event adapter ([`ResponsesStreamRenderer`]): translates
//!    common [`CodexEvent`] streams into canonical Responses SSE frames
//!    (`response.created`, `response.output_item.added`, `response.content_part.added`,
//!    `response.output_text.delta`, `response.function_call_arguments.*`,
//!    `response.output_item.done`, `response.completed` / `response.incomplete` /
//!    `response.failed`).
//! 3. Non-streaming response aggregator ([`responses_response`]): aggregates
//!    [`CodexEvent`] streams into a single canonical Responses JSON object by
//!    reusing the streaming renderer's state machine, guaranteeing exact
//!    equivalence between streaming and non-streaming outputs.

use bytes::Bytes;
use serde_json::{json, Value};
use std::collections::HashSet;

use crate::compat::events::{CodexEvent, Usage};
use crate::compat::split_data_url;

/// Errors returned during Responses request normalization.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ResponsesError {
    #[error("invalid request: {message}")]
    InvalidRequest {
        message: String,
        param: Option<String>,
    },
    #[error("unsupported feature: {message}")]
    Unsupported {
        message: String,
        param: Option<String>,
    },
}

impl ResponsesError {
    pub fn invalid_request(message: impl Into<String>, param: Option<&str>) -> Self {
        Self::InvalidRequest {
            message: message.into(),
            param: param.map(str::to_string),
        }
    }

    pub fn unsupported(message: impl Into<String>, param: Option<&str>) -> Self {
        Self::Unsupported {
            message: message.into(),
            param: param.map(str::to_string),
        }
    }

    pub fn to_json_value(&self) -> Value {
        let (message, param) = match self {
            Self::InvalidRequest { message, param } => (message, param),
            Self::Unsupported { message, param } => (message, param),
        };
        json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "param": param,
                "code": Value::Null,
            }
        })
    }
}

impl From<ResponsesError> for String {
    fn from(err: ResponsesError) -> Self {
        err.to_string()
    }
}

/// Normalizes an inbound OpenAI Responses request into an OpenAI chat-completions
/// request ready for Devin protobuf conversion.
///
/// Features supported:
/// - `instructions` string -> top-level system message
/// - `input` string or structured array -> chat messages
/// - Base64 data URI vision images (`data:image/...;base64,...`)
/// - Multi-turn tool call history (`function_call` + `function_call_output`)
/// - Reasoning content, signature, and redaction flags
/// - Function tool definitions and forced `tool_choice`
/// - Sampling parameters (`temperature`, `top_p`) and `max_output_tokens` -> `max_tokens`
///
/// Rejections (HTTP 400 equivalent `ResponsesError`):
/// - Remote/stateful server-side context (`previous_response_id`)
/// - Asynchronous background processing (`background = true`)
/// - Server-side response persistence (`store = true`)
/// - Remote image URLs (`http://`, `https://`)
/// - Non-function tools (`web_search`, `file_search`, `computer`, etc.)
/// - Malformed tool histories (unmatched outputs, duplicate call IDs, malformed JSON arguments)
pub fn responses_to_openai(req: &Value) -> Result<Value, ResponsesError> {
    let obj = req
        .as_object()
        .ok_or_else(|| ResponsesError::invalid_request("request body must be an object", None))?;

    // ─── 1. Unsupported stateful/remote/background features ─────────────────
    if let Some(prev_id) = obj.get("previous_response_id") {
        if !prev_id.is_null() {
            return Err(ResponsesError::unsupported(
                "previous_response_id is not supported for Devin models",
                Some("previous_response_id"),
            ));
        }
    }
    if obj.get("background").and_then(Value::as_bool) == Some(true) {
        return Err(ResponsesError::unsupported(
            "background=true is not supported",
            Some("background"),
        ));
    }
    if obj.get("store").and_then(Value::as_bool) == Some(true) {
        return Err(ResponsesError::unsupported(
            "store=true is not supported",
            Some("store"),
        ));
    }

    // ─── 2. Tools validation and translation ─────────────────────────────────
    let mut mapped_tools = Vec::new();
    if let Some(tools_val) = obj.get("tools") {
        let tools_arr = tools_val.as_array().ok_or_else(|| {
            ResponsesError::invalid_request("tools must be an array", Some("tools"))
        })?;

        for tool in tools_arr {
            let tool_type = tool
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("function");
            if tool_type != "function" {
                return Err(ResponsesError::unsupported(
                    format!("unsupported tool type '{tool_type}': only function tools are supported"),
                    Some("tools"),
                ));
            }

            let name = tool
                .get("function")
                .and_then(|f| f.get("name"))
                .or_else(|| tool.get("name"))
                .and_then(Value::as_str);
            let name = name
                .filter(|n| !n.trim().is_empty())
                .ok_or_else(|| {
                    ResponsesError::invalid_request("tool function missing name", Some("tools"))
                })?;

            let description = tool
                .get("function")
                .and_then(|f| f.get("description"))
                .or_else(|| tool.get("description"))
                .cloned();

            let parameters = tool
                .get("function")
                .and_then(|f| f.get("parameters"))
                .or_else(|| tool.get("parameters"))
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));

            let mut function = json!({
                "name": name,
                "parameters": parameters,
            });
            if let Some(desc) = description {
                function["description"] = desc;
            }
            if let Some(strict) = tool
                .get("function")
                .and_then(|f| f.get("strict"))
                .or_else(|| tool.get("strict"))
            {
                function["strict"] = strict.clone();
            }

            mapped_tools.push(json!({
                "type": "function",
                "function": function,
            }));
        }
    }

    // Tool choice mapping
    let mapped_tool_choice = if let Some(choice) = obj.get("tool_choice") {
        match choice {
            Value::String(_) => Some(choice.clone()),
            Value::Object(map) => {
                if let Some(name) = map.get("name").and_then(Value::as_str) {
                    Some(json!({
                        "type": "function",
                        "function": {"name": name}
                    }))
                } else {
                    Some(choice.clone())
                }
            }
            _ => Some(choice.clone()),
        }
    } else {
        None
    };

    // ─── 3. Instructions & Messages Normalization ────────────────────────────
    let mut messages = Vec::new();

    if let Some(instructions) = obj.get("instructions").and_then(Value::as_str) {
        if !instructions.is_empty() {
            messages.push(json!({
                "role": "system",
                "content": instructions,
            }));
        }
    }

    let mut defined_tool_calls: HashSet<String> = HashSet::new();
    let mut answered_tool_calls: HashSet<String> = HashSet::new();

    match obj.get("input") {
        Some(Value::String(text)) => {
            messages.push(json!({
                "role": "user",
                "content": text,
            }));
        }
        Some(Value::Array(items)) => {
            for item in items {
                let item_type = item.get("type").and_then(Value::as_str);

                match item_type {
                    Some("function_call") => {
                        let call_id = item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(Value::as_str)
                            .filter(|id| !id.trim().is_empty())
                            .ok_or_else(|| {
                                ResponsesError::invalid_request(
                                    "missing function_call call_id",
                                    Some("input"),
                                )
                            })?;

                        if !defined_tool_calls.insert(call_id.to_string()) {
                            return Err(ResponsesError::invalid_request(
                                format!("duplicate function_call call_id: '{call_id}'"),
                                Some("input"),
                            ));
                        }

                        let name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .filter(|n| !n.trim().is_empty())
                            .ok_or_else(|| {
                                ResponsesError::invalid_request(
                                    "missing function_call name",
                                    Some("input"),
                                )
                            })?;

                        let arguments = match item.get("arguments") {
                            Some(Value::String(s)) => {
                                // Enforce valid JSON argument structure: reject malformed histories
                                if let Err(err) = serde_json::from_str::<Value>(s) {
                                    return Err(ResponsesError::invalid_request(
                                        format!(
                                            "malformed JSON in function_call arguments for '{name}': {err}"
                                        ),
                                        Some("input"),
                                    ));
                                }
                                s.clone()
                            }
                            Some(Value::Object(map)) => Value::Object(map.clone()).to_string(),
                            Some(_) => {
                                return Err(ResponsesError::invalid_request(
                                    format!("invalid function_call arguments type for '{name}': must be a JSON string or object"),
                                    Some("input"),
                                ));
                            }
                            None => "{}".to_string(),
                        };

                        messages.push(json!({
                            "role": "assistant",
                            "content": Value::Null,
                            "tool_calls": [{
                                "id": call_id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": arguments,
                                }
                            }]
                        }));
                    }
                    Some("function_call_output") => {
                        let call_id = item
                            .get("call_id")
                            .or_else(|| item.get("tool_call_id"))
                            .and_then(Value::as_str)
                            .filter(|id| !id.trim().is_empty())
                            .ok_or_else(|| {
                                ResponsesError::invalid_request(
                                    "missing function_call_output call_id",
                                    Some("input"),
                                )
                            })?;

                        if !defined_tool_calls.contains(call_id) {
                            return Err(ResponsesError::invalid_request(
                                format!("unmatched function_call_output for call_id: '{call_id}' (no preceding function_call)"),
                                Some("input"),
                            ));
                        }
                        if !answered_tool_calls.insert(call_id.to_string()) {
                            return Err(ResponsesError::invalid_request(
                                format!("duplicate function_call_output for call_id: '{call_id}'"),
                                Some("input"),
                            ));
                        }

                        let output = match item.get("output") {
                            Some(Value::String(s)) => s.clone(),
                            Some(val) => val.to_string(),
                            None => String::new(),
                        };

                        let mut tool_msg = json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "content": output,
                        });
                        if item.get("is_error").and_then(Value::as_bool) == Some(true)
                            || item.get("status").and_then(Value::as_str) == Some("error")
                        {
                            tool_msg["is_error"] = json!(true);
                        }
                        messages.push(tool_msg);
                    }
                    _ => {
                        // Regular message turn
                        let role = item
                            .get("role")
                            .and_then(Value::as_str)
                            .unwrap_or("user");

                        let mut text_parts = Vec::new();
                        let mut image_parts = Vec::new();
                        let mut reasoning_text: Option<String> = None;
                        let mut reasoning_signature: Option<String> = None;
                        let mut reasoning_redacted = item
                            .get("reasoning_redacted")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);

                        if let Some(r) = item
                            .get("reasoning_content")
                            .or_else(|| item.get("thinking"))
                            .and_then(Value::as_str)
                        {
                            reasoning_text = Some(r.to_string());
                        }
                        if let Some(sig) = item
                            .get("reasoning_signature")
                            .or_else(|| item.get("signature"))
                            .and_then(Value::as_str)
                        {
                            reasoning_signature = Some(sig.to_string());
                        }

                        match item.get("content") {
                            Some(Value::String(s)) => {
                                text_parts.push(s.clone());
                            }
                            Some(Value::Array(parts)) => {
                                for part in parts {
                                    let ptype = part.get("type").and_then(Value::as_str);
                                    match ptype {
                                        Some("input_text") | Some("text") | Some("output_text") => {
                                            if let Some(t) = part.get("text").and_then(Value::as_str) {
                                                text_parts.push(t.to_string());
                                            }
                                        }
                                        Some("input_image") | Some("image_url") => {
                                            let url_opt = part
                                                .get("image_url")
                                                .and_then(|u| {
                                                    u.get("url").and_then(Value::as_str).or_else(|| u.as_str())
                                                })
                                                .or_else(|| part.get("data").and_then(Value::as_str))
                                                .or_else(|| part.get("url").and_then(Value::as_str));

                                            let Some(url) = url_opt else {
                                                return Err(ResponsesError::invalid_request(
                                                    "missing image_url",
                                                    Some("input"),
                                                ));
                                            };

                                            if !url.starts_with("data:") {
                                                return Err(ResponsesError::unsupported(
                                                    "remote image url is unsupported",
                                                    Some("input"),
                                                ));
                                            }

                                            if split_data_url(url).is_none() {
                                                return Err(ResponsesError::invalid_request(
                                                    "invalid base64 data url for image",
                                                    Some("input"),
                                                ));
                                            }

                                            image_parts.push(json!({
                                                "type": "image_url",
                                                "image_url": {"url": url}
                                            }));
                                        }
                                        Some("reasoning") | Some("thinking") => {
                                            if let Some(r) = part
                                                .get("reasoning")
                                                .or_else(|| part.get("thinking"))
                                                .or_else(|| part.get("text"))
                                                .and_then(Value::as_str)
                                            {
                                                reasoning_text = Some(r.to_string());
                                            }
                                            if let Some(sig) = part.get("signature").and_then(Value::as_str) {
                                                reasoning_signature = Some(sig.to_string());
                                            }
                                            if part.get("redacted").and_then(Value::as_bool) == Some(true) {
                                                reasoning_redacted = true;
                                            }
                                        }
                                        Some("reasoning_redacted") => {
                                            reasoning_redacted = true;
                                        }
                                        _ => {
                                            if let Some(t) = part.get("text").and_then(Value::as_str) {
                                                text_parts.push(t.to_string());
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }

                        let combined_text = text_parts.join("");
                        let mut msg = json!({"role": role});

                        if !image_parts.is_empty() {
                            let mut full_parts = Vec::new();
                            if !combined_text.is_empty() {
                                full_parts.push(json!({"type": "text", "text": combined_text}));
                            }
                            full_parts.extend(image_parts);
                            msg["content"] = Value::Array(full_parts);
                        } else {
                            msg["content"] = Value::String(combined_text);
                        }

                        if role == "assistant" {
                            if let Some(r) = reasoning_text {
                                msg["reasoning_content"] = json!(r);
                            }
                            if let Some(sig) = reasoning_signature {
                                msg["reasoning_signature"] = json!(sig);
                            }
                            if reasoning_redacted {
                                msg["reasoning_redacted"] = json!(true);
                            }
                        }

                        messages.push(msg);
                    }
                }
            }
        }
        None => {}
        Some(other) => {
            return Err(ResponsesError::invalid_request(
                format!("invalid input type: expected string or array, got {other:?}"),
                Some("input"),
            ));
        }
    }

    // ─── 4. Build Chat Completions Request Body ──────────────────────────────
    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("devin/glm-5-2");

    let mut chat = json!({
        "model": model,
        "messages": messages,
        "stream": obj.get("stream").and_then(Value::as_bool).unwrap_or(false),
    });

    if !mapped_tools.is_empty() {
        chat["tools"] = Value::Array(mapped_tools);
    }
    if let Some(choice) = mapped_tool_choice {
        chat["tool_choice"] = choice;
    }

    if let Some(t) = obj.get("temperature") {
        chat["temperature"] = t.clone();
    }
    if let Some(p) = obj.get("top_p") {
        chat["top_p"] = p.clone();
    }
    if let Some(max_out) = obj
        .get("max_output_tokens")
        .or_else(|| obj.get("max_tokens"))
    {
        chat["max_tokens"] = max_out.clone();
    }

    Ok(chat)
}

// ─── Stream Renderer Helpers ─────────────────────────────────────────────────

fn sse_frame(event_name: &str, payload: &Value) -> Bytes {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(b"event: ");
    buf.extend_from_slice(event_name.as_bytes());
    buf.extend_from_slice(b"\ndata: ");
    if serde_json::to_writer(&mut buf, payload).is_err() {
        return Bytes::new();
    }
    buf.extend_from_slice(b"\n\n");
    Bytes::from(buf)
}

fn build_usage_json(usage: Option<&Usage>) -> Value {
    match usage {
        None => Value::Null,
        Some(u) => json!({
            "input_tokens": u.prompt_tokens,
            "output_tokens": u.completion_tokens,
            "total_tokens": u.total_tokens,
            "input_tokens_details": {
                "cached_tokens": u.cached_tokens,
            },
            "output_tokens_details": {
                "reasoning_tokens": u.reasoning_tokens,
            }
        }),
    }
}

#[derive(Debug, Clone, PartialEq)]
enum TextItemState {
    Unstarted,
    Open { item_id: String, output_index: u64 },
    Done { item_id: String, output_index: u64 },
}

#[derive(Debug, Clone)]
struct ToolCallItem {
    item_id: String,
    output_index: u64,
    decoder_index: u64,
    call_id: String,
    name: String,
    arguments: String,
    args_done_emitted: bool,
    item_done_emitted: bool,
}

/// Renderer translating upstream [`CodexEvent`] streams into standard OpenAI
/// Responses SSE frames.
pub struct ResponsesStreamRenderer {
    id: String,
    model: String,
    created: i64,
    created_sent: bool,
    terminated: bool,
    output_limit_reached: bool,
    failure: Option<String>,
    usage: Option<Usage>,
    text_accumulator: String,
    reasoning_accumulator: String,
    reasoning_signature: Option<String>,
    reasoning_redacted: bool,
    text_state: TextItemState,
    tools: Vec<ToolCallItem>,
    next_output_index: u64,
}

impl ResponsesStreamRenderer {
    pub fn new(model: String, created: i64) -> Self {
        Self {
            id: format!("resp_{created}"),
            model,
            created,
            created_sent: false,
            terminated: false,
            output_limit_reached: false,
            failure: None,
            usage: None,
            text_accumulator: String::new(),
            reasoning_accumulator: String::new(),
            reasoning_signature: None,
            reasoning_redacted: false,
            text_state: TextItemState::Unstarted,
            tools: Vec::new(),
            next_output_index: 0,
        }
    }

    pub fn terminated(&self) -> bool {
        self.terminated
    }

    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    fn ensure_created_frame(&mut self, out: &mut Vec<Bytes>) {
        if !self.created_sent {
            self.created_sent = true;
            out.push(sse_frame(
                "response.created",
                &json!({
                    "type": "response.created",
                    "response": {
                        "id": self.id,
                        "object": "response",
                        "status": "in_progress",
                        "model": self.model,
                    }
                }),
            ));
        }
    }

    fn close_text_item(&mut self, out: &mut Vec<Bytes>) {
        if let TextItemState::Open {
            ref item_id,
            output_index,
        } = self.text_state
        {
            let item_status = if self.output_limit_reached {
                "incomplete"
            } else {
                "completed"
            };
            out.push(sse_frame(
                "response.output_item.done",
                &json!({
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "item": {
                        "id": item_id,
                        "type": "message",
                        "status": item_status,
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": self.text_accumulator,
                        }]
                    }
                }),
            ));
            self.text_state = TextItemState::Done {
                item_id: item_id.clone(),
                output_index,
            };
        }
    }

    fn close_tool_item(&mut self, idx: usize, out: &mut Vec<Bytes>) {
        if idx >= self.tools.len() {
            return;
        }
        let tool = &mut self.tools[idx];
        if !tool.args_done_emitted {
            tool.args_done_emitted = true;
            out.push(sse_frame(
                "response.function_call_arguments.done",
                &json!({
                    "type": "response.function_call_arguments.done",
                    "item_id": tool.item_id,
                    "output_index": tool.output_index,
                    "arguments": tool.arguments,
                }),
            ));
        }
        if !tool.item_done_emitted {
            tool.item_done_emitted = true;
            let item_status = if self.output_limit_reached {
                "incomplete"
            } else {
                "completed"
            };
            out.push(sse_frame(
                "response.output_item.done",
                &json!({
                    "type": "response.output_item.done",
                    "output_index": tool.output_index,
                    "item": {
                        "id": tool.item_id,
                        "type": "function_call",
                        "status": item_status,
                        "arguments": tool.arguments,
                        "call_id": tool.call_id,
                        "name": tool.name,
                    }
                }),
            ));
        }
    }

    pub fn render(&mut self, event: CodexEvent) -> Vec<Bytes> {
        if self.terminated {
            return Vec::new();
        }

        let mut out = Vec::new();

        match event {
            CodexEvent::Created { response_id } => {
                if !response_id.is_empty() {
                    self.id = if response_id.starts_with("resp_") {
                        response_id
                    } else {
                        format!("resp_{response_id}")
                    };
                }
                self.ensure_created_frame(&mut out);
            }
            CodexEvent::ReasoningDelta(delta) => {
                self.ensure_created_frame(&mut out);
                self.reasoning_accumulator.push_str(&delta);
                out.push(sse_frame(
                    "response.reasoning_text.delta",
                    &json!({
                        "type": "response.reasoning_text.delta",
                        "delta": delta,
                        "output_index": 0,
                    }),
                ));
            }
            CodexEvent::ReasoningSignature(sig) => {
                self.reasoning_signature = Some(sig);
            }
            CodexEvent::ReasoningRedacted => {
                self.reasoning_redacted = true;
            }
            CodexEvent::TextDelta(delta) => {
                self.ensure_created_frame(&mut out);

                let (item_id, output_index) = match self.text_state {
                    TextItemState::Unstarted => {
                        let out_idx = self.next_output_index;
                        self.next_output_index += 1;
                        let id = format!("msg_{out_idx}");
                        out.push(sse_frame(
                            "response.output_item.added",
                            &json!({
                                "type": "response.output_item.added",
                                "output_index": out_idx,
                                "item": {
                                    "id": id,
                                    "type": "message",
                                    "status": "in_progress",
                                    "role": "assistant",
                                    "content": [],
                                }
                            }),
                        ));
                        out.push(sse_frame(
                            "response.content_part.added",
                            &json!({
                                "type": "response.content_part.added",
                                "output_index": out_idx,
                                "content_index": 0,
                                "item_id": id,
                                "part": {
                                    "type": "output_text",
                                    "text": "",
                                }
                            }),
                        ));
                        self.text_state = TextItemState::Open {
                            item_id: id.clone(),
                            output_index: out_idx,
                        };
                        (id, out_idx)
                    }
                    TextItemState::Open {
                        ref item_id,
                        output_index,
                    } => (item_id.clone(), output_index),
                    TextItemState::Done {
                        ref item_id,
                        output_index,
                    } => {
                        // Re-open if subsequent text arrives
                        (item_id.clone(), output_index)
                    }
                };

                self.text_accumulator.push_str(&delta);
                out.push(sse_frame(
                    "response.output_text.delta",
                    &json!({
                        "type": "response.output_text.delta",
                        "output_index": output_index,
                        "content_index": 0,
                        "item_id": item_id,
                        "delta": delta,
                    }),
                ));
            }
            CodexEvent::ToolCallBegin {
                output_index: decoder_idx,
                call_id,
                name,
            } => {
                self.ensure_created_frame(&mut out);
                // Close any open text item before starting a tool call
                self.close_text_item(&mut out);

                let resp_output_idx = self.next_output_index;
                self.next_output_index += 1;
                let item_id = format!("fc_{resp_output_idx}");

                let tool_item = ToolCallItem {
                    item_id: item_id.clone(),
                    output_index: resp_output_idx,
                    decoder_index: decoder_idx,
                    call_id: call_id.clone(),
                    name: name.clone(),
                    arguments: String::new(),
                    args_done_emitted: false,
                    item_done_emitted: false,
                };
                self.tools.push(tool_item);

                out.push(sse_frame(
                    "response.output_item.added",
                    &json!({
                        "type": "response.output_item.added",
                        "output_index": resp_output_idx,
                        "item": {
                            "id": item_id,
                            "type": "function_call",
                            "status": "in_progress",
                            "arguments": "",
                            "call_id": call_id,
                            "name": name,
                        }
                    }),
                ));
            }
            CodexEvent::ToolArgsDelta {
                output_index: decoder_idx,
                delta,
            } => {
                self.ensure_created_frame(&mut out);

                if let Some(tool) = self
                    .tools
                    .iter_mut()
                    .find(|t| t.decoder_index == decoder_idx && !t.args_done_emitted)
                {
                    tool.arguments.push_str(&delta);
                    out.push(sse_frame(
                        "response.function_call_arguments.delta",
                        &json!({
                            "type": "response.function_call_arguments.delta",
                            "output_index": tool.output_index,
                            "item_id": tool.item_id,
                            "delta": delta,
                        }),
                    ));
                }
            }
            CodexEvent::OutputLimitReached => {
                self.output_limit_reached = true;
            }
            CodexEvent::Completed { usage } => {
                self.ensure_created_frame(&mut out);
                self.close_text_item(&mut out);
                for i in 0..self.tools.len() {
                    self.close_tool_item(i, &mut out);
                }

                self.usage = usage;
                self.terminated = true;

                let resp_val = self.current_response();
                if self.output_limit_reached {
                    out.push(sse_frame(
                        "response.incomplete",
                        &json!({
                            "type": "response.incomplete",
                            "response": resp_val,
                        }),
                    ));
                } else {
                    out.push(sse_frame(
                        "response.completed",
                        &json!({
                            "type": "response.completed",
                            "response": resp_val,
                        }),
                    ));
                }
            }
            CodexEvent::Failed { message } => {
                self.close_text_item(&mut out);
                for i in 0..self.tools.len() {
                    self.close_tool_item(i, &mut out);
                }

                self.failure = Some(message.clone());
                self.terminated = true;

                out.push(sse_frame(
                    "response.failed",
                    &json!({
                        "type": "response.failed",
                        "response": {
                            "id": self.id,
                            "object": "response",
                            "status": "failed",
                            "error": {
                                "message": message,
                                "type": "server_error",
                            }
                        }
                    }),
                ));
            }
        }

        out
    }

    pub fn close_unterminated(&mut self) -> Vec<Bytes> {
        if self.terminated {
            return Vec::new();
        }
        let mut out = Vec::new();
        self.ensure_created_frame(&mut out);
        self.close_text_item(&mut out);
        for i in 0..self.tools.len() {
            self.close_tool_item(i, &mut out);
        }
        self.terminated = true;

        let resp_val = self.current_response();
        if self.output_limit_reached {
            out.push(sse_frame(
                "response.incomplete",
                &json!({
                    "type": "response.incomplete",
                    "response": resp_val,
                }),
            ));
        } else {
            out.push(sse_frame(
                "response.completed",
                &json!({
                    "type": "response.completed",
                    "response": resp_val,
                }),
            ));
        }
        out
    }

    /// Assembles the complete canonical Responses JSON response object.
    pub fn current_response(&self) -> Value {
        let mut output_items = Vec::new();

        if !matches!(self.text_state, TextItemState::Unstarted) || !self.text_accumulator.is_empty()
        {
            let (item_id, item_status) = match self.text_state {
                TextItemState::Open { ref item_id, .. } | TextItemState::Done { ref item_id, .. } => (
                    item_id.clone(),
                    if self.output_limit_reached {
                        "incomplete"
                    } else {
                        "completed"
                    },
                ),
                TextItemState::Unstarted => (
                    "msg_0".to_string(),
                    if self.output_limit_reached {
                        "incomplete"
                    } else {
                        "completed"
                    },
                ),
            };

            let mut msg_obj = json!({
                "id": item_id,
                "type": "message",
                "status": item_status,
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": self.text_accumulator,
                }]
            });
            if !self.reasoning_accumulator.is_empty() {
                msg_obj["reasoning_content"] = json!(self.reasoning_accumulator);
            }
            if let Some(ref sig) = self.reasoning_signature {
                msg_obj["reasoning_signature"] = json!(sig);
            }
            if self.reasoning_redacted {
                msg_obj["reasoning_redacted"] = json!(true);
            }
            output_items.push(msg_obj);
        }

        for tool in &self.tools {
            let status = if self.output_limit_reached {
                "incomplete"
            } else {
                "completed"
            };
            output_items.push(json!({
                "id": tool.item_id,
                "type": "function_call",
                "status": status,
                "call_id": tool.call_id,
                "name": tool.name,
                "arguments": tool.arguments,
            }));
        }

        let status = if self.failure.is_some() {
            "failed"
        } else if self.output_limit_reached {
            "incomplete"
        } else if self.terminated {
            "completed"
        } else {
            "in_progress"
        };

        let mut payload = json!({
            "id": self.id,
            "object": "response",
            "created_at": self.created,
            "status": status,
            "model": self.model,
            "output": output_items,
            "usage": build_usage_json(self.usage.as_ref()),
        });

        if self.output_limit_reached {
            payload["incomplete_details"] = json!({"reason": "max_output_tokens"});
        }
        if let Some(ref err) = self.failure {
            payload["error"] = json!({"message": err, "type": "server_error"});
        }

        payload
    }

    pub fn into_response(self) -> Value {
        self.current_response()
    }
}

/// Aggregates an entire stream of [`CodexEvent`]s into a single canonical Responses
/// JSON response object.
///
/// Reuses [`ResponsesStreamRenderer`] directly to guarantee that stream and
/// non-stream representations are perfectly identical in IDs, structure, and lifecycle.
pub fn responses_response(
    events: &[CodexEvent],
    model: &str,
    created: i64,
) -> Result<Value, String> {
    let mut renderer = ResponsesStreamRenderer::new(model.to_string(), created);
    for event in events {
        renderer.render(event.clone());
        if renderer.terminated() {
            break;
        }
    }
    if !renderer.terminated() {
        renderer.close_unterminated();
    }
    if let Some(err) = renderer.failure() {
        return Err(err.to_string());
    }
    Ok(renderer.current_response())
}
