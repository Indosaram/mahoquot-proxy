//! Direct-format Kimi requests and caller-owned, bounded signed-thinking replay.
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Claude,
    Responses,
    OpenAi,
}

impl Format {
    pub const fn path(self) -> &'static str {
        match self {
            Self::Claude => "messages",
            Self::Responses => "responses",
            Self::OpenAi => "chat/completions",
        }
    }
}

pub fn upstream_model(model: &str) -> String {
    let model = model.trim();
    let (base, suffix) = match model.rsplit_once('(') {
        Some((base, suffix)) if suffix.ends_with(')') => (base, format!("({suffix}")),
        _ => (model, String::new()),
    };
    let base = base.to_ascii_lowercase();
    let base = base.strip_suffix("[1m]").unwrap_or(&base);
    let base = base.strip_prefix("kimi-").unwrap_or(base);
    let normalized = match base {
        "k2.8" | "k2.8-code" | "k2.8-preview" | "k2.7-code" | "for-coding" => "kimi-for-coding",
        "k2.7-code-highspeed" | "for-coding-highspeed" => "kimi-for-coding-highspeed",
        other => other,
    };
    format!("{normalized}{suffix}")
}

pub fn prepare_request(format: Format, body: &Value, model: &str) -> Result<Value, String> {
    let mut body = body.clone();
    let object = body
        .as_object_mut()
        .ok_or("Kimi request must be an object")?;
    object.insert("model".into(), json!(upstream_model(model)));
    match format {
        Format::Claude => {}
        Format::Responses => normalize_responses_input(&mut body),
        Format::OpenAi => {
            normalize_chat_links(&mut body);
            let required = if body
                .pointer("/thinking/type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.eq_ignore_ascii_case("disabled"))
            {
                0.6
            } else {
                1.0
            };
            if body.get("temperature").is_some_and(|temperature| {
                temperature
                    .as_f64()
                    .is_none_or(|value| (value - required).abs() > f64::EPSILON)
            }) {
                if let Some(object) = body.as_object_mut() {
                    object.remove("temperature");
                }
            }
            for key in ["tools", "functions"] {
                if let Some(tools) = body.get_mut(key).and_then(Value::as_array_mut) {
                    for tool in tools {
                        let schema = if tool.get("function").is_some() {
                            tool.pointer_mut("/function/parameters")
                        } else {
                            tool.get_mut("parameters")
                        };
                        if let Some(schema) = schema {
                            let root = schema.clone();
                            inline_refs(schema, &root, &mut Vec::new(), &mut 8192, 0)?;
                            if let Some(object) = schema.as_object_mut() {
                                object.remove("$defs");
                                object.remove("definitions");
                                object.entry("type").or_insert(json!("object"));
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(body)
}

fn normalize_chat_links(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    messages.retain(|message| {
        message["role"] != "assistant"
            || message
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| !calls.is_empty())
            || message
                .get("function_call")
                .is_some_and(|call| !call.is_null())
            || message
                .get("reasoning_content")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty())
            || message.get("content").is_some_and(|content| match content {
                Value::String(text) => !text.trim().is_empty(),
                Value::Array(parts) => parts.iter().any(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|s| !s.trim().is_empty())
                        || part
                            .get("type")
                            .and_then(Value::as_str)
                            .is_some_and(|kind| kind != "text")
                }),
                Value::Null => false,
                _ => true,
            })
    });
    let mut pending = Vec::<String>::new();
    let mut latest = String::new();
    for message in messages {
        match message.get("role").and_then(Value::as_str) {
            Some("assistant") => {
                let reasoning = message
                    .get("reasoning_content")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !reasoning.trim().is_empty() && reasoning.trim() != "[reasoning unavailable]" {
                    latest = reasoning.to_string();
                }
                let calls = message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if !calls.is_empty() {
                    if reasoning.trim().is_empty() || reasoning.trim() == "[reasoning unavailable]"
                    {
                        let content = &message["content"];
                        let fallback = if !latest.is_empty() {
                            latest.clone()
                        } else if let Some(text) =
                            content.as_str().filter(|text| !text.trim().is_empty())
                        {
                            text.trim().to_string()
                        } else if let Some(parts) = content.as_array() {
                            parts
                                .iter()
                                .filter_map(|part| part.get("text").and_then(Value::as_str))
                                .filter(|text| !text.trim().is_empty())
                                .collect::<Vec<_>>()
                                .join("\n")
                        } else {
                            String::new()
                        };
                        message["reasoning_content"] = json!(if fallback.is_empty() {
                            "[reasoning unavailable]"
                        } else {
                            &fallback
                        });
                    }
                    pending.extend(
                        calls
                            .iter()
                            .filter_map(|call| call.get("id").and_then(Value::as_str))
                            .filter(|id| !id.trim().is_empty())
                            .map(str::to_string),
                    );
                }
            }
            Some("tool") => {
                let id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                    .or_else(|| {
                        message
                            .get("call_id")
                            .and_then(Value::as_str)
                            .filter(|id| !id.trim().is_empty())
                    })
                    .map(str::to_string)
                    .or_else(|| (pending.len() == 1).then(|| pending[0].clone()));
                if let Some(id) = id {
                    message["tool_call_id"] = json!(id);
                    if let Some(index) = pending.iter().position(|pending| pending == &id) {
                        pending.remove(index);
                    }
                }
            }
            _ => {}
        }
    }
}

fn inline_refs(
    value: &mut Value,
    root: &Value,
    visiting: &mut Vec<String>,
    budget: &mut usize,
    depth: usize,
) -> Result<(), String> {
    if depth > 64 || *budget == 0 {
        return Err("Kimi tool schema expansion limit exceeded".into());
    }
    *budget -= 1;
    if let Some(reference) = value
        .get("$ref")
        .and_then(Value::as_str)
        .map(str::to_string)
    {
        if !reference.starts_with('#') || visiting.contains(&reference) {
            return Err("Kimi tool schema contains unsupported or cyclic reference".into());
        }
        let mut target = root
            .pointer(&reference[1..])
            .cloned()
            .ok_or("Kimi tool schema has unresolved reference")?;
        visiting.push(reference);
        inline_refs(&mut target, root, visiting, budget, depth + 1)?;
        visiting.pop();
        if let (Some(source), Some(target)) = (value.as_object(), target.as_object_mut()) {
            for (key, value) in source {
                if key != "$ref" {
                    target.insert(key.clone(), value.clone());
                }
            }
        }
        *value = target;
    }
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if key != "$defs" && key != "definitions" {
                    inline_refs(value, root, visiting, budget, depth + 1)?;
                }
            }
        }
        Value::Array(array) => {
            for value in array {
                inline_refs(value, root, visiting, budget, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn is_call(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("function_call" | "custom_tool_call")
    )
}

fn is_output(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("function_call_output" | "custom_tool_call_output")
    )
}

fn call_id(item: &Value) -> &str {
    item.get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn normalize_responses_input(body: &mut Value) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    let mut index = 0;
    while index < items.len() {
        if !is_call(&items[index]) {
            index += 1;
            continue;
        }
        let mut end = index;
        let mut needed = BTreeMap::<String, usize>::new();
        while end < items.len() && is_call(&items[end]) {
            let id = call_id(&items[end]);
            if !id.is_empty() {
                *needed.entry(id.to_string()).or_default() += 1;
            }
            end += 1;
        }
        let mut remaining: usize = needed.values().sum();
        let mut matched = Vec::new();
        let mut cursor = end;
        while cursor < items.len() && remaining > 0 && !is_call(&items[cursor]) {
            if is_output(&items[cursor]) {
                if let Some(count) = needed.get_mut(call_id(&items[cursor])) {
                    if *count > 0 {
                        *count -= 1;
                        remaining -= 1;
                        matched.push(cursor);
                    }
                }
            }
            cursor += 1;
        }
        if remaining == 0 && !matched.is_empty() {
            let segment: Vec<Value> = items[end..cursor].to_vec();
            let reordered = matched
                .iter()
                .map(|position| items[*position].clone())
                .chain(
                    segment
                        .into_iter()
                        .enumerate()
                        .filter_map(|(offset, item)| {
                            (!matched.contains(&(end + offset))).then_some(item)
                        }),
                )
                .collect::<Vec<_>>();
            items.splice(end..cursor, reordered);
            index = cursor;
        } else {
            index = end;
        }
    }
}

pub const REPLAY_TTL_MS: u64 = 3_600_000;
pub const REPLAY_MAX_ENTRIES: usize = 10_240;
pub const REPLAY_MAX_ENTRY_BYTES: usize = 8 * 1024 * 1024;
pub const REPLAY_MAX_TOTAL_BYTES: usize = 256 * 1024 * 1024;
pub const REPLAY_MAX_BLOCKS: usize = 512;

/// The principal, account, session and model must all match. Routing IDs remain local.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReplayScope {
    principal: String,
    account: String,
    session: String,
    family: String,
}

impl ReplayScope {
    pub fn new(principal: &str, account: &str, session: &str, model: &str) -> Option<Self> {
        if [principal, account, session, model]
            .iter()
            .any(|value| value.trim().is_empty())
        {
            return None;
        }
        let normalized = upstream_model(model);
        let base = normalized.split('(').next().unwrap_or(&normalized);
        let family = match base {
            "k3" | "k3-256k" => "k3",
            other => other,
        }
        .to_string();
        Some(Self {
            principal: principal.into(),
            account: account.into(),
            session: session.into(),
            family,
        })
    }
}

struct Entry {
    generation: u64,
    touched: u64,
    content: Option<Value>,
    bytes: usize,
}

/// Opaque generation prevents old completions or failures overwriting newer turns.
pub struct ReplaySnapshot {
    scope: ReplayScope,
    generation: u64,
    pub applied: bool,
}

#[derive(Default)]
pub struct ReplayCache {
    entries: BTreeMap<ReplayScope, Entry>,
    total_bytes: usize,
    generation: u64,
}

impl ReplayCache {
    pub fn prepare(&mut self, scope: ReplayScope, body: &mut Value, now_ms: u64) -> ReplaySnapshot {
        self.expire(now_ms);
        if !self.entries.contains_key(&scope) {
            while self.entries.len() >= REPLAY_MAX_ENTRIES {
                self.evict_oldest();
            }
            self.generation = self.generation.wrapping_add(1);
            self.entries.insert(
                scope.clone(),
                Entry {
                    generation: self.generation,
                    touched: now_ms,
                    content: None,
                    bytes: 0,
                },
            );
        }
        let mut snapshot = ReplaySnapshot {
            scope: scope.clone(),
            generation: self.generation,
            applied: false,
        };
        if let Some(entry) = self.entries.get_mut(&scope) {
            entry.touched = now_ms;
            snapshot.generation = entry.generation;
            if let Some(content) = &entry.content {
                snapshot.applied = restore_content(body, content);
            }
        }
        snapshot
    }

    pub fn complete(&mut self, snapshot: &ReplaySnapshot, response: &Value, now_ms: u64) -> bool {
        let content = response
            .get("content")
            .filter(|content| replayable(content));
        self.replace(snapshot, content, now_ms)
    }

    pub fn failed(&mut self, snapshot: &ReplaySnapshot, status: u16, now_ms: u64) -> bool {
        if snapshot.applied && matches!(status, 400 | 422) {
            self.replace(snapshot, None, now_ms)
        } else {
            false
        }
    }

    fn replace(&mut self, snapshot: &ReplaySnapshot, content: Option<&Value>, now_ms: u64) -> bool {
        self.expire(now_ms);
        if self
            .entries
            .get(&snapshot.scope)
            .is_none_or(|entry| entry.generation != snapshot.generation)
        {
            return false;
        }
        let bytes = content
            .and_then(|content| serde_json::to_vec(content).ok())
            .map_or(0, |bytes| bytes.len());
        let content = if bytes <= REPLAY_MAX_ENTRY_BYTES {
            content
        } else {
            None
        };
        let bytes = if content.is_some() { bytes } else { 0 };
        if let Some(entry) = self.entries.get_mut(&snapshot.scope) {
            self.total_bytes -= entry.bytes;
            self.generation = self.generation.wrapping_add(1);
            *entry = Entry {
                generation: self.generation,
                touched: now_ms,
                content: content.cloned(),
                bytes,
            };
            self.total_bytes += bytes;
        }
        while self.total_bytes > REPLAY_MAX_TOTAL_BYTES {
            self.evict_oldest();
        }
        true
    }

    fn expire(&mut self, now_ms: u64) {
        self.entries.retain(|_, entry| {
            let keep = now_ms.saturating_sub(entry.touched) < REPLAY_TTL_MS;
            if !keep {
                self.total_bytes -= entry.bytes;
            }
            keep
        });
    }

    fn evict_oldest(&mut self) {
        let key = self
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.touched)
            .map(|(key, _)| key.clone());
        if let Some(key) = key {
            if let Some(entry) = self.entries.remove(&key) {
                self.total_bytes -= entry.bytes;
            }
        }
    }
}

fn replayable(content: &Value) -> bool {
    let Some(parts) = content
        .as_array()
        .filter(|parts| parts.len() <= REPLAY_MAX_BLOCKS)
    else {
        return false;
    };
    parts.iter().any(|part| {
        part["type"] == "thinking"
            && part
                .get("signature")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty())
    }) && parts.iter().any(|part| {
        part["type"] == "tool_use"
            && part
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty())
    })
}

fn non_thinking(content: &Value) -> Option<Vec<&Value>> {
    let parts = content.as_array()?;
    if !parts.iter().any(|part| part["type"] == "tool_use") {
        return None;
    }
    if parts.iter().any(|part| {
        part["type"] == "tool_use"
            && part
                .get("id")
                .and_then(Value::as_str)
                .is_none_or(|id| id.trim().is_empty())
    }) {
        return None;
    }
    Some(
        parts
            .iter()
            .filter(|part| {
                !matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("thinking" | "redacted_thinking")
                )
            })
            .collect(),
    )
}

fn restore_content(body: &mut Value, cached: &Value) -> bool {
    let Some(cached_parts) = non_thinking(cached) else {
        return false;
    };
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return false;
    };
    for message in messages.iter_mut().rev() {
        if message["role"] != "assistant" {
            continue;
        }
        let Some(content) = message.get("content") else {
            continue;
        };
        if content == cached {
            return false;
        }
        if content.as_array().is_some_and(|parts| {
            parts.iter().any(|part| {
                matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("thinking" | "redacted_thinking")
                )
            })
        }) {
            continue;
        }
        if non_thinking(content).is_some_and(|parts| parts == cached_parts) {
            message["content"] = cached.clone();
            return true;
        }
    }
    false
}

/// Observes Claude SSE without rewriting it. Only message_stop commits content;
/// truncation, parser errors and aborted streams must never publish partial replay.
#[derive(Default)]
pub struct ReplayCollector {
    parser: super::events::SseParser,
    blocks: BTreeMap<u64, Value>,
    open_blocks: BTreeSet<u64>,
    bytes: usize,
    completed: bool,
    invalid: bool,
}

impl ReplayCollector {
    pub fn push(&mut self, chunk: &[u8]) {
        if self.invalid || self.completed {
            return;
        }
        let mut frames = Vec::new();
        if self.parser.push_raw_data(chunk, &mut frames).is_err() {
            self.invalid = true;
            return;
        }
        for data in frames {
            let Ok(event) = serde_json::from_slice::<Value>(&data) else {
                self.invalid = true;
                return;
            };
            self.observe(&event);
            if self.invalid || self.completed {
                return;
            }
        }
    }

    fn observe(&mut self, event: &Value) {
        match event.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                let Some(index) = event.get("index").and_then(Value::as_u64) else {
                    self.invalid = true;
                    return;
                };
                if self.blocks.len() >= REPLAY_MAX_BLOCKS || self.blocks.contains_key(&index) {
                    self.invalid = true;
                    return;
                }
                let block = event["content_block"].clone();
                if !block.is_object() {
                    self.invalid = true;
                    return;
                }
                self.bytes += block.to_string().len();
                self.open_blocks.insert(index);
                self.blocks.insert(index, block);
            }
            Some("content_block_delta") => {
                let Some(index) = event.get("index").and_then(Value::as_u64) else {
                    self.invalid = true;
                    return;
                };
                if !self.open_blocks.contains(&index) {
                    self.invalid = true;
                    return;
                }
                let Some(block) = self.blocks.get_mut(&index) else {
                    self.invalid = true;
                    return;
                };
                let delta = &event["delta"];
                let mapping = match delta.get("type").and_then(Value::as_str) {
                    Some("thinking_delta") => Some(("thinking", "thinking")),
                    Some("signature_delta") => Some(("signature", "signature")),
                    Some("text_delta") => Some(("text", "text")),
                    Some("input_json_delta") => Some(("input", "partial_json")),
                    _ => None,
                };
                if let Some((target, source)) = mapping {
                    let Some(text) = delta.get(source).and_then(Value::as_str) else {
                        self.invalid = true;
                        return;
                    };
                    let mut joined = block
                        .get(target)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    joined.push_str(text);
                    block[target] = json!(joined);
                    self.bytes += text.len();
                }
            }
            Some("content_block_stop") => {
                let Some(index) = event.get("index").and_then(Value::as_u64) else {
                    self.invalid = true;
                    return;
                };
                if !self.open_blocks.remove(&index) {
                    self.invalid = true;
                    return;
                }
                if let Some(block) = self.blocks.get_mut(&index) {
                    if block["type"] == "tool_use" {
                        if let Some(raw) = block["input"].as_str() {
                            let input = if raw.is_empty() {
                                Ok(json!({}))
                            } else {
                                serde_json::from_str::<Value>(raw)
                            };
                            match input {
                                Ok(input) => block["input"] = input,
                                Err(_) => self.invalid = true,
                            }
                        }
                    }
                } else {
                    self.invalid = true;
                }
            }
            Some("message_stop") => {
                if !self.open_blocks.is_empty() {
                    self.invalid = true;
                }
                self.completed = true;
            }
            Some("error") => self.invalid = true,
            _ => {}
        }
        if self.bytes > REPLAY_MAX_ENTRY_BYTES {
            self.invalid = true;
            self.blocks.clear();
        }
    }

    pub fn finish(self) -> Option<Value> {
        if !self.completed || self.invalid {
            return None;
        }
        Some(json!({"content": self.blocks.into_values().collect::<Vec<_>>()}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scope(session: &str) -> ReplayScope {
        ReplayScope::new("principal", "account", session, "kimi-k3").unwrap()
    }
    fn response() -> Value {
        json!({"content":[{"type":"thinking","thinking":"private", "signature":"signed"}, {"type":"tool_use","id":"call", "name":"lookup", "input":{"x":1}}]})
    }
    fn request() -> Value {
        json!({"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"call", "name":"lookup", "input":{"x":1}}]}, {"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":"ok"}]}]})
    }

    #[test]
    fn direct_formats_preserve_fields_when_preparing_model() {
        // Given
        let original = json!({"input":[{"type":"reasoning","encrypted_content":"opaque"}],"tools":[{"type":"custom","name":"apply_patch"}],"stream":true});
        // When
        let prepared = prepare_request(Format::Responses, &original, "kimi-k3[1m](1024)").unwrap();
        // Then
        assert_eq!(prepared["input"], original["input"]);
        assert_eq!(prepared["tools"], original["tools"]);
        assert_eq!(prepared["model"], "k3(1024)");
        assert_eq!(Format::Claude.path(), "messages");
    }

    #[test]
    fn responses_outputs_are_contiguous_when_parallel_calls_have_intervening_messages() {
        // Given
        let body = json!({"input":[{"type":"function_call","call_id":"a"},{"type":"custom_tool_call","call_id":"b"},{"role":"user","content":"defer"},{"type":"function_call_output","call_id":"a"},{"type":"custom_tool_call_output","call_id":"b"}]});
        // When
        let prepared = prepare_request(Format::Responses, &body, "remote").unwrap();
        // Then
        assert_eq!(prepared["input"][2]["call_id"], "a");
        assert_eq!(prepared["input"][3]["call_id"], "b");
        assert_eq!(prepared["input"][4]["role"], "user");
    }

    #[test]
    fn replay_restores_signed_blocks_when_matching_tool_history_returns() {
        // Given
        let mut cache = ReplayCache::default();
        let first = cache.prepare(scope("s"), &mut request(), 0);
        cache.complete(&first, &response(), 1);
        let mut body = request();
        // When
        let snapshot = cache.prepare(scope("s"), &mut body, 2);
        // Then
        assert!(snapshot.applied);
        assert_eq!(body["messages"][0]["content"], response()["content"]);
    }

    #[test]
    fn replay_is_isolated_when_any_scope_component_changes() {
        // Given
        let mut cache = ReplayCache::default();
        let first = cache.prepare(scope("s"), &mut request(), 0);
        cache.complete(&first, &response(), 1);
        // When / Then
        for other in [
            scope("other"),
            ReplayScope::new("other", "account", "s", "k3").unwrap(),
            ReplayScope::new("principal", "other", "s", "k3").unwrap(),
            ReplayScope::new("principal", "account", "s", "different").unwrap(),
        ] {
            assert!(!cache.prepare(other, &mut request(), 2).applied);
        }
    }

    #[test]
    fn stale_failure_cannot_clear_when_newer_completion_changed_generation() {
        // Given
        let mut cache = ReplayCache::default();
        let first = cache.prepare(scope("s"), &mut request(), 0);
        cache.complete(&first, &response(), 1);
        let stale = cache.prepare(scope("s"), &mut request(), 2);
        let newer = cache.prepare(scope("s"), &mut request(), 2);
        cache.complete(&newer, &response(), 3);
        // When
        let cleared = cache.failed(&stale, 422, 4);
        // Then
        assert!(!cleared);
        assert!(cache.prepare(scope("s"), &mut request(), 5).applied);
    }

    #[test]
    fn replay_expires_when_injected_clock_reaches_ttl() {
        // Given
        let mut cache = ReplayCache::default();
        let first = cache.prepare(scope("s"), &mut request(), 0);
        cache.complete(&first, &response(), 1);
        // When
        let next = cache.prepare(scope("s"), &mut request(), REPLAY_TTL_MS + 1);
        // Then
        assert!(!next.applied);
    }

    #[test]
    fn replay_rejects_mismatched_content_when_tool_arguments_changed() {
        // Given
        let mut cache = ReplayCache::default();
        let first = cache.prepare(scope("s"), &mut request(), 0);
        cache.complete(&first, &response(), 1);
        let mut body = request();
        body["messages"][0]["content"][0]["input"]["x"] = json!(2);
        // When
        let snapshot = cache.prepare(scope("s"), &mut body, 2);
        // Then
        assert!(!snapshot.applied);
    }

    #[test]
    fn collector_does_not_publish_when_stream_is_truncated() {
        // Given
        let mut collector = ReplayCollector::default();
        // When
        collector.push(b"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"secret\"}}\n\n");
        // Then
        assert!(collector.finish().is_none());
    }

    #[test]
    fn collector_keeps_signature_and_tool_input_when_chunks_split_every_byte() {
        // Given
        let events = [
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"private"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signed"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call","name":"lookup","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"x\":1}"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_stop"}),
        ];
        let stream = events
            .iter()
            .map(|event| format!("data: {event}\r\n\r\n"))
            .collect::<String>();
        let mut collector = ReplayCollector::default();
        // When
        for byte in stream.bytes() {
            collector.push(&[byte]);
        }
        // Then
        assert_eq!(collector.finish().unwrap(), response());
    }

    #[test]
    fn schema_and_temperature_are_normalized_when_chat_has_local_refs() {
        let body = json!({"temperature":0.2,"tools":[{"type":"function","function":{"name":"f","parameters":{"$defs":{"x":{"type":"string"}},"properties":{"x":{"$ref":"#/$defs/x"}}}}}]});
        let prepared = prepare_request(Format::OpenAi, &body, "remote-model").unwrap();
        assert!(prepared.get("temperature").is_none());
        assert_eq!(
            prepared
                .pointer("/tools/0/function/parameters/properties/x/type")
                .unwrap(),
            "string"
        );
        assert_eq!(
            prepared
                .pointer("/tools/0/function/parameters/type")
                .unwrap(),
            "object"
        );
        assert!(prepared
            .pointer("/tools/0/function/parameters/$defs")
            .is_none());
    }

    #[test]
    fn cyclic_schema_is_rejected_when_local_ref_recurses() {
        let body = json!({"tools":[{"function":{"parameters":{"$defs":{"x":{"$ref":"#/$defs/x"}},"properties":{"x":{"$ref":"#/$defs/x"}}}}}]});
        assert!(prepare_request(Format::OpenAi, &body, "remote").is_err());
    }

    #[test]
    fn rejected_replay_is_cleared_when_upstream_returns_validation_error() {
        let mut cache = ReplayCache::default();
        let first = cache.prepare(scope("s"), &mut request(), 0);
        cache.complete(&first, &response(), 1);
        let applied = cache.prepare(scope("s"), &mut request(), 2);
        assert!(cache.failed(&applied, 400, 3));
        assert!(!cache.prepare(scope("s"), &mut request(), 4).applied);
    }

    #[test]
    fn operational_error_keeps_replay_when_upstream_is_unavailable() {
        let mut cache = ReplayCache::default();
        let first = cache.prepare(scope("s"), &mut request(), 0);
        cache.complete(&first, &response(), 1);
        let applied = cache.prepare(scope("s"), &mut request(), 2);
        assert!(!cache.failed(&applied, 429, 3));
        assert!(cache.prepare(scope("s"), &mut request(), 4).applied);
    }

    #[test]
    fn oversized_content_is_not_cached_when_block_bound_is_exceeded() {
        let mut cache = ReplayCache::default();
        let first = cache.prepare(scope("s"), &mut request(), 0);
        let mut oversized = response();
        let content = oversized["content"].as_array_mut().unwrap();
        content.extend((0..REPLAY_MAX_BLOCKS).map(|_| json!({"type":"text","text":"x"})));
        cache.complete(&first, &oversized, 1);
        assert!(!cache.prepare(scope("s"), &mut request(), 2).applied);
    }

    #[test]
    fn tool_link_is_inferred_when_one_pending_call_exists() {
        let body = json!({"messages":[{"role":"assistant","content":null,"tool_calls":[{"id":"call","type":"function","function":{"name":"f","arguments":"{}"}}]}, {"role":"tool","content":"ok"}]});
        let prepared = prepare_request(Format::OpenAi, &body, "remote").unwrap();
        assert_eq!(prepared["messages"][1]["tool_call_id"], "call");
        assert_eq!(
            prepared["messages"][0]["reasoning_content"],
            "[reasoning unavailable]"
        );
    }
}
