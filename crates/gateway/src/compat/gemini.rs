use base64::Engine as _;
use serde_json::{json, Map, Value};

use super::events::{CodexEvent, Usage};
use super::signature_ledger::{self, ReplayScope};

pub fn interactions_to_gemini(body: &Value) -> Result<Value, String> {
    use sha2::{Digest, Sha256};

    fn names(value: &Value, out: &mut std::collections::BTreeSet<String>) {
        match value {
            Value::Array(items) => items.iter().for_each(|item| names(item, out)),
            Value::Object(map) => {
                if let Some(name) = map
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|n| !n.is_empty())
                {
                    out.insert(name.to_string());
                }
                for key in [
                    "tools",
                    "function",
                    "functionDeclarations",
                    "function_declarations",
                ] {
                    if let Some(value) = map.get(key) {
                        names(value, out);
                    }
                }
            }
            _ => {}
        }
    }
    fn sanitize(name: &str) -> String {
        let mut out: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        if !out.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
            out.insert(0, '_');
        }
        out.truncate(64);
        out
    }
    fn part_list(content: &Value) -> Result<Vec<Value>, String> {
        if let Some(text) = content.as_str() {
            return Ok(vec![json!({"text":text})]);
        }
        content.as_array().ok_or("input content must be a string or array")?.iter().map(|part| {
            let kind = part.get("type").and_then(Value::as_str).unwrap_or("");
            let mut out = match kind {
                "text" | "thought" => json!({"text":part.get("text").cloned().unwrap_or(Value::String(String::new()))}),
                "image" => {
                    let data = part.get("data").and_then(Value::as_str).ok_or("image input requires data")?;
                    let mime = part.get("mime_type").and_then(Value::as_str).unwrap_or("image/png");
                    json!({"inlineData":{"mimeType":mime,"data":data}})
                }
                "document" => {
                    let uri = part.get("uri").and_then(Value::as_str).ok_or("document input requires uri")?;
                    json!({"fileData":{"fileUri":uri,"mimeType":part.get("mime_type").and_then(Value::as_str).unwrap_or("application/octet-stream")}})
                }
                _ => return Err(format!("unsupported Interactions content type: {kind}")),
            };
            if kind == "thought" { out["thought"] = json!(true); }
            if let Some(signature) = part.get("thought_signature").or_else(|| part.get("thoughtSignature")) { out["thoughtSignature"] = signature.clone(); }
            Ok(out)
        }).collect()
    }

    let mut original_names = std::collections::BTreeSet::new();
    names(
        body.get("tools").unwrap_or(&Value::Null),
        &mut original_names,
    );
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for name in &original_names {
        *counts.entry(sanitize(name)).or_default() += 1;
    }
    let mut function_names = std::collections::BTreeMap::new();
    let mut used = std::collections::BTreeSet::new();
    for name in original_names {
        let base = sanitize(&name);
        let mut mapped = base.clone();
        if counts.get(&base).copied().unwrap_or(0) > 1 || used.contains(&mapped) {
            for attempt in 0u64.. {
                let digest = Sha256::digest(format!("{name}\0{attempt}").as_bytes());
                let suffix: String = digest
                    .iter()
                    .take(6)
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                mapped = format!("{}_{}", &base[..base.len().min(51)], suffix);
                if !used.contains(&mapped) {
                    break;
                }
            }
        }
        used.insert(mapped.clone());
        function_names.insert(name, mapped);
    }

    let mut contents = Vec::new();
    if let Some(input) = body.get("input") {
        if let Some(text) = input.as_str() {
            contents.push(json!({"role":"user","parts":[{"text":text}]}));
        } else {
            let steps = input.as_array().ok_or("input must be a string or array")?;
            for step in steps {
                let kind = step.get("type").and_then(Value::as_str).unwrap_or("");
                let (role, mut converted) = match kind {
                    "user_input" | "model_output" | "" => (
                        if kind == "model_output"
                            || matches!(
                                step.get("role").and_then(Value::as_str),
                                Some("model" | "assistant")
                            )
                        {
                            "model"
                        } else {
                            "user"
                        },
                        part_list(step.get("content").ok_or("input step requires content")?)?,
                    ),
                    "function_call" => {
                        let name = step
                            .get("name")
                            .and_then(Value::as_str)
                            .ok_or("function_call requires name")?;
                        let args = match step.get("arguments") {
                            Some(Value::String(raw)) => {
                                serde_json::from_str::<Value>(raw).map_err(|e| e.to_string())?
                            }
                            Some(value) => value.clone(),
                            None => json!({}),
                        };
                        let mut part = json!({"functionCall":{"id":step.get("id").and_then(Value::as_str).ok_or("function_call requires id")?,"name":function_names.get(name).unwrap_or(&name.to_string()),"args":args}});
                        if let Some(signature) = step
                            .get("thought_signature")
                            .or_else(|| step.get("signature"))
                        {
                            part["thoughtSignature"] = signature.clone();
                        }
                        ("model", vec![part])
                    }
                    "function_result" => {
                        let name = step
                            .get("name")
                            .and_then(Value::as_str)
                            .ok_or("function_result requires name")?;
                        let result = step
                            .get("result")
                            .or_else(|| step.get("output"))
                            .ok_or("function_result requires result")?;
                        let response = if result.is_object() {
                            result.clone()
                        } else {
                            json!({"result":result})
                        };
                        (
                            "user",
                            vec![
                                json!({"functionResponse":{"id":step.get("call_id").and_then(Value::as_str).ok_or("function_result requires call_id")?,"name":function_names.get(name).unwrap_or(&name.to_string()),"response":response}}),
                            ],
                        )
                    }
                    "thought" => {
                        let mut parts = if let Some(content) = step.get("content") {
                            part_list(content)?
                        } else {
                            vec![
                                json!({"text":step.get("text").and_then(Value::as_str).unwrap_or("")}),
                            ]
                        };
                        for part in &mut parts {
                            part["thought"] = json!(true);
                            if let Some(signature) = step
                                .get("signature")
                                .or_else(|| step.get("thought_signature"))
                                .or_else(|| step.get("thoughtSignature"))
                            {
                                part["thoughtSignature"] = signature.clone();
                            }
                        }
                        ("model", parts)
                    }
                    _ => return Err(format!("unsupported Interactions input type: {kind}")),
                };
                if let Some(last) = contents
                    .last_mut()
                    .filter(|last| last.get("role").and_then(Value::as_str) == Some(role))
                {
                    if let Some(parts) = last["parts"].as_array_mut() {
                        parts.append(&mut converted);
                    }
                } else {
                    contents.push(json!({"role":role,"parts":converted}));
                }
            }
        }
    } else {
        return Err("missing input".into());
    }

    let mut out = json!({"contents":contents});
    if let Some(system) = body.get("system_instruction") {
        out["systemInstruction"] = if system.is_object() {
            if let Some(text) = system.get("text").filter(|_| system.get("parts").is_none()) {
                json!({"parts":[{"text":text}]})
            } else {
                system.clone()
            }
        } else {
            json!({"parts":part_list(system)?})
        };
    }
    fn camel(value: &Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, value)| {
                        let mut words = key.split('_');
                        let mut name = words.next().unwrap_or("").to_owned();
                        for word in words {
                            let mut chars = word.chars();
                            if let Some(first) = chars.next() {
                                name.extend(first.to_uppercase());
                                name.extend(chars);
                            }
                        }
                        (name, camel(value))
                    })
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.iter().map(camel).collect()),
            _ => value.clone(),
        }
    }
    let mut generation = body
        .get("generation_config")
        .map(camel)
        .or_else(|| body.get("generationConfig").cloned())
        .unwrap_or_else(|| json!({}));
    if !generation.is_object() {
        return Err("generation config must be an object".into());
    }
    for field in ["thinkingLevel", "thinkingBudget", "includeThoughts"] {
        if let Some(value) = generation.as_object_mut().and_then(|map| map.remove(field)) {
            generation["thinkingConfig"][field] = value;
        }
    }
    fn summary(value: &Value) -> Option<bool> {
        match value.as_str()?.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(true),
            "none" => Some(false),
            _ => None,
        }
    }
    if let Some(value) = generation
        .as_object_mut()
        .and_then(|map| map.remove("thinkingSummaries"))
        .as_ref()
        .and_then(summary)
    {
        generation["thinkingConfig"]["includeThoughts"] = json!(value);
    }
    let tool_choice = body
        .get("tool_choice")
        .or_else(|| body.pointer("/generation_config/tool_choice"))
        .or_else(|| body.pointer("/generationConfig/toolChoice"));
    if let Some(map) = generation.as_object_mut() {
        map.remove("toolChoice");
    }
    if let Some(reasoning) = body.get("reasoning") {
        if let Some(effort) = reasoning
            .get("effort")
            .or_else(|| reasoning.get("thinking_level"))
            .and_then(Value::as_str)
        {
            let effort = effort.trim().to_ascii_lowercase();
            if effort == "auto" {
                generation["thinkingConfig"]["thinkingBudget"] = json!(-1);
            } else if !effort.is_empty() {
                generation["thinkingConfig"]["thinkingLevel"] = json!(effort);
            }
        }
        if let Some(value) = reasoning.get("summary").and_then(summary) {
            generation["thinkingConfig"]["includeThoughts"] = json!(value);
        }
    }
    if let Some(modalities) = body
        .get("response_modalities")
        .or_else(|| body.get("responseModalities"))
        .and_then(Value::as_array)
    {
        let mapped: Vec<_> = modalities
            .iter()
            .filter_map(Value::as_str)
            .map(|value| value.trim().to_ascii_uppercase())
            .filter(|value| matches!(value.as_str(), "TEXT" | "IMAGE" | "AUDIO"))
            .collect();
        if !mapped.is_empty() {
            generation["responseModalities"] = json!(mapped);
        }
    }
    if generation.as_object().is_some_and(|map| !map.is_empty()) {
        out["generationConfig"] = generation;
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let mut declarations = Vec::new();
        let mut other_tools = Vec::new();
        for tool in tools {
            let funcs = tool
                .get("functionDeclarations")
                .or_else(|| tool.get("function_declarations"))
                .and_then(Value::as_array);
            let direct = [tool.clone()];
            let is_function = tool.get("type").and_then(Value::as_str) == Some("function")
                || tool.get("name").is_some();
            if funcs.is_none() && !is_function {
                let mut translated = tool.clone();
                for (snake, camel) in [
                    ("url_context", "urlContext"),
                    ("code_execution", "codeExecution"),
                    ("google_search", "googleSearch"),
                    ("web_search", "googleSearch"),
                ] {
                    if let Some(value) =
                        translated.as_object_mut().and_then(|map| map.remove(snake))
                    {
                        translated[camel] = value;
                    }
                }
                let known = ["urlContext", "codeExecution", "googleSearch"];
                if let Some(kind) = translated.get("type").and_then(Value::as_str) {
                    let key = match kind {
                        "url_context" => Some("urlContext"),
                        "code_execution" => Some("codeExecution"),
                        "google_search" | "web_search" => Some("googleSearch"),
                        _ => None,
                    };
                    if let Some(key) = key {
                        translated =
                            json!({key:translated.get(key).cloned().unwrap_or_else(|| json!({}))});
                    }
                } else if translated
                    .as_object()
                    .is_some_and(|map| map.keys().all(|key| !known.contains(&key.as_str())))
                {
                    translated = json!({"googleSearch":translated});
                }
                other_tools.push(translated);
                continue;
            }
            for function in funcs.map(Vec::as_slice).unwrap_or(&direct) {
                let function = function.get("function").unwrap_or(function);
                let Some(name) = function
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty())
                else {
                    continue;
                };
                let mut declaration = json!({"name":function_names.get(name).unwrap_or(&name.to_string()),"parametersJsonSchema":function.get("parametersJsonSchema").or_else(|| function.get("parameters")).cloned().unwrap_or_else(|| json!({"type":"object","properties":{}}))});
                if let Some(description) = function.get("description") {
                    declaration["description"] = description.clone();
                }
                for field in ["response", "responseJsonSchema"] {
                    if let Some(value) = function.get(field) {
                        declaration[field] = value.clone();
                    }
                }
                declarations.push(declaration);
            }
        }
        if !declarations.is_empty() {
            other_tools.insert(0, json!({"functionDeclarations":declarations}));
        }
        if !other_tools.is_empty() {
            out["tools"] = json!(other_tools);
        }
    }
    if let Some(choice) = tool_choice {
        let kind = choice
            .as_str()
            .or_else(|| choice.get("type").and_then(Value::as_str))
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let mode = match kind.as_str() {
            "none" => Some("NONE"),
            "auto" => Some("AUTO"),
            "required" | "any" | "function" | "tool" => Some("ANY"),
            _ => None,
        };
        if let Some(mode) = mode {
            out["toolConfig"] = json!({"functionCallingConfig":{"mode":mode}});
            if let Some(name) = choice
                .pointer("/function/name")
                .or_else(|| choice.get("name"))
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
            {
                out["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"] = json!([name]);
            }
            if mode == "NONE" {
                out.as_object_mut().unwrap().remove("tools");
            }
        }
    }
    if let Some(function_calling) = body.pointer("/toolConfig/functionCallingConfig") {
        out["toolConfig"] = json!({"functionCallingConfig":function_calling});
    }
    out["safetySettings"] = body
        .get("safetySettings")
        .or_else(|| body.get("safety_settings"))
        .cloned()
        .unwrap_or_else(|| {
            json!([
                {"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"},
                {"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"OFF"},
                {"category":"HARM_CATEGORY_SEXUALLY_EXPLICIT","threshold":"OFF"},
                {"category":"HARM_CATEGORY_DANGEROUS_CONTENT","threshold":"OFF"},
                {"category":"HARM_CATEGORY_CIVIC_INTEGRITY","threshold":"BLOCK_NONE"}
            ])
        });
    Ok(out)
}

pub fn interactions_function_names(body: &Value) -> Vec<(String, String)> {
    let Ok(translated) = interactions_to_gemini(body) else {
        return Vec::new();
    };
    let declarations = translated
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| tool.get("functionDeclarations").and_then(Value::as_array))
        .flatten();
    let originals = body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|tool| {
            tool.get("functionDeclarations")
                .or_else(|| tool.get("function_declarations"))
                .and_then(Value::as_array)
                .map(|functions| functions.iter().collect::<Vec<_>>())
                .unwrap_or_else(|| vec![tool])
        })
        .filter_map(|function| {
            let function = function.get("function").unwrap_or(function);
            function
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    originals
        .zip(declarations)
        .filter_map(|(original, declaration)| {
            declaration
                .get("name")
                .and_then(Value::as_str)
                .map(|mapped| (original, mapped.to_owned()))
        })
        .collect()
}

const SIGNATURE_ID_SEPARATOR: char = '#';
const GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR: &str = "skip_thought_signature_validator";

#[cfg(test)]
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

fn build_thinking_config(model: &str, body: &Value) -> Option<Value> {
    let mut config = Map::new();

    let reasoning_effort = body.get("reasoning_effort").and_then(Value::as_str);
    let explicit_budget = body
        .pointer("/thinking/budget_tokens")
        .or_else(|| body.get("thinking_budget"))
        .and_then(Value::as_i64);

    if reasoning_effort == Some("none") {
        config.insert("thinkingBudget".to_string(), json!(0));
        return Some(Value::Object(config));
    }

    let thinking_level = match reasoning_effort {
        Some("high") => Some("HIGH"),
        Some("medium") => Some("HIGH"),
        Some("low") => Some("LOW"),
        _ => {
            if model.ends_with("-high") || model.contains("-high") {
                Some("HIGH")
            } else if model.ends_with("-low") || model.contains("-low") {
                Some("LOW")
            } else if is_thinking_model(model) {
                Some("HIGH")
            } else {
                None
            }
        }
    };

    if let Some(level) = thinking_level {
        config.insert("thinkingLevel".to_string(), json!(level));
        config.insert("includeThoughts".to_string(), json!(true));
    } else if let Some(budget) = explicit_budget {
        config.insert("thinkingBudget".to_string(), json!(budget));
        config.insert("includeThoughts".to_string(), json!(true));
    } else if is_thinking_model(model) {
        config.insert("includeThoughts".to_string(), json!(true));
    }

    if config.is_empty() {
        None
    } else {
        Some(Value::Object(config))
    }
}

fn extract_text_from_parts(parts: Option<&Value>) -> String {
    let mut text = String::new();
    if let Some(arr) = parts.and_then(Value::as_array) {
        for p in arr {
            if let Some(t) = p.get("text").and_then(Value::as_str) {
                text.push_str(t);
            }
        }
    }
    text
}

pub fn gemini_to_openai(req: &Value, model: &str) -> Result<Value, String> {
    gemini_to_openai_inner(req, model, None)
}

pub fn gemini_to_openai_with_replay(
    req: &Value,
    model: &str,
    replay: &ReplayScope,
) -> Result<Value, String> {
    gemini_to_openai_inner(req, model, Some(replay))
}

fn gemini_to_openai_inner(
    req: &Value,
    model: &str,
    replay: Option<&ReplayScope>,
) -> Result<Value, String> {
    let mut out_messages = Vec::new();

    // 1. System instructions
    if let Some(sys) = req
        .get("systemInstruction")
        .or_else(|| req.get("system_instruction"))
    {
        let text = extract_text_from_parts(sys.get("parts"));
        if !text.is_empty() {
            out_messages.push(json!({
                "role": "system",
                "content": text
            }));
        }
    }

    // 2. Contents
    if let Some(contents) = req.get("contents").and_then(Value::as_array) {
        for turn in contents {
            let role_str = turn.get("role").and_then(Value::as_str).unwrap_or("user");
            let is_model = role_str == "model" || role_str == "assistant";

            let mut text_parts = Vec::new();
            let mut reasoning_parts = Vec::new();
            let mut reasoning_sig: Option<String> = None;
            let mut tool_calls = Vec::new();
            let mut tool_results = Vec::new();

            if let Some(parts) = turn.get("parts").and_then(Value::as_array) {
                for part in parts {
                    if let Some(sig) = part
                        .get("thoughtSignature")
                        .or_else(|| part.get("thought_signature"))
                        .and_then(Value::as_str)
                    {
                        reasoning_sig = Some(sig.to_string());
                    }
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        if part.get("thought").and_then(Value::as_bool) == Some(true) {
                            reasoning_parts.push(text.to_string());
                        } else {
                            text_parts.push(text.to_string());
                        }
                    }
                    if let Some(call) = part
                        .get("functionCall")
                        .or_else(|| part.get("function_call"))
                    {
                        let name = call.get("name").and_then(Value::as_str).unwrap_or("");
                        let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
                        let call_id = call
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|id| !id.is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| signature_ledger::synthetic_call_id(name));
                        if let (Some(replay), Some(sig)) = (replay, reasoning_sig.as_ref()) {
                            replay.remember(&call_id, name, &args.to_string(), sig);
                        }
                        tool_calls.push(json!({
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": args.to_string()
                            }
                        }));
                    }
                    if let Some(resp) = part
                        .get("functionResponse")
                        .or_else(|| part.get("function_response"))
                    {
                        let name = resp.get("name").and_then(Value::as_str).unwrap_or("");
                        let response = resp.get("response").cloned().unwrap_or_else(|| json!({}));
                        let explicit_id = resp
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|id| !id.is_empty());
                        let call_id = explicit_id
                            .map(str::to_string)
                            .unwrap_or_else(|| signature_ledger::synthetic_call_id(name));
                        tool_results.push(json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "content": serde_json::to_string(&response).unwrap_or_default()
                        }));
                    }
                }
            }

            if is_model {
                let mut m = serde_json::Map::new();
                m.insert("role".to_string(), json!("assistant"));
                m.insert("content".to_string(), json!(text_parts.join("")));
                if !tool_calls.is_empty() {
                    m.insert("tool_calls".to_string(), Value::Array(tool_calls));
                }
                if !reasoning_parts.is_empty() {
                    m.insert(
                        "reasoning_content".to_string(),
                        json!(reasoning_parts.join("")),
                    );
                }
                if let Some(sig) = reasoning_sig {
                    m.insert("reasoning_signature".to_string(), json!(sig));
                }
                out_messages.push(Value::Object(m));
            } else {
                if !text_parts.is_empty() {
                    out_messages.push(json!({
                        "role": "user",
                        "content": text_parts.join("")
                    }));
                }
                out_messages.extend(tool_results);
            }
        }
    }

    let mut out = serde_json::Map::new();
    out.insert("model".to_string(), json!(model));
    out.insert("messages".to_string(), Value::Array(out_messages));

    // Tools
    if let Some(tools_arr) = req.get("tools").and_then(Value::as_array) {
        let mut out_tools = Vec::new();
        for t in tools_arr {
            if let Some(funcs) = t
                .get("functionDeclarations")
                .or_else(|| t.get("function_declarations"))
                .and_then(Value::as_array)
            {
                for func in funcs {
                    let name = func.get("name").and_then(Value::as_str).unwrap_or("");
                    let desc = func
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let params = func
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object"}));
                    out_tools.push(json!({
                        "type": "function",
                        "function": {
                            "name": name,
                            "description": desc,
                            "parameters": params
                        }
                    }));
                }
            }
        }
        if !out_tools.is_empty() {
            out.insert("tools".to_string(), Value::Array(out_tools));
        }
    }

    // Tool config
    if let Some(cfg) = req
        .pointer("/toolConfig/functionCallingConfig/mode")
        .or_else(|| req.pointer("/tool_config/function_calling_config/mode"))
        .and_then(Value::as_str)
    {
        match cfg {
            "AUTO" | "auto" => {
                out.insert("tool_choice".to_string(), json!("auto"));
            }
            "NONE" | "none" => {
                out.insert("tool_choice".to_string(), json!("none"));
            }
            "ANY" | "any" => {
                out.insert("tool_choice".to_string(), json!("required"));
            }
            _ => {}
        }
    }

    // Generation config
    if let Some(gen_cfg) = req
        .get("generationConfig")
        .or_else(|| req.get("generation_config"))
    {
        if let Some(t) = gen_cfg.get("temperature").and_then(Value::as_f64) {
            out.insert("temperature".to_string(), json!(t));
        }
        if let Some(p) = gen_cfg
            .get("topP")
            .or_else(|| gen_cfg.get("top_p"))
            .and_then(Value::as_f64)
        {
            out.insert("top_p".to_string(), json!(p));
        }
        if let Some(m) = gen_cfg
            .get("maxOutputTokens")
            .or_else(|| gen_cfg.get("max_output_tokens"))
            .and_then(Value::as_i64)
        {
            out.insert("max_tokens".to_string(), json!(m));
        }
    }

    Ok(Value::Object(out))
}

pub fn openai_to_gemini(body: &Value) -> Result<Value, String> {
    openai_to_gemini_inner(body, false, None)
}

pub fn openai_to_gemini_with_replay(body: &Value, replay: &ReplayScope) -> Result<Value, String> {
    openai_to_gemini_inner(body, false, Some(replay))
}

fn openai_to_gemini_inner(
    body: &Value,
    claude_tools: bool,
    replay: Option<&ReplayScope>,
) -> Result<Value, String> {
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

    let mut pending_calls: Vec<(String, String, String, String)> = Vec::new();
    let mut seen_call_ids: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut generated_call_index = 0_u64;

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
                let mut first_function_call_seen = false;
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
                    let (plain_id, embedded_signature) = split_signature_from_call_id(raw_id);
                    let arguments = args.to_string();
                    let signature = embedded_signature
                        .or_else(|| {
                            call.get("thought_signature")
                                .or_else(|| call.get("thoughtSignature"))
                                .or_else(|| call.get("signature"))
                                .or_else(|| call.pointer("/extra_content/google/thought_signature"))
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .or_else(|| {
                            replay.and_then(|replay| replay.recall(&plain_id, name, &arguments))
                        });
                    let base_id = if plain_id.is_empty() {
                        let generated = format!("call_{name}_{generated_call_index}");
                        generated_call_index += 1;
                        generated
                    } else {
                        plain_id.clone()
                    };
                    let occurrence = seen_call_ids.entry(base_id.clone()).or_default();
                    *occurrence += 1;
                    let outbound_id = if *occurrence == 1 {
                        base_id
                    } else {
                        format!("{base_id}-{occurrence}")
                    };
                    pending_calls.push((
                        raw_id.to_string(),
                        plain_id,
                        name.to_string(),
                        outbound_id.clone(),
                    ));
                    if let Some(signature) = signature {
                        parts.push(json!({
                            "functionCall": { "id": outbound_id, "name": name, "args": args },
                            "thoughtSignature": signature,
                        }));
                    } else if !first_function_call_seen {
                        if let Some(replay) = replay {
                            replay.record_unsigned_replay();
                        }
                        parts.push(json!({
                            "functionCall": { "id": outbound_id, "name": name, "args": args },
                            "thoughtSignature": GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
                        }));
                    } else {
                        parts.push(json!({
                            "functionCall": { "id": outbound_id, "name": name, "args": args }
                        }));
                    }
                    first_function_call_seen = true;
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
                let explicit_name = msg
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty());
                let (plain_id, _) = split_signature_from_call_id(call_id);
                let pending = pending_calls
                    .iter()
                    .position(|(raw, plain, _, _)| raw == call_id || plain == &plain_id)
                    .map(|index| pending_calls.remove(index));
                let name = pending
                    .as_ref()
                    .map(|(_, _, name, _)| name.clone())
                    .or_else(|| explicit_name.map(str::to_string))
                    .ok_or_else(|| {
                        format!(
                            "tool response references unknown tool_call_id {call_id:?}; Gemini functionResponse needs the originating function name"
                        )
                    })?;
                let outbound_id = pending
                    .map(|(_, _, _, outbound_id)| outbound_id)
                    .unwrap_or(plain_id);
                let raw = content_to_text(msg.get("content")).unwrap_or_default();
                let response = struct_response(&raw);
                let response_part = json!({
                    "functionResponse": { "id": outbound_id, "name": name, "response": response }
                });
                if let Some(parts) = contents
                    .last_mut()
                    .filter(|content| content.get("role").and_then(Value::as_str) == Some("user"))
                    .and_then(|content| content.get_mut("parts"))
                    .and_then(Value::as_array_mut)
                    .filter(|parts| {
                        parts
                            .iter()
                            .all(|part| part.get("functionResponse").is_some())
                    })
                {
                    parts.push(response_part);
                } else {
                    contents.push(json!({
                        "role": "user",
                        "parts": [response_part]
                    }));
                }
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
    // If the client sent an assistant prefill or retry at the tail, strip trailing
    // model turns so the request always terminates on a user or functionResponse turn.
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

    // Antigravity threads reasoning state per conversation, so a replayed
    // signature is only accepted under the session that produced it. The id is
    // the caller's stable scope id, never anything forwarded from ingress.
    if let Some(session_id) = replay
        .map(ReplayScope::session_id)
        .filter(|id| !id.is_empty())
    {
        request.insert("sessionId".to_string(), json!(session_id));
    }

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
    if let Some(thinking) = build_thinking_config(model, body) {
        generation.insert("thinkingConfig".to_string(), thinking);
    }
    if !generation.is_empty() {
        request.insert("generationConfig".to_string(), Value::Object(generation));
    }

    if let Some(tool_choice) = body.get("tool_choice") {
        if let Some(mode_str) = tool_choice.as_str() {
            let mode = match mode_str {
                "auto" => Some("AUTO"),
                "none" => Some("NONE"),
                "required" => Some("ANY"),
                _ => None,
            };
            if let Some(m) = mode {
                request.insert(
                    "toolConfig".to_string(),
                    json!({
                        "functionCallingConfig": { "mode": m }
                    }),
                );
            }
        } else if let Some(obj) = tool_choice.as_object() {
            if let Some(name) = obj
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
            {
                request.insert(
                    "toolConfig".to_string(),
                    json!({
                        "functionCallingConfig": {
                            "mode": "ANY",
                            "allowedFunctionNames": [name]
                        }
                    }),
                );
            }
        }
    }

    request.insert(
        "safetySettings".to_string(),
        json!([
            { "category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_NONE" },
            { "category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "BLOCK_NONE" },
            { "category": "HARM_CATEGORY_SEXUALLY_EXPLICIT", "threshold": "BLOCK_NONE" },
            { "category": "HARM_CATEGORY_DANGEROUS_CONTENT", "threshold": "BLOCK_NONE" },
            { "category": "HARM_CATEGORY_CIVIC_INTEGRITY", "threshold": "BLOCK_NONE" }
        ]),
    );

    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let decls: Vec<Value> = tools
            .iter()
            .enumerate()
            .filter_map(|(index, t)| t.get("function").map(|f| (index, f)))
            .map(|(index, f)| {
                let mut parameters = if claude_tools {
                    super::tool_schema::antigravity_claude_schema(f.get("parameters"))
                        .map_err(|error| format!("tools[{index}].function.parameters: {error}"))?
                } else {
                    f.get("parameters").cloned().unwrap_or_else(|| json!({}))
                };
                if claude_tools {
                    sanitize_claude_schema_for_gemini(&mut parameters)
                        .map_err(|error| format!("tools[{index}].function.parameters: {error}"))?;
                } else {
                    sanitize_gemini_schema(&mut parameters);
                }
                Ok(json!({
                    "name": f.get("name").and_then(Value::as_str).unwrap_or(""),
                    "description": f.get("description").and_then(Value::as_str).unwrap_or(""),
                    "parameters": parameters,
                }))
            })
            .collect::<Result<Vec<_>, String>>()?;
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
    "uniqueItems",
    "minProperties",
    "maxProperties",
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

// Unlike the legacy Gemini JSON walk, visit only schema positions: a tool may
// legitimately have a property named "const", "$defs", or "additionalProperties".
fn sanitize_claude_schema_for_gemini(value: &mut Value) -> Result<(), String> {
    if *value == Value::Bool(true) {
        *value = json!({});
    }
    let map = value
        .as_object_mut()
        .ok_or("Antigravity requires object-valued schema nodes")?;
    if let Some(constant) = map.remove("const") {
        if map
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|values| !values.contains(&constant))
        {
            return Err("tool schema const conflicts with enum".into());
        }
        map.insert("enum".into(), json!([constant]));
    }
    for key in GEMINI_UNSUPPORTED_SCHEMA_KEYS {
        map.remove(*key);
    }
    if let Some(one) = map.remove("oneOf") {
        map.entry("anyOf").or_insert(one);
    }
    super::tool_schema::visit_schema_children(map, &mut sanitize_claude_schema_for_gemini)
}

pub fn openai_to_antigravity(body: &Value, project_id: &str) -> Result<Value, String> {
    openai_to_antigravity_inner(body, project_id, None)
}

pub fn openai_to_antigravity_with_replay(
    body: &Value,
    project_id: &str,
    replay: &ReplayScope,
) -> Result<Value, String> {
    openai_to_antigravity_inner(body, project_id, Some(replay))
}

fn openai_to_antigravity_inner(
    body: &Value,
    project_id: &str,
    replay: Option<&ReplayScope>,
) -> Result<Value, String> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing model".to_string())?;

    // Antigravity uses Gemini's wire format even when its actual validator is
    // Anthropic. Keep this repair out of native Gemini and non-Claude models.
    let request = openai_to_gemini_inner(body, model.starts_with("claude-"), replay)?;

    // The envelope carries the client-identity fields the real Antigravity
    // client sends; CLIProxyAPI and OpenCodex both mirror them.
    Ok(json!({
        "model": model,
        "userAgent": "antigravity",
        "requestType": "agent",
        "project": project_id,
        "requestId": format!("agent-{}", uuid::Uuid::new_v4()),
        "request": request,
    }))
}

pub fn gemini_json_to_openai(body: &Value, model: &str, created: i64) -> Value {
    gemini_json_to_openai_inner(body, model, created, None)
}

pub fn gemini_json_to_openai_with_replay(
    body: &Value,
    model: &str,
    created: i64,
    replay: &ReplayScope,
) -> Value {
    gemini_json_to_openai_inner(body, model, created, Some(replay))
}

fn gemini_json_to_openai_inner(
    body: &Value,
    model: &str,
    created: i64,
    replay: Option<&ReplayScope>,
) -> Value {
    let response = body.get("response").unwrap_or(body);
    let candidate = response
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|c| c.first());

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut pending_signature: Option<String> = None;

    if let Some(parts) = candidate
        .and_then(|c| c.get("content"))
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
                let id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| signature_ledger::synthetic_call_id(name));
                let signature = part
                    .get("thoughtSignature")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| pending_signature.clone());
                if let (Some(replay), Some(signature)) = (replay, signature.as_deref()) {
                    replay.remember(&id, name, &args, signature);
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
                let is_thought = part
                    .get("thought")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if is_thought {
                    reasoning.push_str(t);
                    if let Some(sig) = part.get("thoughtSignature").and_then(Value::as_str) {
                        pending_signature = Some(sig.to_string());
                    }
                } else {
                    text.push_str(t);
                    pending_signature = None;
                }
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
    let reasoning_tokens = usage
        .and_then(|u| u.get("thoughtsTokenCount"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let mut message = if !tool_calls.is_empty() {
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
    if !reasoning.is_empty() {
        message["reasoning_content"] = Value::String(reasoning);
    }

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
            "completion_tokens_details": {
                "reasoning_tokens": reasoning_tokens,
            }
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
    saw_finish_reason: bool,
    pending_signature: Option<String>,
    replay: Option<ReplayScope>,
}

impl GeminiDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_replay(replay: ReplayScope) -> Self {
        Self {
            replay: Some(replay),
            ..Self::default()
        }
    }

    pub fn decode(&mut self, payload: &[u8], out: &mut Vec<CodexEvent>) {
        if self.completed {
            return;
        }
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

        let response = value.get("response").unwrap_or(&value);
        if let Some(err) = value.get("error").or_else(|| response.get("error")) {
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
            // A cache read is known only when the metric is numerically
            // present: zero is a measurement, absent is unknown. Gemini emits
            // no cache-write metric at all, so that one is never known.
            let cached = usage.get("cachedContentTokenCount").and_then(Value::as_u64);
            self.usage = Some(Usage {
                prompt_tokens: prompt,
                completion_tokens: completion,
                total_tokens: total,
                cached_tokens: cached.unwrap_or(0),
                cached_tokens_known: cached.is_some(),
                reasoning_tokens: usage
                    .get("thoughtsTokenCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cache_write_tokens: 0,
                cache_write_tokens_known: false,
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
                    let native_id = call
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty());
                    let signature = part
                        .get("thoughtSignature")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        // Clone, not take: upstream emits one standalone
                        // signature part for a whole parallel-call block, so
                        // every functionCall in the block must carry it.
                        .or_else(|| self.pending_signature.clone());
                    let call_id = native_id
                        .map(str::to_string)
                        .unwrap_or_else(|| signature_ledger::synthetic_call_id(name));
                    if let (Some(replay), Some(signature)) =
                        (self.replay.as_ref(), signature.as_deref())
                    {
                        replay.remember(&call_id, name, &args, signature);
                    }
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
                    let is_thought = part
                        .get("thought")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if is_thought {
                        out.push(CodexEvent::ReasoningDelta(text.to_string()));
                    } else if !text.is_empty() {
                        // Text ends the signature-bearing block; a stale
                        // signature must not leak into a later tool turn.
                        self.pending_signature = None;
                        out.push(CodexEvent::TextDelta(text.to_string()));
                    }
                }
            }
        }

        if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
            if reason == "MAX_TOKENS" {
                out.push(CodexEvent::OutputLimitReached);
            }
            self.saw_finish_reason = true;
        }
    }

    pub fn finish(&mut self, out: &mut Vec<CodexEvent>) {
        if self.completed {
            return;
        }
        self.completed = true;
        if self.saw_finish_reason {
            out.push(CodexEvent::Completed {
                usage: self.usage.take(),
            });
        } else {
            out.push(CodexEvent::Failed {
                message: "stream truncated before finish reason".to_string(),
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
                            "tags": {
                                "type": "array",
                                "items": { "type": "string" },
                                "uniqueItems": true
                            },
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
        assert!(params["properties"]["tags"].get("uniqueItems").is_none());
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
        assert_eq!(parts[0]["functionCall"]["id"], "call_todo_4");
        assert_eq!(parts[0]["thoughtSignature"], "SIG");
        assert_eq!(
            request["contents"][2]["parts"][0]["functionResponse"]["id"],
            "call_todo_4"
        );
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
    fn gemini_json_to_openai_parks_the_signature_under_the_native_call_id() {
        let response = json!({
            "candidates": [{ "content": { "parts": [
                { "functionCall": { "id": "native-call-17", "name": "todo", "args": {} },
                  "thoughtSignature": "SIGJSON" }
            ]}}]
        });

        let ledger = signature_ledger::SignatureLedger::in_memory();
        let replay = ledger.scope("gemini-3.7-flash-high", "session-json-park");
        let out = gemini_json_to_openai_with_replay(&response, "gemini-3.7-flash-high", 0, &replay);
        let id = out["choices"][0]["message"]["tool_calls"][0]["id"]
            .as_str()
            .expect("tool call id");
        assert_eq!(id, "native-call-17");
        assert_eq!(replay.recall(id, "todo", "{}").as_deref(), Some("SIGJSON"));
    }

    #[test]
    fn decoder_parks_the_signature_under_the_native_call_id() {
        let ledger = signature_ledger::SignatureLedger::in_memory();
        let replay = ledger.scope("gemini-3.8-flash-high", "session-stream-park");
        let mut decoder = GeminiDecoder::with_replay(replay.clone());
        let mut out = Vec::new();
        decoder.decode(
            br#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"native-stream-23","name":"todo","args":{}},"thoughtSignature":"SIGSTREAM"}]}}]}"#,
            &mut out,
        );
        let begin = out.iter().find_map(|event| match event {
            CodexEvent::ToolCallBegin { call_id, .. } => Some(call_id.clone()),
            _ => None,
        });
        let call_id = begin.expect("tool call begin");
        assert_eq!(call_id, "native-stream-23");
        assert_eq!(
            replay.recall(&call_id, "todo", "{}").as_deref(),
            Some("SIGSTREAM")
        );
    }

    #[test]
    fn a_parked_signature_replays_into_the_next_request() {
        let ledger = signature_ledger::SignatureLedger::in_memory();
        let replay = ledger.scope("gemini-3.8-flash-high", "session-replay");
        let mut decoder = GeminiDecoder::with_replay(replay.clone());
        let mut out = Vec::new();
        decoder.decode(
            br#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"replay-native-1","name":"bash","args":{"command":"ls"}},"thoughtSignature":"SIGREPLAY"}]}}]}"#,
            &mut out,
        );
        let call_id = out
            .iter()
            .find_map(|event| match event {
                CodexEvent::ToolCallBegin { call_id, .. } => Some(call_id.clone()),
                _ => None,
            })
            .expect("tool call begin");

        let body = json!({
            "model": "gemini-3.8-flash-high",
            "messages": [
                { "role": "user", "content": "list files" },
                { "role": "assistant", "tool_calls": [{
                    "id": call_id, "type": "function",
                    "function": { "name": "bash", "arguments": "{\"command\":\"ls\"}" }
                }]},
                { "role": "tool", "tool_call_id": call_id, "content": "out" }
            ]
        });
        let request = openai_to_gemini_with_replay(&body, &replay).expect("translate");
        assert_eq!(
            request["contents"][1]["parts"][0]["thoughtSignature"],
            "SIGREPLAY"
        );
    }

    #[test]
    fn a_signature_parked_under_one_session_never_replays_into_another() {
        let ledger = signature_ledger::SignatureLedger::in_memory();
        let first = ledger.scope("gemini-3.8-flash-high", "session-one");
        let second = ledger.scope("gemini-3.8-flash-high", "session-two");
        let other_model = ledger.scope("gemini-3.7-flash-high", "session-one");
        let mut decoder = GeminiDecoder::with_replay(first.clone());
        let mut out = Vec::new();
        decoder.decode(
            br#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"scoped-1","name":"bash","args":{}},"thoughtSignature":"SIGSCOPED"}]}}]}"#,
            &mut out,
        );
        let body = json!({
            "model": "gemini-3.8-flash-high",
            "messages": [
                { "role": "user", "content": "go" },
                { "role": "assistant", "tool_calls": [{
                    "id": "scoped-1", "type": "function",
                    "function": { "name": "bash", "arguments": "{}" }
                }]},
                { "role": "tool", "tool_call_id": "scoped-1", "content": "out" }
            ]
        });

        assert_eq!(
            openai_to_gemini_with_replay(&body, &first).expect("translate")["contents"][1]["parts"]
                [0]["thoughtSignature"],
            "SIGSCOPED"
        );
        for foreign in [&second, &other_model] {
            assert_eq!(
                openai_to_gemini_with_replay(&body, foreign).expect("translate")["contents"][1]
                    ["parts"][0]["thoughtSignature"],
                GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
                "another model or session must not inherit a parked signature"
            );
        }
    }

    #[test]
    fn an_unscoped_conversion_parks_nothing_and_replays_nothing() {
        let ledger = signature_ledger::SignatureLedger::in_memory();
        let mut decoder = GeminiDecoder::new();
        let mut out = Vec::new();
        decoder.decode(
            br#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"unscoped-1","name":"bash","args":{}},"thoughtSignature":"SIGUNSCOPED"}]}}]}"#,
            &mut out,
        );
        assert_eq!(ledger.write_count(), 0);
        assert!(ledger.is_empty());
    }

    #[test]
    fn a_scoped_request_carries_a_stable_session_id_and_an_unscoped_one_omits_it() {
        let ledger = signature_ledger::SignatureLedger::in_memory();
        let replay = ledger.scope("gemini-3.8-flash-high", "session-wire-1");
        let body = json!({
            "model": "gemini-3.8-flash-high",
            "messages": [{ "role": "user", "content": "hello" }]
        });

        let first = openai_to_gemini_with_replay(&body, &replay).expect("translate");
        let second = openai_to_gemini_with_replay(&body, &replay).expect("translate");
        assert_eq!(first["sessionId"], "session-wire-1");
        assert_eq!(
            first["sessionId"], second["sessionId"],
            "the session id must be stable across requests in one session"
        );

        let other = ledger.scope("gemini-3.8-flash-high", "session-wire-2");
        assert_eq!(
            openai_to_gemini_with_replay(&body, &other).expect("translate")["sessionId"],
            "session-wire-2"
        );
        assert!(
            openai_to_gemini(&body)
                .expect("translate")
                .get("sessionId")
                .is_none(),
            "an unscoped conversion must not invent a session id"
        );
    }

    #[test]
    fn an_antigravity_request_nests_the_session_id_under_request() {
        let ledger = signature_ledger::SignatureLedger::in_memory();
        let replay = ledger.scope("gemini-3.8-flash-high", "session-antigravity");
        let body = json!({
            "model": "gemini-3.8-flash-high",
            "messages": [{ "role": "user", "content": "hello" }]
        });

        let out =
            openai_to_antigravity_with_replay(&body, "project-1", &replay).expect("translate");
        assert_eq!(out["request"]["sessionId"], "session-antigravity");
        assert_eq!(out["project"], "project-1");
        assert!(out.get("sessionId").is_none());
        assert!(
            openai_to_antigravity(&body, "project-1").expect("translate")["request"]
                .get("sessionId")
                .is_none()
        );
    }

    #[test]
    fn a_present_cache_read_metric_is_known_and_an_absent_one_is_not() {
        let decode_usage = |payload: Value| {
            let mut decoder = GeminiDecoder::new();
            let mut out = Vec::new();
            decoder.decode(payload.to_string().as_bytes(), &mut out);
            decoder.finish(&mut out);
            out.into_iter().find_map(|event| match event {
                CodexEvent::Completed { usage } => usage,
                _ => None,
            })
        };

        let reported = decode_usage(json!({
            "candidates": [{ "content": { "parts": [] }, "finishReason": "STOP" }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 4,
                "cachedContentTokenCount": 0
            }
        }))
        .expect("usage");
        assert!(
            reported.cached_tokens_known,
            "a reported zero is a measurement"
        );
        assert_eq!(reported.cached_tokens, 0);
        assert!(!reported.cache_write_tokens_known);

        let absent = decode_usage(json!({
            "candidates": [{ "content": { "parts": [] }, "finishReason": "STOP" }],
            "usageMetadata": { "promptTokenCount": 10, "candidatesTokenCount": 4 }
        }))
        .expect("usage");
        assert!(
            !absent.cached_tokens_known,
            "an absent metric is unknown, not a zero"
        );
        assert_eq!(absent.cached_tokens, 0);

        let hit = decode_usage(json!({
            "candidates": [{ "content": { "parts": [] }, "finishReason": "STOP" }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 4,
                "cachedContentTokenCount": 7
            }
        }))
        .expect("usage");
        assert!(hit.cached_tokens_known);
        assert_eq!(hit.cached_tokens, 7);
    }

    #[test]
    fn client_visible_call_ids_stay_short_for_large_signatures() {
        let signature = "A".repeat(40_000);
        let response = json!({
            "candidates": [{ "content": { "parts": [
                { "functionCall": { "id": "native-large-1", "name": "eval", "args": {} },
                  "thoughtSignature": signature }
            ]}}]
        });

        let ledger = signature_ledger::SignatureLedger::in_memory();
        let replay = ledger.scope("gemini-3.8-flash-high", "session-large");
        let out = gemini_json_to_openai_with_replay(&response, "gemini-3.8-flash-high", 0, &replay);
        let id = out["choices"][0]["message"]["tool_calls"][0]["id"]
            .as_str()
            .expect("tool call id");
        assert_eq!(id, "native-large-1");
        assert_eq!(replay.recall(id, "eval", "{}"), Some(signature));
    }

    #[test]
    fn an_unparked_history_call_falls_back_to_the_bypass_sentinel() {
        let body = json!({
            "model": "gemini-3.8-flash-high",
            "messages": [
                { "role": "user", "content": "go" },
                { "role": "assistant", "tool_calls": [{
                    "id": "never-parked-1", "type": "function",
                    "function": { "name": "bash", "arguments": "{}" }
                }]},
                { "role": "tool", "tool_call_id": "never-parked-1", "content": "out" }
            ]
        });
        let request = openai_to_gemini(&body).expect("translate");
        assert_eq!(
            request["contents"][1]["parts"][0]["thoughtSignature"],
            GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
        );
    }

    #[test]
    fn a_reused_call_id_with_other_arguments_does_not_replay_a_stale_signature() {
        let ledger = signature_ledger::SignatureLedger::in_memory();
        let replay = ledger.scope("gemini-3.8-flash-high", "session-stale");
        replay.remember("stale-native-1", "eval", "{\"offset\":10}", "SIGSTALE");
        let body = json!({
            "model": "gemini-3.8-flash-high",
            "messages": [
                { "role": "user", "content": "go" },
                { "role": "assistant", "tool_calls": [{
                    "id": "stale-native-1", "type": "function",
                    "function": { "name": "eval", "arguments": "{\"offset\":20}" }
                }]},
                { "role": "tool", "tool_call_id": "stale-native-1", "content": "out" }
            ]
        });
        let request = openai_to_gemini_with_replay(&body, &replay).expect("translate");
        assert_eq!(
            request["contents"][1]["parts"][0]["thoughtSignature"],
            GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
        );
    }

    #[test]
    fn independent_responses_keep_distinct_native_tool_call_ids() {
        let decode_id = |native_id: &str| {
            let payload = json!({
                "candidates": [{ "content": { "parts": [{
                    "functionCall": { "id": native_id, "name": "eval", "args": {} },
                    "thoughtSignature": "SAME-SIGNATURE"
                }]}}]
            });
            let mut decoder = GeminiDecoder::new();
            let mut out = Vec::new();
            decoder.decode(payload.to_string().as_bytes(), &mut out);
            out.into_iter().find_map(|event| match event {
                CodexEvent::ToolCallBegin { call_id, .. } => Some(call_id),
                _ => None,
            })
        };

        let first = decode_id("eval-native-a").expect("first call id");
        let second = decode_id("eval-native-b").expect("second call id");
        assert_ne!(first, second, "independent calls must not collide");
        assert_eq!(first, "eval-native-a");
        assert_eq!(second, "eval-native-b");
    }

    #[test]
    fn synthetic_call_ids_stay_unique_when_upstream_omits_them() {
        let decode_id = || {
            let mut decoder = GeminiDecoder::new();
            let mut out = Vec::new();
            decoder.decode(
                br#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"eval","args":{}},"thoughtSignature":"SAME-SIGNATURE"}]}}]}"#,
                &mut out,
            );
            out.into_iter().find_map(|event| match event {
                CodexEvent::ToolCallBegin { call_id, .. } => Some(call_id),
                _ => None,
            })
        };

        let first = decode_id().expect("first call id");
        let second = decode_id().expect("second call id");
        assert_ne!(first, second, "unidentified calls must not collide");
    }

    #[test]
    fn decoder_carries_a_standalone_signature_part_into_the_next_call_id() {
        let ledger = signature_ledger::SignatureLedger::in_memory();
        let replay = ledger.scope("gemini-3.8-flash-high", "session-pending");
        let mut decoder = GeminiDecoder::with_replay(replay.clone());
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
        assert_eq!(
            replay
                .recall(&begin.expect("tool call begin"), "todo", "{}")
                .as_deref(),
            Some("SIGPENDING")
        );
    }

    #[test]
    fn decoder_shares_a_pending_signature_across_parallel_calls() {
        let ledger = signature_ledger::SignatureLedger::in_memory();
        let replay = ledger.scope("gemini-3.8-flash-high", "session-parallel");
        let mut decoder = GeminiDecoder::with_replay(replay.clone());
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
        assert_eq!(
            replay.recall(&ids[0], "bash", "{}").as_deref(),
            Some("SIGBLOCK")
        );
        assert_eq!(
            replay.recall(&ids[1], "edit", "{}").as_deref(),
            Some("SIGBLOCK")
        );
    }

    #[test]
    fn unsigned_history_pairs_use_bypass_signature_and_are_preserved() {
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
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["functionCall"]["name"], "bash");
        assert_eq!(
            contents[1]["parts"][0]["thoughtSignature"],
            "skip_thought_signature_validator"
        );
        assert_eq!(contents[2]["role"], "user");
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["name"], "bash");
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
        assert_eq!(contents.len(), 3);
        let call_parts = contents[1]["parts"].as_array().expect("call parts");
        assert_eq!(call_parts.len(), 2);
        assert_eq!(call_parts[0]["functionCall"]["name"], "bash");
        assert_eq!(call_parts[0]["thoughtSignature"], "SIGREAL");
        assert_eq!(call_parts[1]["functionCall"]["name"], "edit");
        assert!(call_parts[1].get("thoughtSignature").is_none());
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["name"], "bash");
        assert_eq!(contents[2]["parts"][1]["functionResponse"]["name"], "edit");
    }

    #[test]
    fn repeated_openai_call_ids_are_disambiguated_chronologically() {
        let repeated_id = embed_signature_in_call_id("call_eval_0", "SAME-SIGNATURE");
        let body = json!({
            "model": "gemini-3.8-flash-high",
            "messages": [
                { "role": "user", "content": "inspect both ranges" },
                { "role": "assistant", "tool_calls": [{
                    "id": repeated_id,
                    "type": "function",
                    "function": { "name": "eval", "arguments": "{\"offset\":10}" }
                }]},
                { "role": "tool", "tool_call_id": repeated_id, "content": "first" },
                { "role": "assistant", "tool_calls": [{
                    "id": repeated_id,
                    "type": "function",
                    "function": { "name": "eval", "arguments": "{\"offset\":20}" }
                }]},
                { "role": "tool", "tool_call_id": repeated_id, "content": "second" }
            ]
        });

        let request = openai_to_gemini(&body).expect("translate");
        let contents = request["contents"].as_array().expect("contents");
        assert_eq!(contents.len(), 5);
        assert_eq!(contents[1]["parts"][0]["functionCall"]["id"], "call_eval_0");
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"]["id"],
            "call_eval_0"
        );
        assert_eq!(
            contents[3]["parts"][0]["functionCall"]["id"],
            "call_eval_0-2"
        );
        assert_eq!(
            contents[4]["parts"][0]["functionResponse"]["id"],
            "call_eval_0-2"
        );
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
    fn unsigned_tool_call_with_text_is_preserved_with_bypass_signature() {
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
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(
            contents[1]["parts"][0]["text"],
            "thinking aloud before tool call"
        );
        assert_eq!(
            contents[1]["parts"][1]["thoughtSignature"],
            "skip_thought_signature_validator"
        );
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["name"], "bash");
    }

    #[test]
    fn thinking_config_generated_for_high_flash_models() {
        let body = json!({
            "model": "gemini-3.8-flash-high",
            "messages": [{ "role": "user", "content": "hello" }]
        });
        let req = openai_to_gemini(&body).expect("translate");
        let thinking = &req["generationConfig"]["thinkingConfig"];
        assert_eq!(thinking["thinkingLevel"], "HIGH");
        assert_eq!(thinking["includeThoughts"], true);
    }

    #[test]
    fn thinking_config_respects_reasoning_effort() {
        let body_low = json!({
            "model": "gemini-3-flash",
            "reasoning_effort": "low",
            "messages": [{ "role": "user", "content": "hello" }]
        });
        let req_low = openai_to_gemini(&body_low).expect("translate");
        let thinking_low = &req_low["generationConfig"]["thinkingConfig"];
        assert_eq!(thinking_low["thinkingLevel"], "LOW");
        assert_eq!(thinking_low["includeThoughts"], true);

        let body_none = json!({
            "model": "gemini-3.8-flash-high",
            "reasoning_effort": "none",
            "messages": [{ "role": "user", "content": "hello" }]
        });
        let req_none = openai_to_gemini(&body_none).expect("translate");
        let thinking_none = &req_none["generationConfig"]["thinkingConfig"];
        assert_eq!(thinking_none["thinkingBudget"], 0);
    }

    #[test]
    fn safety_settings_are_included_with_block_none() {
        let body = json!({
            "model": "gemini-3.8-flash-high",
            "messages": [{ "role": "user", "content": "hello" }]
        });
        let req = openai_to_gemini(&body).expect("translate");
        let safety = req["safetySettings"].as_array().expect("safetySettings");
        assert_eq!(safety.len(), 5);
        for s in safety {
            assert_eq!(s["threshold"], "BLOCK_NONE");
        }
    }

    #[test]
    fn tool_choice_mode_maps_correctly() {
        let body_req = json!({
            "model": "gemini-3-flash",
            "messages": [{ "role": "user", "content": "hi" }],
            "tool_choice": "required"
        });
        let req_req = openai_to_gemini(&body_req).expect("translate");
        assert_eq!(
            req_req["toolConfig"]["functionCallingConfig"]["mode"],
            "ANY"
        );

        let body_named = json!({
            "model": "gemini-3-flash",
            "messages": [{ "role": "user", "content": "hi" }],
            "tool_choice": { "type": "function", "function": { "name": "my_tool" } }
        });
        let req_named = openai_to_gemini(&body_named).expect("translate");
        let fc = &req_named["toolConfig"]["functionCallingConfig"];
        assert_eq!(fc["mode"], "ANY");
        assert_eq!(fc["allowedFunctionNames"][0], "my_tool");
    }

    #[test]
    fn decoder_emits_reasoning_delta_for_thought_parts() {
        let mut decoder = GeminiDecoder::new();
        let mut out = Vec::new();
        decoder.decode(
            br#"{"candidates":[{"content":{"parts":[
                {"thought":true,"text":"pondering the answer"},
                {"text":"the actual answer"}
            ]}}]}"#,
            &mut out,
        );
        let reasoning: Vec<String> = out
            .iter()
            .filter_map(|e| match e {
                CodexEvent::ReasoningDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        let text: Vec<String> = out
            .iter()
            .filter_map(|e| match e {
                CodexEvent::TextDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning, vec!["pondering the answer"]);
        assert_eq!(text, vec!["the actual answer"]);
    }

    #[test]
    fn gemini_json_to_openai_separates_reasoning_content() {
        let gemini_resp = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        { "thought": true, "text": "I should say hello" },
                        { "text": "Hello world!" }
                    ]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "thoughtsTokenCount": 12,
                "totalTokenCount": 27
            }
        });
        let openai_resp = gemini_json_to_openai(&gemini_resp, "gemini-3.8-flash-high", 1000);
        let choice = &openai_resp["choices"][0]["message"];
        assert_eq!(choice["content"], "Hello world!");
        assert_eq!(choice["reasoning_content"], "I should say hello");
        assert_eq!(
            openai_resp["usage"]["completion_tokens_details"]["reasoning_tokens"],
            12
        );
    }

    #[test]
    fn plain_call_id_in_tool_response_matches_signed_assistant_call() {
        let signed_id = embed_signature_in_call_id("call_lookup_0", "SIG999");
        let body = json!({
            "model": "gemini-3.8-flash-high",
            "messages": [
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": signed_id,
                        "type": "function",
                        "function": { "name": "lookup", "arguments": "{}" }
                    }]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_lookup_0", // Client stripped the #SIG999 suffix
                    "content": "{\"result\":\"ok\"}"
                }
            ]
        });
        let req = openai_to_gemini(&body)
            .expect("translate must succeed even if client stripped signature suffix");
        let contents = req["contents"].as_array().expect("contents");
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0]["role"], "model");
        assert_eq!(contents[0]["parts"][0]["thoughtSignature"], "SIG999");
        assert_eq!(contents[1]["role"], "user");
        assert_eq!(
            contents[1]["parts"][0]["functionResponse"]["name"],
            "lookup"
        );
    }

    #[test]
    fn split_stop_and_trailing_usage_retains_usage_at_eof() {
        let mut decoder = GeminiDecoder::new();
        let mut events = Vec::new();
        decoder.decode(
            br#"{"candidates":[{"content":{"parts":[{"text":"hello"}]},"finishReason":"STOP"}]}"#,
            &mut events,
        );
        // Completed must not be emitted on STOP alone; delayed until decoder EOF.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, CodexEvent::Completed { .. })),
            "Completed should be delayed until decoder finish(): {events:?}"
        );
        assert_eq!(events, vec![CodexEvent::TextDelta("hello".to_string())]);

        // Trailing usage frame arrives after STOP
        decoder.decode(
            br#"{"usageMetadata":{"promptTokenCount":700,"candidatesTokenCount":60,"totalTokenCount":760,"thoughtsTokenCount":16,"cachedContentTokenCount":0}}"#,
            &mut events,
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, CodexEvent::Completed { .. })),
            "Completed must still wait for EOF finish(): {events:?}"
        );

        // Decoder EOF triggers final Completed with retained trailing usage
        decoder.finish(&mut events);
        let completed_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, CodexEvent::Completed { .. }))
            .collect();
        assert_eq!(
            completed_events.len(),
            1,
            "exactly one Completed event must be emitted"
        );
        match completed_events[0] {
            CodexEvent::Completed { usage: Some(ref u) } => {
                assert_eq!(u.prompt_tokens, 700);
                assert_eq!(u.completion_tokens, 60);
                assert_eq!(u.total_tokens, 760);
                assert_eq!(u.reasoning_tokens, 16);
                assert_eq!(u.cached_tokens, 0);
                assert!(u.cached_tokens_known);
            }
            other => panic!("expected Completed with usage, got {other:?}"),
        }
    }

    #[test]
    fn truncated_stream_without_finish_reason_fails_at_eof() {
        let mut decoder = GeminiDecoder::new();
        let mut events = Vec::new();
        decoder.decode(
            br#"{"candidates":[{"content":{"parts":[{"text":"partial cut"}]}}]}"#,
            &mut events,
        );
        assert_eq!(
            events,
            vec![CodexEvent::TextDelta("partial cut".to_string())]
        );

        // Stream EOF without finishReason
        decoder.finish(&mut events);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, CodexEvent::Failed { .. })),
            "truncated stream without finishReason must emit Failed at EOF: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, CodexEvent::Completed { .. })),
            "truncated stream must not emit Completed: {events:?}"
        );
    }
}
