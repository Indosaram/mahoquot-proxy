use serde_json::{json, Map, Value};

#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    #[error("invalid json body: {0}")]
    Json(String),
    #[error("missing required field: {0}")]
    Missing(&'static str),
}

pub struct TranslatedRequest {
    pub body: Vec<u8>,
    pub model: String,
    pub stream: bool,
    pub include_usage: bool,
}

pub fn openai_to_codex(raw: &[u8]) -> Result<TranslatedRequest, TranslateError> {
    openai_to_codex_with_cache_key(raw, None)
}

/// Same translation with an optional stable `prompt_cache_key`. OpenAI-side
/// implicit prompt caching is routed by this field: without it consecutive
/// turns of one conversation land on unrelated cache shards and report
/// `cached_tokens: 0` on an identical prefix (opencodex devlog 090 observed
/// the same on the ChatGPT backend). Callers must pass a stable per-session
/// identity; never a per-request or head-hash value.
pub fn openai_to_codex_with_cache_key(
    raw: &[u8],
    prompt_cache_key: Option<&str>,
) -> Result<TranslatedRequest, TranslateError> {
    let root: Value =
        serde_json::from_slice(raw).map_err(|e| TranslateError::Json(e.to_string()))?;
    let obj = root.as_object().ok_or(TranslateError::Missing("body"))?;

    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .ok_or(TranslateError::Missing("model"))?
        .to_string();
    let stream = obj.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let include_usage = obj
        .get("stream_options")
        .and_then(|v| v.get("include_usage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let messages = obj
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(TranslateError::Missing("messages"))?;

    let (instructions, input) = split_messages(messages);

    let mut out = Map::new();
    out.insert("model".into(), Value::String(model.clone()));
    out.insert("instructions".into(), Value::String(instructions));
    out.insert("input".into(), Value::Array(input));
    out.insert("stream".into(), Value::Bool(true));
    out.insert("store".into(), Value::Bool(false));
    if let Some(key) = prompt_cache_key.map(str::trim).filter(|key| !key.is_empty()) {
        out.insert("prompt_cache_key".into(), Value::String(key.to_string()));
    }

    if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
        let mapped = map_tools(tools);
        if !mapped.is_empty() {
            out.insert("tools".into(), Value::Array(mapped));
        }
    }
    if let Some(choice) = map_tool_choice(obj.get("tool_choice")) {
        out.insert("tool_choice".into(), choice);
    }
    for key in ["temperature", "top_p", "parallel_tool_calls"] {
        if let Some(v) = obj.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    // The Codex backend rejects max_output_tokens with
    // `400 Unsupported parameter: max_output_tokens`, verified against 4 live
    // accounts. OpenAI clients routinely send max_tokens, so forwarding it would
    // fail those requests outright; CLIProxyAPI drops it for the same reason.
    // Deliberately not translated.

    let body =
        serde_json::to_vec(&Value::Object(out)).map_err(|e| TranslateError::Json(e.to_string()))?;
    Ok(TranslatedRequest {
        body,
        model,
        stream,
        include_usage,
    })
}

fn split_messages(messages: &[Value]) -> (String, Vec<Value>) {
    let mut instructions = String::new();
    let mut input: Vec<Value> = Vec::with_capacity(messages.len());

    for msg in messages {
        match msg.get("role").and_then(Value::as_str).unwrap_or("user") {
            "system" | "developer" => {
                let text = flatten_text(msg.get("content"));
                if !text.is_empty() {
                    if !instructions.is_empty() {
                        instructions.push_str("\n\n");
                    }
                    instructions.push_str(&text);
                }
            }
            "tool" | "function" => input.push(json!({
                "type": "function_call_output",
                "call_id": msg.get("tool_call_id").and_then(Value::as_str).unwrap_or_default(),
                "output": tool_output(msg.get("content")),
            })),
            "assistant" => {
                let text = flatten_text(msg.get("content"));
                if !text.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}],
                    }));
                }
                for call in msg
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .unwrap_or(&Vec::new())
                {
                    let function = call.get("function");
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.get("id").and_then(Value::as_str).unwrap_or_default(),
                        "name": function.and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or_default(),
                        "arguments": function.and_then(|f| f.get("arguments")).and_then(Value::as_str).unwrap_or("{}"),
                    }));
                }
            }
            _ => input.push(json!({
                "type": "message",
                "role": "user",
                "content": user_parts(msg.get("content")),
            })),
        }
    }
    (instructions, input)
}

fn flatten_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn tool_output(content: Option<&Value>) -> Value {
    // Responses supports multimodal function_call_output arrays. Flattening a
    // screenshot tool's result to text silently removes its images (and turns
    // an image-only result into an empty string). Keep text-only outputs in the
    // existing string format, but preserve image-bearing results in call order.
    if matches!(content, Some(Value::Array(_))) {
        let parts = user_parts(content);
        if parts.iter().any(|part| part["type"] == "input_image") {
            return Value::Array(parts);
        }
    }
    Value::String(flatten_text(content))
}

fn user_parts(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                Some("image_url") => {
                    let image = part.get("image_url")?;
                    let url = image.get("url")?.as_str()?;
                    let mut mapped = json!({"type": "input_image", "image_url": url});
                    // Preserve the caller's preprocessing choice. Dropping high
                    // or low lets the backend choose auto instead, which can
                    // change image sizing, token use, and acceptance limits.
                    if let Some(detail) = image.get("detail") {
                        mapped["detail"] = detail.clone();
                    }
                    Some(mapped)
                }
                _ => part
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|text| json!({"type": "input_text", "text": text})),
            })
            .collect(),
        other => vec![json!({"type": "input_text", "text": flatten_text(other)})],
    }
}

fn map_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            let name = function.get("name").and_then(Value::as_str)?;
            Some(json!({
                "type": "function",
                "name": name,
                "description": function.get("description").and_then(Value::as_str).unwrap_or_default(),
                "parameters": function
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                "strict": function.get("strict").and_then(Value::as_bool).unwrap_or(false),
            }))
        })
        .collect()
}

fn map_tool_choice(choice: Option<&Value>) -> Option<Value> {
    match choice {
        Some(Value::String(s)) => Some(Value::String(s.clone())),
        Some(Value::Object(o)) => {
            let name = o
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)?;
            Some(json!({"type": "function", "name": name}))
        }
        _ => None,
    }
}

pub fn extract_model(raw: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(raw)
        .ok()?
        .get("model")
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

#[cfg(test)]
mod prompt_cache_key_tests {
    use super::*;

    fn sample_body() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "model": "gpt-6-astra",
            "stream": true,
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap()
    }

    #[test]
    fn injects_stable_prompt_cache_key_when_provided() {
        let translated = openai_to_codex_with_cache_key(&sample_body(), Some("abc123")).unwrap();
        let body: Value = serde_json::from_slice(&translated.body).unwrap();
        assert_eq!(
            body.get("prompt_cache_key"),
            Some(&Value::String("abc123".into()))
        );
    }

    #[test]
    fn omits_prompt_cache_key_without_identity() {
        let translated = openai_to_codex(&sample_body()).unwrap();
        let body: Value = serde_json::from_slice(&translated.body).unwrap();
        assert!(body.get("prompt_cache_key").is_none());
    }

    #[test]
    fn blank_prompt_cache_key_is_omitted() {
        let translated = openai_to_codex_with_cache_key(&sample_body(), Some("   ")).unwrap();
        let body: Value = serde_json::from_slice(&translated.body).unwrap();
        assert!(body.get("prompt_cache_key").is_none());
    }

    #[test]
    fn messages_have_explicit_type_message() {
        let body = serde_json::json!({
            "model": "gpt-6-sol",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"}
            ]
        });
        let translated = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap();
        let val: Value = serde_json::from_slice(&translated.body).unwrap();
        let input = val["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "assistant");
    }
}
