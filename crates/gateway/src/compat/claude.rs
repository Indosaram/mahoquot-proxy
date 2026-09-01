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
    input_tokens: u64,
    output_tokens: u64,
    next_tool_index: u64,
}

impl AnthropicDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn decode(&mut self, frame: &[u8], out: &mut Vec<CodexEvent>) {
        let Ok(value) = serde_json::from_slice::<Value>(frame) else {
            return;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                self.started = true;
                self.input_tokens = value["message"]["usage"]["input_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                out.push(CodexEvent::Created {
                    response_id: value["message"]["id"]
                        .as_str()
                        .unwrap_or("msg_anthropic")
                        .to_string(),
                });
            }
            Some("content_block_start") if value["content_block"]["type"] == "tool_use" => {
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
                        out.push(CodexEvent::TextDelta(text.to_string()));
                    }
                }
                Some("thinking_delta") | Some("reasoning_delta") => {
                    if let Some(text) = value["delta"]["thinking"]
                        .as_str()
                        .or_else(|| value["delta"]["text"].as_str())
                    {
                        out.push(CodexEvent::ReasoningDelta(text.to_string()));
                    }
                }
                Some("signature_delta") => {
                    if let Some(signature) = value["delta"]["signature"].as_str() {
                        out.push(CodexEvent::ReasoningSignature(signature.to_string()));
                    }
                }
                Some("input_json_delta") => {
                    if let Some(delta) = value["delta"]["partial_json"].as_str() {
                        out.push(CodexEvent::ToolArgsDelta {
                            output_index: self.next_tool_index.saturating_sub(1),
                            delta: delta.to_string(),
                        });
                    }
                }
                _ => {}
            },
            Some("message_delta") => {
                self.output_tokens = value["usage"]["output_tokens"].as_u64().unwrap_or(0);
            }
            Some("message_stop") => {
                self.started = false;
                out.push(CodexEvent::Completed {
                    usage: Some(Usage {
                        prompt_tokens: self.input_tokens,
                        completion_tokens: self.output_tokens,
                        total_tokens: self.input_tokens + self.output_tokens,
                        cached_tokens: 0,
                        reasoning_tokens: 0,
                    }),
                });
            }
            Some("error") => out.push(CodexEvent::Failed {
                message: value["error"]["message"]
                    .as_str()
                    .unwrap_or("Anthropic upstream error")
                    .to_string(),
            }),
            _ => {}
        }
    }

    pub fn finish(&mut self, out: &mut Vec<CodexEvent>) {
        if self.started {
            self.started = false;
            out.push(CodexEvent::Completed { usage: None });
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
        let content = msg.get("content");

        let mut text_parts: Vec<String> = Vec::new();
        let mut tool_calls: Vec<Value> = Vec::new();
        let mut tool_results: Vec<Value> = Vec::new();

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
                            let text = block
                                .get("content")
                                .and_then(|c| match c {
                                    Value::String(s) => Some(s.clone()),
                                    Value::Array(items) => Some(
                                        items
                                            .iter()
                                            .filter_map(|i| i.get("text").and_then(Value::as_str))
                                            .collect::<Vec<_>>()
                                            .join(""),
                                    ),
                                    _ => None,
                                })
                                .unwrap_or_default();
                            tool_results.push(json!({
                                "role": "tool",
                                "tool_call_id": id,
                                "content": text
                            }));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }

        if !tool_calls.is_empty() {
            let mut m = Map::new();
            m.insert("role".to_string(), json!("assistant"));
            m.insert("content".to_string(), json!(text_parts.join("")));
            m.insert("tool_calls".to_string(), Value::Array(tool_calls));
            out_messages.push(Value::Object(m));
        } else if !text_parts.is_empty() {
            out_messages.push(json!({ "role": role, "content": text_parts.join("") }));
        }

        out_messages.extend(tool_results);
    }

    let mut out = Map::new();
    out.insert("model".to_string(), json!(model));
    out.insert("messages".to_string(), Value::Array(out_messages));

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

    Ok(Value::Object(out))
}

pub fn openai_to_anthropic(body: &Value) -> Result<Value, String> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing model".to_string())?;
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing messages".to_string())?;
    let mut system = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        if role == "system" || role == "developer" {
            if let Some(text) = message.get("content").and_then(content_text) {
                system.push(text.to_string());
            }
            continue;
        }
        if role == "tool" {
            let block = json!({
                "type": "tool_result",
                "tool_use_id": message.get("tool_call_id").and_then(Value::as_str).unwrap_or(""),
                "content": message.get("content").cloned().unwrap_or(Value::String(String::new())),
            });
            if let Some(last) = out.last_mut().filter(|last| last["role"] == "user") {
                if let Some(content) = last.get_mut("content").and_then(Value::as_array_mut) {
                    content.push(block);
                    continue;
                }
            }
            out.push(json!({"role": "user", "content": [block]}));
            continue;
        }
        let mut content = match message.get("content") {
            Some(Value::String(text)) => vec![json!({"type":"text","text":text})],
            Some(Value::Array(blocks)) => blocks.iter().map(openai_content_to_anthropic).collect(),
            _ => Vec::new(),
        };
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let input = call["function"]["arguments"]
                    .as_str()
                    .and_then(|raw| serde_json::from_str(raw).ok())
                    .unwrap_or_else(|| json!({}));
                content.push(json!({
                    "type": "tool_use",
                    "id": call["id"],
                    "name": call["function"]["name"],
                    "input": input,
                }));
            }
        }
        out.push(json!({"role": role, "content": content}));
    }
    let tools: Vec<Value> = body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| tool.get("function"))
        .map(|function| {
            let name = anthropic_tool_name(function["name"].as_str().unwrap_or("tool"));
            json!({
                "name": name,
                "description": function["description"],
                "input_schema": function.get("parameters").cloned().unwrap_or_else(|| json!({"type":"object"})),
            })
        })
        .collect();
    // Anthropic-style upstreams require max_tokens, but OpenAI-agent clients
    // often omit it. A small default truncates reasoning models mid-response
    // (thinking burns the output budget, so a tool_use block arrives cut off
    // and the agent loop stalls), so default generously — billing follows
    // actual output, not the cap.
    let mut result = json!({
        "model": model,
        "messages": out,
        "max_tokens": body.get("max_tokens").and_then(Value::as_u64).unwrap_or(32_768),
        "stream": body.get("stream").and_then(Value::as_bool).unwrap_or(false),
    });
    if !system.is_empty() {
        result["system"] = Value::String(system.join("\n\n"));
    }
    if !tools.is_empty() {
        result["tools"] = Value::Array(tools);
    }
    for key in ["temperature", "top_p", "stop_sequences"] {
        if let Some(value) = body.get(key) {
            result[key] = value.clone();
        }
    }
    // Reasoning effort maps to the Anthropic thinking budget both upstreams
    // (api.anthropic.com and z.ai's /api/anthropic) accept. The thinking block
    // must fit inside max_tokens (budget < max_tokens, plus room for the
    // answer), so a too-small client max_tokens is raised to make room.
    if let Some(effort) = body.get("reasoning_effort").and_then(Value::as_str) {
        let budget: u64 = match effort {
            "minimal" => 1024,
            "low" => 2048,
            "medium" => 8192,
            "high" => 16384,
            "xhigh" => 49152,
            "max" => 32768,
            _ => 0,
        };
        if budget > 0 {
            let max_tokens = body
                .get("max_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(32_768);
            let needed = budget + 4096;
            if max_tokens <= budget {
                result["max_tokens"] = json!(needed);
            }
            result["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
        }
    }
    Ok(result)
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

fn openai_content_to_anthropic(part: &Value) -> Value {
    if part["type"] != "image_url" {
        return part.clone();
    }
    let Some(url) = part["image_url"]["url"].as_str() else {
        return part.clone();
    };
    let Some((media_type, data)) = super::split_data_url(url) else {
        return part.clone();
    };
    json!({
        "type": "image",
        "source": {"type":"base64", "media_type":media_type, "data":data},
    })
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
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
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
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    json!({
        "id": body.get("id").cloned().unwrap_or_else(|| json!(format!("chatcmpl-{created}"))),
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": if body["stop_reason"] == "tool_use" { "tool_calls" } else { "stop" },
        }],
        "usage": {
            "prompt_tokens": body["usage"]["input_tokens"].as_u64().unwrap_or(0),
            "completion_tokens": body["usage"]["output_tokens"].as_u64().unwrap_or(0),
            "total_tokens": body["usage"]["input_tokens"].as_u64().unwrap_or(0)
                + body["usage"]["output_tokens"].as_u64().unwrap_or(0),
        }
    })
}

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
    let mut content: Vec<Value> = Vec::new();
    // Anthropic orders the thinking block ahead of the visible answer.
    if let Some(sig) = reasoning_signature {
        content.push(json!({ "type": "thinking", "thinking": "", "signature": sig }));
    }
    if !text.is_empty() {
        content.push(json!({ "type": "text", "text": text }));
    }
    for (id, name, args) in tool_calls {
        let input = serde_json::from_str::<Value>(args).unwrap_or_else(|_| json!({}));
        content.push(json!({
            "type": "tool_use",
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
        "usage": {
            "input_tokens": usage.map(|u| u.prompt_tokens).unwrap_or(0),
            "output_tokens": usage.map(|u| u.completion_tokens).unwrap_or(0),
        }
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
                "usage": {
                    "input_tokens": final_usage.as_ref().map(|u| u.prompt_tokens).unwrap_or(0),
                    "output_tokens": final_usage.as_ref().map(|u| u.completion_tokens).unwrap_or(0),
                }
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
            CodexEvent::ToolCallBegin { .. } | CodexEvent::ToolArgsDelta { .. } => {
                finish = "tool_calls".to_string();
            }
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

fn sse(event: &str, payload: &Value) -> String {
    format!("event: {event}\ndata: {payload}\n\n")
}

pub struct AnthropicStreamRenderer {
    id: String,
    model: String,
    started: bool,
    text_open: bool,
    terminated: bool,
    tool_index: u64,
    next_content_index: u64,
    current_tool_index: Option<u64>,
    thinking_index: Option<u64>,
}

impl AnthropicStreamRenderer {
    pub fn new(model: String, created: i64) -> Self {
        Self {
            id: format!("msg_{created}"),
            model,
            started: false,
            text_open: false,
            terminated: false,
            tool_index: 0,
            next_content_index: 1,
            current_tool_index: None,
            thinking_index: None,
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
        if self.text_open {
            self.text_open = false;
            out.push(Self::frame(
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
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

    fn close_open_tool(&mut self, out: &mut Vec<bytes::Bytes>) {
        if let Some(index) = self.current_tool_index.take() {
            out.push(Self::frame(
                "content_block_stop",
                json!({"type":"content_block_stop","index":index}),
            ));
        }
    }

    pub fn render(&mut self, event: CodexEvent) -> Vec<bytes::Bytes> {
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
                self.close_open_tool(&mut out);
                if !self.text_open {
                    self.text_open = true;
                    out.push(Self::frame(
                        "content_block_start",
                        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    ));
                }
                out.push(Self::frame(
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
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
            CodexEvent::ToolCallBegin { call_id, name, .. } => {
                self.close_thinking(&mut out);
                self.close_text(&mut out);
                self.close_open_tool(&mut out);
                let index = self.next_content_index;
                self.next_content_index += 1;
                self.tool_index += 1;
                self.current_tool_index = Some(index);
                out.push(Self::frame(
                    "content_block_start",
                    json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":call_id,"name":name,"input":{}}}),
                ));
            }
            CodexEvent::ToolArgsDelta { delta, .. } => {
                let index = self.current_tool_index.unwrap_or(1);
                out.push(Self::frame(
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":delta}}),
                ));
            }
            CodexEvent::Completed { usage } => {
                self.close_thinking(&mut out);
                self.close_text(&mut out);
                self.close_open_tool(&mut out);
                let stop = if self.tool_index > 0 {
                    "tool_use"
                } else {
                    "end_turn"
                };
                out.push(Self::frame(
                    "message_delta",
                    json!({"type":"message_delta","delta":{"stop_reason":stop,"stop_sequence":Value::Null},"usage":{"output_tokens":usage.map(|u|u.completion_tokens).unwrap_or(0)}}),
                ));
                out.push(Self::frame("message_stop", json!({"type":"message_stop"})));
                self.terminated = true;
            }
            CodexEvent::Failed { message } => {
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
        self.close_open_tool(out);
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
        self.render(CodexEvent::Completed { usage: None })
    }

    pub fn terminated(&self) -> bool {
        self.terminated
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;

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
            json!({"type": "enabled", "budget_tokens": 2048})
        );
        // low already fits under the client max_tokens; only max needs the raise
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
            json!({"type": "enabled", "budget_tokens": 32768})
        );
        assert_eq!(out["max_tokens"], json!(36864));
    }

    #[test]
    fn a_tiny_max_tokens_is_raised_to_make_room_for_thinking() {
        let body = json!({
            "model": "glm-5.3-flash",
            "reasoning_effort": "high",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = openai_to_anthropic(&body).unwrap();
        assert_eq!(
            out["thinking"],
            json!({"type": "enabled", "budget_tokens": 16384})
        );
        assert_eq!(out["max_tokens"], json!(20480));
    }

    #[test]
    fn unknown_efforts_are_ignored() {
        let body = json!({
            "model": "glm-5.3-flash",
            "reasoning_effort": "turbo",
            "max_tokens": 4096,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = openai_to_anthropic(&body).unwrap();
        assert!(out.get("thinking").is_none());
        assert_eq!(out["max_tokens"], json!(4096));
    }
}
