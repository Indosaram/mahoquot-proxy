use base64::Engine as _;
use serde_json::{json, Map, Value};

use super::events::{CodexEvent, Usage};

const SIGNATURE_ID_SEPARATOR: char = '#';

fn embed_signature_in_call_id(id: &str, signature: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "{id}{SIGNATURE_ID_SEPARATOR}{}",
        URL_SAFE_NO_PAD.encode(signature.as_bytes())
    )
}

fn split_signature_from_call_id(id: &str) -> (String, Option<String>) {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let Some((plain, encoded)) = id.split_once(SIGNATURE_ID_SEPARATOR) else {
        return (id.to_string(), None);
    };
    let decoded = URL_SAFE_NO_PAD
        .decode(encoded)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok());
    (plain.to_string(), decoded)
}

/// Gemini's functionResponse.response is a google.protobuf.Struct, which
/// only accepts a JSON object. Tool payloads that parse to a bare scalar or
/// array must be wrapped, or the upstream rejects the whole request with
/// INVALID_ARGUMENT.
fn struct_response(raw: &str) -> Value {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(map)) => Value::Object(map),
        Ok(value) => json!({ "result": value }),
        Err(_) => json!({ "result": raw }),
    }
}

pub fn openai_to_gemini(body: &Value) -> Result<Value, String> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing model".to_string())?;

    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing messages".to_string())?;

    let mut contents: Vec<Value> = Vec::new();
    let mut system_parts: Vec<Value> = Vec::new();
    let empty: Vec<Value> = Vec::new();

    // OpenAI tool messages carry `tool_call_id`, not `name`; Gemini's
    // functionResponse needs the originating function name, so index every
    // assistant tool_call by id up front.
    let mut call_names: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    // Gemini 3.x hard-rejects (400 INVALID_ARGUMENT) any historical
    // functionCall part without a thoughtSignature. When the client loses
    // the signature (stripped id suffix, client-generated ids), it cannot be
    // recovered and must never be fabricated (invalid signatures are also
    // rejected), so the unsigned call and its matching functionResponse are
    // dropped instead of failing the whole request.
    let mut dropped_unsigned_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for msg in messages {
        if msg.get("role").and_then(Value::as_str) == Some("assistant") {
            for call in msg
                .get("tool_calls")
                .and_then(Value::as_array)
                .unwrap_or(&empty)
            {
                if let (Some(id), Some(name)) = (
                    call.get("id").and_then(Value::as_str),
                    call.pointer("/function/name").and_then(Value::as_str),
                ) {
                    call_names.insert(id, name);
                }
            }
        }
    }

    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
        match role {
            "system" | "developer" => {
                if let Some(text) = content_to_text(msg.get("content")) {
                    if !text.is_empty() {
                        system_parts.push(json!({ "text": text }));
                    }
                }
            }
            "assistant" => {
                let mut parts: Vec<Value> = Vec::new();
                if let Some(text) = content_to_text(msg.get("content")) {
                    if !text.is_empty() {
                        parts.push(json!({ "text": text }));
                    }
                }
                for call in msg
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .unwrap_or(&empty)
                {
                    let func = call.get("function").unwrap_or(&Value::Null);
                    let name = func.get("name").and_then(Value::as_str).unwrap_or("");
                    let args = func
                        .get("arguments")
                        .and_then(Value::as_str)
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .unwrap_or_else(|| json!({}));
                    let raw_id = call.get("id").and_then(Value::as_str).unwrap_or("");
                    let (_, signature) = split_signature_from_call_id(raw_id);
                    let signature = signature.or_else(|| {
                        call.get("thought_signature")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    });
                    let model_name = body.get("model").and_then(Value::as_str).unwrap_or("");
                    let drop_unsigned = model_name.contains("3.8");
                    if let Some(signature) = signature {
                        parts.push(json!({
                            "functionCall": { "name": name, "args": args },
                            "thoughtSignature": signature,
                        }));
                    } else if drop_unsigned {
                        if !raw_id.is_empty() {
                            dropped_unsigned_ids.insert(raw_id.to_string());
                        }
                        continue;
                    } else {
                        parts.push(json!({
                            "functionCall": { "name": name, "args": args }
                        }));
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({ "role": "model", "parts": parts }));
                }
            }
            "tool" => {
                let call_id = msg
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if dropped_unsigned_ids.contains(call_id) {
                    continue;
                }
                let explicit_name = msg
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty());
                let name = match explicit_name {
                    // Legacy OpenAI clients carry the function name on the
                    // tool message itself; it outranks the id lookup.
                    Some(name) => name.to_string(),
                    None => {
                        let (plain_id, _) = split_signature_from_call_id(call_id);
                        call_names
                            .get(call_id)
                            .or_else(|| call_names.get(plain_id.as_str()))
                            .copied()
                            .ok_or_else(|| {
                                format!(
                                    "tool response references unknown tool_call_id {call_id:?}; Gemini functionResponse needs the originating function name"
                                )
                            })?
                            .to_string()
                    }
                };
                let raw = content_to_text(msg.get("content")).unwrap_or_default();
                let response = struct_response(&raw);
                contents.push(json!({
                    "role": "user",
                    "parts": [{ "functionResponse": { "name": name, "response": response } }]
                }));
            }
            "function" => {
                let name = msg
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("function");
                let raw = content_to_text(msg.get("content")).unwrap_or_default();
                let response = struct_response(&raw);
                contents.push(json!({
                    "role": "user",
                    "parts": [{ "functionResponse": { "name": name, "response": response } }]
                }));
            }
            _ => {
                let parts = openai_content_to_gemini_parts(msg.get("content"));
                if !parts.is_empty() {
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
        }
    }

    // Gemini rejects any request ending with a model turn (400 INVALID_ARGUMENT:
    // "Requests ending with a model turn are not supported.").
    // If historical unsigned tool calls were dropped, or if the client sent an
    // assistant prefill/retry at the tail, strip trailing model turns so the request
    // always terminates on a user or functionResponse turn.
    while contents
        .last()
        .and_then(|c| c.get("role"))
        .and_then(Value::as_str)
        == Some("model")
    {
        contents.pop();
    }

    let mut request = Map::new();
    request.insert("contents".to_string(), Value::Array(contents));

    if !system_parts.is_empty() {
        request.insert(
            "systemInstruction".to_string(),
            json!({ "role": "user", "parts": system_parts }),
        );
    }

    let mut generation = Map::new();
    if let Some(t) = body.get("temperature").and_then(Value::as_f64) {
        generation.insert("temperature".to_string(), json!(t));
    }
    if let Some(p) = body.get("top_p").and_then(Value::as_f64) {
        generation.insert("topP".to_string(), json!(p));
    }
    if let Some(m) = body
        .get("max_completion_tokens")
        .or_else(|| body.get("max_tokens"))
        .and_then(Value::as_i64)
    {
        generation.insert(
            "maxOutputTokens".to_string(),
            json!(reserve_thinking_budget(model, m)),
        );
    }
    if !generation.is_empty() {
        request.insert("generationConfig".to_string(), Value::Object(generation));
    }

    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let decls: Vec<Value> = tools
            .iter()
            .filter_map(|t| t.get("function"))
            .map(|f| {
                let mut parameters = f.get("parameters").cloned().unwrap_or_else(|| json!({}));
                sanitize_gemini_schema(&mut parameters);
                json!({
                    "name": f.get("name").and_then(Value::as_str).unwrap_or(""),
                    "description": f.get("description").and_then(Value::as_str).unwrap_or(""),
                    "parameters": parameters,
                })
            })
            .collect();
        if !decls.is_empty() {
            request.insert(
                "tools".to_string(),
                json!([{ "functionDeclarations": decls }]),
            );
        }
    }

    Ok(Value::Object(request))
}

/// JSON-Schema keywords the Gemini function-declaration subset rejects with a
/// request-wide 400 ("Unknown name"). `oneOf` folds into `anyOf`, the union
/// form Gemini actually supports; the rest carry no translatable content.
const GEMINI_UNSUPPORTED_SCHEMA_KEYS: &[&str] = &[
    "const",
    "additionalProperties",
    "default",
    "examples",
    "$schema",
    "$id",
    "$defs",
    "definitions",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "patternProperties",
    "propertyNames",
];

fn sanitize_gemini_schema(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for key in GEMINI_UNSUPPORTED_SCHEMA_KEYS {
                map.remove(*key);
            }
            if let Some(one) = map.remove("oneOf") {
                map.entry("anyOf").or_insert(one);
            }
            for child in map.values_mut() {
                sanitize_gemini_schema(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                sanitize_gemini_schema(item);
            }
        }
        _ => {}
    }
}

pub fn openai_to_antigravity(body: &Value, project_id: &str) -> Result<Value, String> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing model".to_string())?;

    let request = openai_to_gemini(body)?;

    Ok(json!({
        "model": model,
        "project": project_id,
        "request": request,
    }))
}

pub fn gemini_json_to_openai(body: &Value, model: &str, created: i64) -> Value {
    let response = body.get("response").unwrap_or(body);
    let candidate = response
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|c| c.first());

    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut pending_signature: Option<String> = None;

    if let Some(parts) = candidate
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
    {
        for (idx, part) in parts.iter().enumerate() {
            if let Some(call) = part.get("functionCall") {
                let name = call.get("name").and_then(Value::as_str).unwrap_or("");
                let args = call
                    .get("args")
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "{}".to_string());
                let mut id = format!("call_{name}_{idx}");
                let signature = part
                    .get("thoughtSignature")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| pending_signature.clone());
                if let Some(sig) = signature {
                    id = embed_signature_in_call_id(&id, &sig);
                }
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": args,
                    }
                }));
            } else if let Some(t) = part.get("text").and_then(Value::as_str) {
                text.push_str(t);
                pending_signature = None;
            } else if let Some(sig) = part.get("thoughtSignature").and_then(Value::as_str) {
                pending_signature = Some(sig.to_string());
            }
        }
    }

    let finish_reason = if !tool_calls.is_empty() {
        "tool_calls"
    } else {
        match candidate
            .and_then(|c| c.get("finishReason"))
            .and_then(Value::as_str)
        {
            Some("MAX_TOKENS") => "length",
            _ => "stop",
        }
    };

    let usage = response.get("usageMetadata");
    let prompt_tokens = usage
        .and_then(|u| u.get("promptTokenCount"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion_tokens = usage
        .and_then(|u| u.get("candidatesTokenCount"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total_tokens = usage
        .and_then(|u| u.get("totalTokenCount"))
        .and_then(Value::as_u64)
        .unwrap_or(prompt_tokens + completion_tokens);

    let message = if !tool_calls.is_empty() {
        if text.is_empty() {
            json!({
                "role": "assistant",
                "content": Value::Null,
                "tool_calls": tool_calls
            })
        } else {
            json!({
                "role": "assistant",
                "content": text,
                "tool_calls": tool_calls
            })
        }
    } else {
        json!({
            "role": "assistant",
            "content": text
        })
    };

    let id = response
        .get("responseId")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("chatcmpl-{created}"));

    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
        }],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": total_tokens,
        }
    })
}

/// Antigravity thinking models bill internal reasoning against maxOutputTokens
/// before emitting any text. Measured overhead on gemini-3.7/3.8-flash-high is
/// 29-89 tokens for a two-word answer, so a client's small max_tokens (e.g. 32)
/// returns finishReason=MAX_TOKENS with zero candidate tokens: a silently empty
/// 200. Raising the floor keeps that request answerable; the client-visible
/// output is still bounded because we surface the OpenAI-side limit downstream.
const THINKING_BUDGET_FLOOR: i64 = 512;

fn reserve_thinking_budget(model: &str, requested: i64) -> i64 {
    if is_thinking_model(model) {
        requested.max(THINKING_BUDGET_FLOOR)
    } else {
        requested
    }
}

fn is_thinking_model(model: &str) -> bool {
    model.starts_with("gemini-3") || model.starts_with("gemini-2.5") || model.ends_with("-thinking")
}

fn content_to_text(content: Option<&Value>) -> Option<String> {
    match content {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(""),
        ),
        _ => None,
    }
}

/// OpenAI multimodal content to Gemini parts: text maps to `text`, data-URL
/// `image_url` parts map to `inlineData`. The Gemini path used to collect only
/// text, silently dropping images that the claude adapter handled.
fn openai_content_to_gemini_parts(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(s)) => vec![json!({ "text": s })],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                match item.get("type").and_then(Value::as_str) {
                Some("text") => item
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|t| json!({ "text": t })),
                Some("image_url") => item
                    .pointer("/image_url/url")
                    .and_then(Value::as_str)
                    .and_then(super::split_data_url)
                    .map(|(media_type, data)| {
                        json!({ "inlineData": { "mimeType": media_type, "data": data } })
                    }),
                _ => None,
            }
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[derive(Default)]
pub struct GeminiDecoder {
    tool_index: u64,
    usage: Option<Usage>,
    completed: bool,
    pending_signature: Option<String>,
}

impl GeminiDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn decode(&mut self, payload: &[u8], out: &mut Vec<CodexEvent>) {
        let text = match std::str::from_utf8(payload) {
            Ok(t) => t.trim(),
            Err(_) => return,
        };
        if text.is_empty() || text == "[DONE]" {
            return;
        }
        let value: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => return,
        };

        if let Some(err) = value.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("upstream error");
            out.push(CodexEvent::Failed {
                message: msg.to_string(),
            });
            self.completed = true;
            return;
        }

        let response = value.get("response").unwrap_or(&value);

        if let Some(id) = response.get("responseId").and_then(Value::as_str) {
            out.push(CodexEvent::Created {
                response_id: id.to_string(),
            });
        }

        if let Some(usage) = response.get("usageMetadata") {
            let prompt = usage
                .get("promptTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let completion = usage
                .get("candidatesTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let total = usage
                .get("totalTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(prompt + completion);
            self.usage = Some(Usage {
                prompt_tokens: prompt,
                completion_tokens: completion,
                total_tokens: total,
                cached_tokens: usage
                    .get("cachedContentTokenCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                reasoning_tokens: usage
                    .get("thoughtsTokenCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            });
        }

        let Some(candidate) = response
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return;
        };

        if let Some(parts) = candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array)
        {
            for part in parts {
                if let Some(call) = part.get("functionCall") {
                    let name = call.get("name").and_then(Value::as_str).unwrap_or("");
                    let args = call
                        .get("args")
                        .map(|a| a.to_string())
                        .unwrap_or_else(|| "{}".to_string());
                    let output_index = self.tool_index;
                    self.tool_index += 1;
                    let signature = part
                        .get("thoughtSignature")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        // Clone, not take: upstream emits one standalone
                        // signature part for a whole parallel-call block, so
                        // every functionCall in the block must carry it.
                        .or_else(|| self.pending_signature.clone());
                    let call_id = match signature.as_deref() {
                        Some(sig) => {
                            embed_signature_in_call_id(&format!("call_{name}_{output_index}"), sig)
                        }
                        None => format!("call_{name}_{output_index}"),
                    };
                    out.push(CodexEvent::ToolCallBegin {
                        output_index,
                        call_id,
                        name: name.to_string(),
                    });
                    out.push(CodexEvent::ToolArgsDelta {
                        output_index,
                        delta: args,
                    });
                    continue;
                }
                if let Some(sig) = part.get("thoughtSignature").and_then(Value::as_str) {
                    out.push(CodexEvent::ReasoningSignature(sig.to_string()));
                    self.pending_signature = Some(sig.to_string());
                    if part.get("text").is_none() {
                        continue;
                    }
                }
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        // Text ends the signature-bearing block; a stale
                        // signature must not leak into a later tool turn.
                        self.pending_signature = None;
                        out.push(CodexEvent::TextDelta(text.to_string()));
                    }
                }
            }
        }

        if candidate
            .get("finishReason")
            .and_then(Value::as_str)
            .is_some()
        {
            out.push(CodexEvent::Completed {
                usage: self.usage.take(),
            });
            self.completed = true;
        }
    }

    pub fn finish(&mut self, out: &mut Vec<CodexEvent>) {
        if !self.completed {
            self.completed = true;
            out.push(CodexEvent::Completed {
                usage: self.usage.take(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_parameters_strip_gemini_rejected_keywords_recursively() {
        let body = json!({
            "model": "gemini-3.7-flash-high",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "lookup",
                    "description": "lookup a value",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "mode": { "type": "string", "const": "fast", "default": "slow" },
                            "nested": {
                                "type": "object",
                                "additionalProperties": false,
                                "$schema": "https://json-schema.org/draft/2020-12/schema",
                                "properties": {
                                    "deep": { "type": "string", "const": "x" }
                                }
                            }
                        }
                    }
                }
            }]
        });

        let request = openai_to_gemini(&body).expect("translate");
        let decl = &request["tools"][0]["functionDeclarations"][0];
        let params = &decl["parameters"];
        assert!(params["properties"]["mode"].get("const").is_none());
        assert!(params["properties"]["mode"].get("default").is_none());
        assert!(params["properties"]["nested"]
            .get("additionalProperties")
            .is_none());
        assert!(params["properties"]["nested"].get("$schema").is_none());
        assert!(params["properties"]["nested"]["properties"]["deep"]
            .get("const")
            .is_none());
        assert_eq!(decl["name"], "lookup");
        assert_eq!(params["properties"]["mode"]["type"], "string");
    }

    #[test]
    fn tool_parameters_fold_oneof_into_anyof() {
        let body = json!({
            "model": "gemini-3.7-flash-high",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "pick",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "value": {
                                "oneOf": [
                                    { "type": "string" },
                                    { "type": "number" }
                                ]
                            }
                        }
                    }
                }
            }]
        });

        let request = openai_to_gemini(&body).expect("translate");
        let value =
            &request["tools"][0]["functionDeclarations"][0]["parameters"]["properties"]["value"];
        assert!(value.get("oneOf").is_none());
        assert_eq!(value["anyOf"].as_array().expect("anyOf").len(), 2);
    }

    #[test]
    fn non_tool_fields_are_untouched_by_the_sanitizer() {
        let body = json!({
            "model": "gemini-3.7-flash-high",
            "messages": [{ "role": "user", "content": "{\"const\": \"payload text stays\"}" }]
        });

        let request = openai_to_gemini(&body).expect("translate");
        let text = request["contents"][0]["parts"][0]["text"].as_str().unwrap();
        assert!(text.contains("payload text stays"));
    }
}

#[cfg(test)]
mod signature_tests {
    use super::*;

    #[test]
    fn image_data_urls_become_inline_data_parts() {
        let body = json!({
            "model":"gemini-3-flash",
            "messages":[{"role":"user","content":[
                {"type":"text","text":"what is this?"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,aGVsbG8="}}
            ]}]
        });
        let request = openai_to_gemini(&body).expect("translate");
        let parts = &request["contents"][0]["parts"];
        assert_eq!(parts[0]["text"], "what is this?");
        assert_eq!(parts[1]["inlineData"]["mimeType"], "image/png");
        assert_eq!(parts[1]["inlineData"]["data"], "aGVsbG8=");
    }

    #[test]
    fn tool_responses_resolve_the_function_name_from_tool_call_id() {
        let signed_id = embed_signature_in_call_id("call_9", "SIG");
        let body = json!({
            "model":"gemini-3-flash",
            "messages":[
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":signed_id,"type":"function","function":{"name":"lookup","arguments":"{}"}}
                ]},
                {"role":"tool","tool_call_id":signed_id,"content":"{\"temp\":21}"}
            ]
        });
        let request = openai_to_gemini(&body).expect("translate");
        let parts = &request["contents"][1]["parts"];
        assert_eq!(parts[0]["functionResponse"]["name"], "lookup");
        assert_eq!(parts[0]["functionResponse"]["response"]["temp"], 21);
    }

    #[test]
    fn scalar_tool_responses_wrap_into_a_struct() {
        // Gemini functionResponse.response is a protobuf Struct: a bare
        // scalar (e.g. a tool that answers just `0`) is INVALID_ARGUMENT
        // upstream, so scalars must be wrapped into an object.
        for content in [
            "0", "42", "-7.5", "true", "false", "null", "", "\"done\"", "[1,2]", "1e10",
        ] {
            let signed_id = embed_signature_in_call_id("call_s", "SIG");
            let body = json!({
                "model":"gemini-3-flash",
                "messages":[
                    {"role":"assistant","content":null,"tool_calls":[
                        {"id":signed_id,"type":"function","function":{"name":"count","arguments":"{}"}}
                    ]},
                    {"role":"tool","tool_call_id":signed_id,"content":content}
                ]
            });
            let request = openai_to_gemini(&body).expect("translate");
            let response = &request["contents"][1]["parts"][0]["functionResponse"]["response"];
            assert!(
                response.is_object(),
                "content {content} must produce a Struct response, got {response}"
            );
            let expected: serde_json::Value =
                serde_json::from_str(content).unwrap_or_else(|_| serde_json::json!(""));
            assert_eq!(response["result"], expected);
        }
    }

    #[test]
    fn function_role_scalar_responses_wrap_into_a_struct() {
        let body = json!({
            "model":"gemini-3-flash",
            "messages":[
                {"role":"function","name":"count","content":"0"}
            ]
        });
        let request = openai_to_gemini(&body).expect("translate");
        let response = &request["contents"][0]["parts"][0]["functionResponse"]["response"];
        assert!(response.is_object());
        assert_eq!(response["result"], 0);
    }

    #[test]
    fn tool_responses_without_a_known_call_id_fail_explicitly() {
        let body = json!({
            "model":"gemini-3-flash",
            "messages":[{"role":"tool","tool_call_id":"ghost","content":"x"}]
        });
        let error = openai_to_gemini(&body).expect_err("unknown call id must fail");
        assert!(error.contains("ghost"), "{error}");
    }

    #[test]
    fn thought_signature_round_trips_through_call_ids() {
        let sig = "GgVSIGWq3bct77QK0b5EwQ==";
        let id = embed_signature_in_call_id("call_todo_4", sig);
        assert!(id.starts_with("call_todo_4#"));
        let (plain, decoded) = split_signature_from_call_id(&id);
        assert_eq!(plain, "call_todo_4");
        assert_eq!(decoded.as_deref(), Some(sig));
    }

    #[test]
    fn plain_call_ids_decode_without_signature() {
        let (plain, decoded) = split_signature_from_call_id("call_todo_4");
        assert_eq!(plain, "call_todo_4");
        assert_eq!(decoded, None);
    }

    #[test]
    fn request_history_attaches_signature_from_tool_call_id() {
        let id = embed_signature_in_call_id("call_todo_4", "SIG");
        let body = json!({
            "model": "gemini-3.7-flash-high",
            "messages": [
                { "role": "user", "content": "plan" },
                { "role": "assistant", "tool_calls": [
                    { "id": id, "type": "function",
                      "function": { "name": "todo", "arguments": "{}" } }
                ]},
                { "role": "tool", "tool_call_id": id, "name": "todo", "content": "{}" }
            ]
        });

        let request = openai_to_gemini(&body).expect("translate");
        let parts = &request["contents"][1]["parts"];
        assert_eq!(parts[0]["functionCall"]["name"], "todo");
        assert_eq!(parts[0]["thoughtSignature"], "SIG");
    }

    #[test]
    fn request_history_stays_signature_free_for_plain_ids() {
        let body = json!({
            "model": "gemini-3.7-flash-high",
            "messages": [
                { "role": "user", "content": "plan" },
                { "role": "assistant", "tool_calls": [
                    { "id": "call_todo_4", "type": "function",
                      "function": { "name": "todo", "arguments": "{}" } }
                ]},
                { "role": "tool", "tool_call_id": "call_todo_4", "name": "todo", "content": "{}" }
            ]
        });

        let request = openai_to_gemini(&body).expect("translate");
        let parts = &request["contents"][1]["parts"];
        assert!(parts[0]["functionCall"].get("thoughtSignature").is_none());
    }

    #[test]
    fn gemini_json_to_openai_embeds_signature_from_function_call() {
        let response = json!({
            "candidates": [{ "content": { "parts": [
                { "functionCall": { "name": "todo", "args": {} },
                  "thoughtSignature": "SIGJSON" }
            ]}}]
        });

        let out = gemini_json_to_openai(&response, "gemini-3.7-flash-high", 0);
        let id = out["choices"][0]["message"]["tool_calls"][0]["id"]
            .as_str()
            .expect("tool call id");
        let (_, decoded) = split_signature_from_call_id(id);
        assert_eq!(decoded.as_deref(), Some("SIGJSON"));
    }

    #[test]
    fn decoder_embeds_signature_from_the_function_call_part() {
        let mut decoder = GeminiDecoder::new();
        let mut out = Vec::new();
        decoder.decode(
            br#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"todo","args":{}},"thoughtSignature":"SIGSTREAM"}]}}]}"#,
            &mut out,
        );
        let begin = out.iter().find_map(|event| match event {
            CodexEvent::ToolCallBegin { call_id, .. } => Some(call_id.clone()),
            _ => None,
        });
        let (_, decoded) = split_signature_from_call_id(begin.expect("tool call begin").as_str());
        assert_eq!(decoded.as_deref(), Some("SIGSTREAM"));
    }

    #[test]
    fn decoder_carries_a_standalone_signature_part_into_the_next_call_id() {
        let mut decoder = GeminiDecoder::new();
        let mut out = Vec::new();
        decoder.decode(
            br#"{"candidates":[{"content":{"parts":[
                {"thoughtSignature":"SIGPENDING"},
                {"functionCall":{"name":"todo","args":{}}}
            ]}}]}"#,
            &mut out,
        );
        let begin = out.iter().find_map(|event| match event {
            CodexEvent::ToolCallBegin { call_id, .. } => Some(call_id.clone()),
            _ => None,
        });
        let (_, decoded) = split_signature_from_call_id(begin.expect("tool call begin").as_str());
        assert_eq!(decoded.as_deref(), Some("SIGPENDING"));
    }

    #[test]
    fn decoder_shares_a_pending_signature_across_parallel_calls() {
        let mut decoder = GeminiDecoder::new();
        let mut out = Vec::new();
        decoder.decode(
            br#"{"candidates":[{"content":{"parts":[
                {"thoughtSignature":"SIGBLOCK"},
                {"functionCall":{"name":"bash","args":{}}}
            ]}}]}"#,
            &mut out,
        );
        decoder.decode(
            br#"{"candidates":[{"content":{"parts":[
                {"functionCall":{"name":"edit","args":{}}}
            ]}}]}"#,
            &mut out,
        );
        let ids: Vec<String> = out
            .iter()
            .filter_map(|event| match event {
                CodexEvent::ToolCallBegin { call_id, .. } => Some(call_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 2);
        for id in &ids {
            let (_, decoded) = split_signature_from_call_id(id);
            assert_eq!(decoded.as_deref(), Some("SIGBLOCK"));
        }
    }

    #[test]
    fn unsigned_history_pairs_are_dropped_before_send() {
        let body = json!({
            "model": "gemini-3.8-flash-high",
            "messages": [
                { "role": "user", "content": "step 1" },
                { "role": "assistant", "content": Value::Null, "tool_calls": [
                    { "id": "call_bash_0", "type": "function",
                      "function": { "name": "bash", "arguments": "{\"command\":\"ls\"}" } }
                ]},
                { "role": "tool", "tool_call_id": "call_bash_0", "content": "out" }
            ],
            "tools": [{ "type": "function", "function": {
                "name": "bash", "parameters": { "type": "object", "properties": {} }
            }}]
        });
        let request = openai_to_gemini(&body).expect("translate");
        let contents = request["contents"].as_array().expect("contents");
        assert_eq!(
            contents.len(),
            1,
            "unsigned call+response pair must be dropped"
        );
        assert_eq!(contents[0]["role"], "user");
    }

    #[test]
    fn signed_history_calls_keep_their_thought_signature() {
        let signed_id = embed_signature_in_call_id("call_bash_0", "SIGREAL");
        let body = json!({
            "model": "gemini-3.8-flash-high",
            "messages": [
                { "role": "user", "content": "step 1" },
                { "role": "assistant", "content": Value::Null, "tool_calls": [
                    { "id": signed_id, "type": "function",
                      "function": { "name": "bash", "arguments": "{\"command\":\"ls\"}" } },
                    { "id": "call_edit_1", "type": "function",
                      "function": { "name": "edit", "arguments": "{}" } }
                ]},
                { "role": "tool", "tool_call_id": signed_id, "content": "out" },
                { "role": "tool", "tool_call_id": "call_edit_1", "content": "ok" }
            ],
            "tools": [{ "type": "function", "function": {
                "name": "bash", "parameters": { "type": "object", "properties": {} }
            }}]
        });
        let request = openai_to_gemini(&body).expect("translate");
        let contents = request["contents"].as_array().expect("contents");
        // user, model(signed call), user(functionResponse) — unsigned pair gone.
        assert_eq!(contents.len(), 3);
        let call_part = &contents[1]["parts"][0];
        assert_eq!(call_part["functionCall"]["name"], "bash");
        assert_eq!(call_part["thoughtSignature"], "SIGREAL");
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["name"], "bash");
    }

    #[test]
    fn trailing_model_turn_is_stripped_to_avoid_upstream_rejection() {
        let body = json!({
            "model": "gemini-3.8-flash-high",
            "messages": [
                { "role": "user", "content": "hello" },
                { "role": "assistant", "content": "prefilled text or unfinished response" }
            ]
        });
        let request = openai_to_gemini(&body).expect("translate");
        let contents = request["contents"].as_array().expect("contents");
        assert_eq!(contents.len(), 1, "trailing model turn must be stripped");
        assert_eq!(contents[0]["role"], "user");
    }

    #[test]
    fn unsigned_tool_call_with_text_leaves_no_trailing_model_turn() {
        let body = json!({
            "model": "gemini-3.8-flash-high",
            "messages": [
                { "role": "user", "content": "run command" },
                {
                    "role": "assistant",
                    "content": "thinking aloud before tool call",
                    "tool_calls": [
                        { "id": "call_unsigned_1", "type": "function",
                          "function": { "name": "bash", "arguments": "{\"command\":\"ls\"}" } }
                    ]
                },
                { "role": "tool", "tool_call_id": "call_unsigned_1", "content": "output" }
            ],
            "tools": [{ "type": "function", "function": {
                "name": "bash", "parameters": { "type": "object", "properties": {} }
            }}]
        });
        let request = openai_to_gemini(&body).expect("translate");
        let contents = request["contents"].as_array().expect("contents");
        assert_eq!(
            contents.len(),
            1,
            "both the dropped tool response and the resulting stranded model turn must not end the request"
        );
        assert_eq!(contents[0]["role"], "user");
    }
}
