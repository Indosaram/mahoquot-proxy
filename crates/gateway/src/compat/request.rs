use serde_json::{json, Map, Value};

#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    #[error("invalid json body: {0}")]
    Json(String),
    #[error("missing required field: {0}")]
    Missing(&'static str),
    #[error("unsupported option: {0}")]
    UnsupportedOption(String),
    #[error("unsupported thinking configuration: {0}")]
    UnsupportedThinking(String),
}

#[derive(Debug)]
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

    let raw_model = obj
        .get("model")
        .and_then(Value::as_str)
        .ok_or(TranslateError::Missing("model"))?;
    let (model, suffix_opt) = parse_model_suffix(raw_model)?;
    let model = model.to_string();
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
    if let Some(key) = prompt_cache_key
        .map(str::trim)
        .filter(|key| !key.is_empty())
    {
        out.insert("prompt_cache_key".into(), Value::String(key.to_string()));
    }

    // Thinking / reasoning effort resolution per canonical thinking pipeline:
    // Suffix takes precedence over body parameters.
    // Canonical Codex chat translation defaults omitted effort to medium.
    let resolved_effort = if let Some(suffix) = suffix_opt {
        Some(normalize_thinking_level(suffix)?)
    } else if let Some(effort) = obj.get("reasoning_effort") {
        let effort_str = effort.as_str().ok_or_else(|| {
            TranslateError::UnsupportedThinking("reasoning_effort must be a string".to_string())
        })?;
        Some(normalize_thinking_level(effort_str)?)
    } else if let Some(reasoning_val) = obj.get("reasoning") {
        let reasoning_obj = reasoning_val.as_object().ok_or_else(|| {
            TranslateError::UnsupportedThinking("reasoning must be an object".to_string())
        })?;
        if let Some(effort) = reasoning_obj.get("effort") {
            let effort_str = effort.as_str().ok_or_else(|| {
                TranslateError::UnsupportedThinking("reasoning.effort must be a string".to_string())
            })?;
            Some(normalize_thinking_level(effort_str)?)
        } else {
            None
        }
    } else {
        None
    };
    // An explicit thinking budget must be -1 (auto), 0 (off) or positive; any
    // other value is invalid and is rejected before dispatch (Task 5).
    if let Some(budget) = obj
        .get("thinking_budget")
        .or_else(|| obj.get("thinking").and_then(|t| t.get("budget_tokens")))
    {
        match budget.as_i64() {
            Some(-1) | Some(0) => {}
            Some(value) if value > 0 => {}
            _ => {
                return Err(TranslateError::UnsupportedThinking(
                    "invalid thinking budget".to_string(),
                ))
            }
        }
    }

    out.insert(
        "reasoning".into(),
        json!({ "effort": resolved_effort.as_deref().unwrap_or("medium") }),
    );

    if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
        let mapped = map_tools(tools);
        if !mapped.is_empty() {
            out.insert("tools".into(), Value::Array(mapped));
        }
    }
    if let Some(choice) = map_tool_choice(obj.get("tool_choice")) {
        out.insert("tool_choice".into(), choice);
    }
    // Codex does NOT support temperature, top_p, top_k, max_tokens, or max_output_tokens.
    // Copy only supported knobs (parallel_tool_calls).
    if let Some(v) = obj.get("parallel_tool_calls") {
        out.insert("parallel_tool_calls".into(), v.clone());
    } else {
        out.insert("parallel_tool_calls".into(), Value::Bool(true));
    }
    // The Codex backend rejects max_output_tokens with
    // `400 Unsupported parameter: max_output_tokens`, verified against 4 live
    // accounts. OpenAI clients routinely send max_tokens, so forwarding it would
    // fail those requests outright; CLIProxyAPI drops it for the same reason.
    // Deliberately not translated.

    // Map response_format and text settings to Responses API text.format
    if let Some(rf) = obj.get("response_format") {
        let rf_obj = rf.as_object().ok_or_else(|| {
            TranslateError::UnsupportedOption("response_format must be an object".into())
        })?;
        let rft = rf_obj.get("type").and_then(Value::as_str).ok_or_else(|| {
            TranslateError::UnsupportedOption("response_format must have a string 'type'".into())
        })?;

        let mut format = Map::new();
        match rft {
            "text" => {
                format.insert("type".into(), Value::String("text".into()));
            }
            "json_schema" => {
                let js = rf_obj
                    .get("json_schema")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        TranslateError::UnsupportedOption(
                            "json_schema object is required for response_format json_schema".into(),
                        )
                    })?;
                format.insert("type".into(), Value::String("json_schema".into()));
                if let Some(name) = js.get("name") {
                    format.insert("name".into(), name.clone());
                }
                if let Some(strict) = js.get("strict") {
                    format.insert("strict".into(), strict.clone());
                }
                if let Some(schema) = js.get("schema") {
                    format.insert("schema".into(), schema.clone());
                }
            }
            "json_object" => {
                format.insert("type".into(), Value::String("json_object".into()));
            }
            other => {
                return Err(TranslateError::UnsupportedOption(format!(
                    "unsupported response_format type: '{other}'"
                )));
            }
        }

        let mut text_obj = Map::new();
        text_obj.insert("format".into(), Value::Object(format));
        if let Some(verbosity) = obj.get("text").and_then(|t| t.get("verbosity")) {
            text_obj.insert("verbosity".into(), verbosity.clone());
        }
        out.insert("text".into(), Value::Object(text_obj));
    } else if let Some(text_in) = obj.get("text").and_then(Value::as_object) {
        let mut text_obj = Map::new();
        if let Some(format) = text_in.get("format") {
            text_obj.insert("format".into(), format.clone());
        }
        if let Some(verbosity) = text_in.get("verbosity") {
            text_obj.insert("verbosity".into(), verbosity.clone());
        }
        if !text_obj.is_empty() {
            out.insert("text".into(), Value::Object(text_obj));
        }
    }

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
            "tool" | "function" => {
                // OpenAI-chat tool results keep Responses' multimodal output
                // array (images in call order). A tool result translated from
                // an Anthropic `tool_result` instead carries its text as the
                // output and relays its images as the following user turn,
                // the Anthropic->Codex wire contract (Task 4).
                let from_anthropic = msg
                    .get("x_anthropic_tool_result")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": msg.get("tool_call_id").and_then(Value::as_str).unwrap_or_default(),
                    "output": if from_anthropic {
                        Value::String(flatten_text(msg.get("content")))
                    } else {
                        tool_output(msg.get("content"))
                    },
                }));
                let images: Vec<Value> = if from_anthropic {
                    user_parts(msg.get("content"))
                        .into_iter()
                        .filter(|part| part["type"] == "input_image")
                        .collect()
                } else {
                    Vec::new()
                };
                if !images.is_empty() {
                    let mut content = vec![json!({"type": "input_text", "text": ""})];
                    content.extend(images);
                    input.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": content,
                    }));
                }
            }
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
            _ => {
                let content = user_parts(msg.get("content"));
                if !content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": content,
                    }));
                }
            }
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

pub fn parse_model_suffix(model: &str) -> Result<(&str, Option<&str>), TranslateError> {
    if let Some(last_open) = model.rfind('(') {
        if model.ends_with(')') && last_open > 0 && !model[..last_open].contains(['(', ')']) {
            let base_model = &model[..last_open];
            let raw_suffix = &model[last_open + 1..model.len() - 1];
            return Ok((base_model, Some(raw_suffix)));
        } else {
            return Err(TranslateError::UnsupportedThinking(format!(
                "invalid model suffix syntax in '{model}'"
            )));
        }
    }
    if model.contains(')') {
        return Err(TranslateError::UnsupportedThinking(format!(
            "invalid model suffix syntax in '{model}'"
        )));
    }
    Ok((model, None))
}

pub fn normalize_thinking_level(raw: &str) -> Result<String, TranslateError> {
    let lower = raw.trim().to_lowercase();
    match lower.as_str() {
        "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" => Ok(lower),
        "auto" | "-1" => Ok("medium".to_string()),
        other => {
            if let Ok(budget) = other.parse::<i64>() {
                if budget < -1 {
                    return Err(TranslateError::UnsupportedThinking(format!(
                        "invalid negative budget: {budget}"
                    )));
                } else if budget == -1 {
                    return Ok("medium".to_string());
                } else if budget == 0 {
                    return Ok("none".to_string());
                } else if budget <= 512 {
                    return Ok("minimal".to_string());
                } else if budget <= 1024 {
                    return Ok("low".to_string());
                } else if budget <= 8192 {
                    return Ok("medium".to_string());
                } else if budget <= 24576 {
                    return Ok("high".to_string());
                } else {
                    return Ok("xhigh".to_string());
                }
            }
            Err(TranslateError::UnsupportedThinking(format!(
                "unsupported thinking level: '{raw}'"
            )))
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ThinkingMode {
    None,
    Auto,
    Budget(i64),
    Level(String),
}

pub fn thinking_mode(model: &str, body: &Value) -> Result<Option<ThinkingMode>, TranslateError> {
    let (_, suffix) = parse_model_suffix(model)?;
    let raw = if let Some(suffix) = suffix {
        Some(Value::String(suffix.to_string()))
    } else if let Some(effort) = body.get("reasoning_effort") {
        Some(effort.clone())
    } else if let Some(reasoning) = body.get("reasoning") {
        let reasoning = reasoning.as_object().ok_or_else(|| {
            TranslateError::UnsupportedThinking("reasoning must be an object".into())
        })?;
        reasoning.get("effort").cloned()
    } else {
        None
    };
    if let Some(raw) = raw {
        let raw = raw.as_str().ok_or_else(|| {
            TranslateError::UnsupportedThinking("reasoning effort must be a string".into())
        })?;
        let raw = raw.trim().to_ascii_lowercase();
        return Ok(Some(match raw.as_str() {
            "none" | "0" => ThinkingMode::None,
            "auto" | "-1" => ThinkingMode::Auto,
            _ => match raw.parse::<i64>() {
                Ok(budget) if budget > 0 => ThinkingMode::Budget(budget),
                Ok(0) => ThinkingMode::None,
                Ok(-1) => ThinkingMode::Auto,
                _ => ThinkingMode::Level(normalize_thinking_level(&raw)?),
            },
        }));
    }
    let budget = body
        .pointer("/thinking/budget_tokens")
        .or_else(|| body.get("thinking_budget"));
    budget
        .map(|budget| {
            let budget = budget.as_i64().ok_or_else(|| {
                TranslateError::UnsupportedThinking("thinking budget must be an integer".into())
            })?;
            match budget {
                -1 => Ok(ThinkingMode::Auto),
                0 => Ok(ThinkingMode::None),
                1.. => Ok(ThinkingMode::Budget(budget)),
                _ => Err(TranslateError::UnsupportedThinking(
                    "invalid negative thinking budget".into(),
                )),
            }
        })
        .transpose()
}

pub fn level_budget(level: &str) -> i64 {
    match level {
        "minimal" => 512,
        "low" => 1024,
        "medium" => 8192,
        "high" => 24576,
        "xhigh" => 32768,
        "max" => 128000,
        _ => 0,
    }
}

pub fn clamp_thinking_level(level: &str, support: &mahoquot_registry::ThinkingSupport) -> String {
    if support.levels.iter().any(|supported| supported == level) {
        return level.to_string();
    }
    let order = ["minimal", "low", "medium", "high", "xhigh", "max"];
    let Some(index) = order.iter().position(|candidate| *candidate == level) else {
        return level.to_string();
    };
    support
        .levels
        .iter()
        .filter_map(|level| {
            order
                .iter()
                .position(|candidate| *candidate == level.as_str())
                .map(|position| (position, level))
        })
        .min_by_key(|(position, _)| (position.abs_diff(index), *position))
        .map(|(_, level)| level.clone())
        .unwrap_or_else(|| level.to_string())
}

pub fn target_thinking_mode(
    mode: ThinkingMode,
    capability: &mahoquot_registry::ThinkingCapability,
    claude: bool,
) -> Result<ThinkingMode, String> {
    use mahoquot_registry::ThinkingCapability;
    let support = match capability {
        ThinkingCapability::Unknown => return Ok(mode),
        ThinkingCapability::Unsupported => {
            return match mode {
                ThinkingMode::None => Ok(ThinkingMode::None),
                _ => Err(
                    "thinking not supported for this target: unsupported option conflict".into(),
                ),
            }
        }
        ThinkingCapability::Supported(support) => support,
    };
    let budget_capable = support.min > 0 || support.max > 0;
    let level_capable = !support.levels.is_empty();
    let mode = match mode {
        ThinkingMode::Level(level) if !level_capable && budget_capable => {
            ThinkingMode::Budget(level_budget(&level))
        }
        ThinkingMode::Level(level) => ThinkingMode::Level(clamp_thinking_level(&level, support)),
        ThinkingMode::Budget(budget) if !budget_capable && level_capable => {
            let level =
                normalize_thinking_level(&budget.to_string()).map_err(|error| error.to_string())?;
            ThinkingMode::Level(clamp_thinking_level(&level, support))
        }
        ThinkingMode::Auto if !claude && !support.dynamic_allowed => {
            if !budget_capable && level_capable {
                ThinkingMode::Level(clamp_thinking_level("medium", support))
            } else {
                ThinkingMode::Budget(support.min + (support.max - support.min) / 2)
            }
        }
        other => other,
    };
    Ok(match mode {
        ThinkingMode::Budget(budget) if budget_capable => {
            ThinkingMode::Budget(budget.clamp(support.min, support.max))
        }
        ThinkingMode::None if !claude && !support.zero_allowed => {
            if let Some(level) = support.levels.first() {
                ThinkingMode::Level(level.clone())
            } else {
                ThinkingMode::Budget(support.min)
            }
        }
        other => other,
    })
}

pub fn embedded_thinking_capability(
    model: &str,
    provider: &str,
) -> Result<mahoquot_registry::ThinkingCapability, String> {
    let (model, _) = parse_model_suffix(model).map_err(|error| error.to_string())?;
    let id = mahoquot_registry::ModelId::new(model).map_err(|error| error.to_string())?;
    match mahoquot_registry::embedded_snapshot().models().get(&id) {
        Some(descriptor) => descriptor
            .thinking_capability(provider)
            .map_err(|error| error.to_string()),
        None => Ok(mahoquot_registry::ThinkingCapability::Unknown),
    }
}

pub fn extract_model(raw: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(raw)
        .ok()?
        .get("model")
        .and_then(Value::as_str)
        .map(|m| {
            if let Ok((base, _)) = parse_model_suffix(m) {
                base.to_string()
            } else {
                m.to_string()
            }
        })
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

    #[test]
    fn maps_reasoning_effort_to_codex_reasoning_effort() {
        let body = serde_json::json!({
            "model": "gpt-6-astra",
            "reasoning_effort": "high",
            "messages": [{"role": "user", "content": "solve this"}]
        });
        let translated = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap();
        let val: Value = serde_json::from_slice(&translated.body).unwrap();
        assert_eq!(val["reasoning"]["effort"], "high");
    }

    #[test]
    fn maps_inbound_responses_reasoning_effort() {
        let body = serde_json::json!({
            "model": "gpt-6-astra",
            "reasoning": {"effort": "low"},
            "messages": [{"role": "user", "content": "solve this"}]
        });
        let translated = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap();
        let val: Value = serde_json::from_slice(&translated.body).unwrap();
        assert_eq!(val["reasoning"]["effort"], "low");
    }

    #[test]
    fn suffix_overrides_body_reasoning_effort() {
        let body = serde_json::json!({
            "model": "gpt-6-astra(high)",
            "reasoning_effort": "low",
            "messages": [{"role": "user", "content": "solve this"}]
        });
        let translated = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(translated.model, "gpt-6-astra");
        let val: Value = serde_json::from_slice(&translated.body).unwrap();
        assert_eq!(val["model"], "gpt-6-astra");
        assert_eq!(val["reasoning"]["effort"], "high");
    }

    #[test]
    fn numeric_suffix_converts_to_discrete_level() {
        let body = serde_json::json!({
            "model": "gpt-6-astra(16384)",
            "messages": [{"role": "user", "content": "solve this"}]
        });
        let translated = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(translated.model, "gpt-6-astra");
        let val: Value = serde_json::from_slice(&translated.body).unwrap();
        assert_eq!(val["reasoning"]["effort"], "high");
    }

    #[test]
    fn unsupported_thinking_level_rejected() {
        let body = serde_json::json!({
            "model": "gpt-6-astra",
            "reasoning_effort": "ultra",
            "messages": [{"role": "user", "content": "solve this"}]
        });
        let err = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap_err();
        assert!(matches!(err, TranslateError::UnsupportedThinking(_)));
    }

    #[test]
    fn omitted_reasoning_defaults_to_canonical_medium() {
        let body = serde_json::json!({
            "model": "gpt-6-astra",
            "messages": [{"role": "user", "content": "simple question"}]
        });
        let translated = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap();
        let val: Value = serde_json::from_slice(&translated.body).unwrap();
        assert_eq!(val["reasoning"]["effort"], "medium");
    }

    #[test]
    fn reasoning_without_effort_defaults_to_medium() {
        let body = json!({"model":"gpt-6-astra", "reasoning":{}, "messages":[]});
        let translated = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap();
        let out: Value = serde_json::from_slice(&translated.body).unwrap();
        assert_eq!(out["reasoning"]["effort"], "medium");
    }

    #[test]
    fn drops_unsupported_sampling_knobs() {
        let body = serde_json::json!({
            "model": "gpt-6-astra",
            "temperature": 0.7,
            "top_p": 0.95,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let translated = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap();
        let val: Value = serde_json::from_slice(&translated.body).unwrap();
        assert!(val.get("temperature").is_none());
        assert!(val.get("top_p").is_none());
    }

    #[test]
    fn maps_json_schema_response_format() {
        let body = serde_json::json!({
            "model": "gpt-6-astra",
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "result_schema",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "properties": {"ans": {"type": "string"}},
                        "required": ["ans"],
                        "additionalProperties": false
                    }
                }
            },
            "messages": [{"role": "user", "content": "extract"}]
        });
        let translated = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap();
        let val: Value = serde_json::from_slice(&translated.body).unwrap();
        assert_eq!(val["text"]["format"]["type"], "json_schema");
        assert_eq!(val["text"]["format"]["name"], "result_schema");
        assert_eq!(val["text"]["format"]["strict"], true);
        assert_eq!(
            val["text"]["format"]["schema"]["properties"]["ans"]["type"],
            "string"
        );
    }

    #[test]
    fn suffix_none_overrides_high_body_reasoning_effort() {
        let body = serde_json::json!({
            "model": "gpt-6-astra(none)",
            "reasoning_effort": "high",
            "messages": [{"role": "user", "content": "solve this"}]
        });
        let translated = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(translated.model, "gpt-6-astra");
        let val: Value = serde_json::from_slice(&translated.body).unwrap();
        assert_eq!(val["model"], "gpt-6-astra");
        assert_eq!(val["reasoning"]["effort"], "none");
    }

    #[test]
    fn suffix_medium_applies_without_body_param() {
        let body = serde_json::json!({
            "model": "gpt-6-astra(medium)",
            "messages": [{"role": "user", "content": "solve this"}]
        });
        let translated = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(translated.model, "gpt-6-astra");
        let val: Value = serde_json::from_slice(&translated.body).unwrap();
        assert_eq!(val["reasoning"]["effort"], "medium");
    }

    #[test]
    fn extract_model_strips_suffix() {
        let body = serde_json::json!({
            "model": "gpt-6-astra(high)",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let model = extract_model(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(model, "gpt-6-astra");
    }

    #[test]
    fn malformed_suffix_syntax_rejected() {
        let body = serde_json::json!({
            "model": "gpt-6-astra(high",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let err = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap_err();
        assert!(matches!(err, TranslateError::UnsupportedThinking(_)));
    }

    #[test]
    fn invalid_negative_budget_suffix_rejected() {
        let body = serde_json::json!({
            "model": "gpt-6-astra(-5)",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let err = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap_err();
        assert!(matches!(err, TranslateError::UnsupportedThinking(_)));
    }

    #[test]
    fn non_string_reasoning_effort_rejected() {
        let body = serde_json::json!({
            "model": "gpt-6-astra",
            "reasoning_effort": 123,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let err = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap_err();
        assert!(matches!(err, TranslateError::UnsupportedThinking(_)));
    }

    #[test]
    fn non_string_reasoning_object_effort_rejected() {
        let body = serde_json::json!({
            "model": "gpt-6-astra",
            "reasoning": {"effort": true},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let err = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap_err();
        assert!(matches!(err, TranslateError::UnsupportedThinking(_)));
    }

    #[test]
    fn non_object_reasoning_rejected() {
        let body = serde_json::json!({
            "model": "gpt-6-astra",
            "reasoning": "high",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let err = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap_err();
        assert!(matches!(err, TranslateError::UnsupportedThinking(_)));
    }

    #[test]
    fn non_object_response_format_rejected() {
        let body = serde_json::json!({
            "model": "gpt-6-astra",
            "response_format": "json_object",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let err = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap_err();
        assert!(matches!(err, TranslateError::UnsupportedOption(_)));
    }

    #[test]
    fn response_format_missing_type_rejected() {
        let body = serde_json::json!({
            "model": "gpt-6-astra",
            "response_format": {},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let err = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap_err();
        assert!(matches!(err, TranslateError::UnsupportedOption(_)));
    }

    #[test]
    fn response_format_unsupported_type_rejected() {
        let body = serde_json::json!({
            "model": "gpt-6-astra",
            "response_format": {"type": "xml"},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let err = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap_err();
        assert!(matches!(err, TranslateError::UnsupportedOption(_)));
    }

    #[test]
    fn response_format_json_schema_missing_schema_object_rejected() {
        let body = serde_json::json!({
            "model": "gpt-6-astra",
            "response_format": {"type": "json_schema"},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let err = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap_err();
        assert!(matches!(err, TranslateError::UnsupportedOption(_)));
    }

    #[test]
    fn response_format_text_mapped() {
        let body = serde_json::json!({
            "model": "gpt-6-astra",
            "response_format": {"type": "text"},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let translated = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap();
        let val: Value = serde_json::from_slice(&translated.body).unwrap();
        assert_eq!(val["text"]["format"]["type"], "text");
    }

    #[test]
    fn response_format_json_object_mapped() {
        let body = serde_json::json!({
            "model": "gpt-6-astra",
            "response_format": {"type": "json_object"},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let translated = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap();
        let val: Value = serde_json::from_slice(&translated.body).unwrap();
        assert_eq!(val["text"]["format"]["type"], "json_object");
    }

    #[test]
    fn inbound_responses_text_format_preserved() {
        let body = serde_json::json!({
            "model": "gpt-6-astra",
            "text": {
                "format": {"type": "custom_format", "name": "custom"},
                "verbosity": "high"
            },
            "messages": [{"role": "user", "content": "hi"}]
        });
        let translated = openai_to_codex(&serde_json::to_vec(&body).unwrap()).unwrap();
        let val: Value = serde_json::from_slice(&translated.body).unwrap();
        assert_eq!(val["text"]["format"]["type"], "custom_format");
        assert_eq!(val["text"]["format"]["name"], "custom");
        assert_eq!(val["text"]["verbosity"], "high");
    }
}
