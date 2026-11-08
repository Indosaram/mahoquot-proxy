use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const REPLAY_TTL: Duration = Duration::from_secs(3600);
const MAX_ENTRIES: usize = 10240;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReplayScope {
    caller: [u8; 32],
    account: String,
    model: String,
    session: [u8; 32],
}

impl ReplayScope {
    pub fn new(caller: &str, account: &str, model: &str, session: &str) -> Option<Self> {
        if [caller, account, model, session]
            .iter()
            .any(|part| part.trim().is_empty())
        {
            return None;
        }
        Some(Self {
            caller: Sha256::digest(caller.as_bytes()).into(),
            account: account.to_owned(),
            model: model.to_owned(),
            session: Sha256::digest(session.as_bytes()).into(),
        })
    }

    pub fn conversation_id(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(self.caller);
        for part in [&self.account, &self.model] {
            hash.update(part.len().to_be_bytes());
            hash.update(part.as_bytes());
        }
        hash.update(self.session);
        format!("xai-{:x}", hash.finalize())
    }
}

struct Entry {
    at: Instant,
    items: Vec<Value>,
}

#[derive(Default)]
pub struct ReasoningReplay {
    entries: BTreeMap<ReplayScope, Entry>,
}

impl ReasoningReplay {
    pub fn store_completed(&mut self, scope: ReplayScope, response: &Value, now: Instant) {
        let response = response.get("response").unwrap_or(response);
        if response.get("status").and_then(Value::as_str) != Some("completed") {
            return;
        }
        let Some(output) = response.get("output").and_then(Value::as_array) else {
            return;
        };
        let items: Vec<_> = output.iter().filter_map(normalize_replay_item).collect();
        if !items.iter().any(|item| {
            matches!(
                item["type"].as_str(),
                Some("reasoning" | "function_call" | "custom_tool_call")
            )
        }) {
            self.entries.remove(&scope);
            return;
        }
        self.entries
            .retain(|_, entry| now.saturating_duration_since(entry.at) < REPLAY_TTL);
        if !self.entries.contains_key(&scope) && self.entries.len() >= MAX_ENTRIES {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.at)
                .map(|(key, _)| key.clone());
            if let Some(key) = oldest {
                self.entries.remove(&key);
            }
        }
        self.entries.insert(scope, Entry { at: now, items });
    }

    pub fn apply(&mut self, scope: &ReplayScope, body: &mut Value, websocket: bool, now: Instant) {
        if websocket
            && body
                .get("previous_response_id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty())
        {
            return;
        }
        if self
            .entries
            .get(scope)
            .is_some_and(|entry| now.saturating_duration_since(entry.at) >= REPLAY_TTL)
        {
            self.entries.remove(scope);
        }
        let Some(entry) = self.entries.get(scope) else {
            return;
        };
        let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
            return;
        };
        let assistant = input.iter().rev().find(|item| {
            item["role"] == "assistant" && matches!(item["type"].as_str(), None | Some("message"))
        });
        let cached_assistant = entry.items.iter().find(|item| item["role"] == "assistant");
        if let (Some(actual), Some(cached)) = (assistant, cached_assistant) {
            if assistant_parts(actual) != assistant_parts(cached) {
                return;
            }
        }
        let mut replay = Vec::new();
        for item in &entry.items {
            let kind = item["type"].as_str().unwrap_or("");
            let already_present = match kind {
                "reasoning" => input.iter().any(|existing| {
                    existing["type"] == "reasoning"
                        && existing["encrypted_content"] == item["encrypted_content"]
                }),
                "message" => assistant.is_some(),
                "function_call" | "custom_tool_call" => {
                    let call = &item["call_id"];
                    if !input.iter().any(|existing| {
                        matches!(
                            existing["type"].as_str(),
                            Some("function_call_output" | "custom_tool_call_output")
                        ) && existing["call_id"] == *call
                    }) {
                        continue;
                    }
                    input.iter().any(|existing| {
                        matches!(
                            existing["type"].as_str(),
                            Some("function_call" | "custom_tool_call")
                        ) && existing["call_id"] == *call
                    })
                }
                _ => true,
            };
            if !already_present {
                replay.push(item.clone());
            }
        }
        let position = input
            .iter()
            .position(|item| {
                item["role"] == "assistant"
                    || matches!(
                        item["type"].as_str(),
                        Some("function_call_output" | "custom_tool_call_output")
                    )
            })
            .unwrap_or_else(|| {
                input
                    .iter()
                    .rposition(|item| item["role"] == "user")
                    .unwrap_or(input.len())
            });
        input.splice(position..position, replay);
    }
}

fn assistant_parts(item: &Value) -> Option<Vec<Value>> {
    if let Some(text) = item["content"].as_str() {
        return Some(vec![json!({"type":"output_text","text":text})]);
    }
    item["content"].as_array().map(|parts| {
        parts
            .iter()
            .filter_map(|part| match part["type"].as_str() {
                Some("output_text") => part["text"]
                    .as_str()
                    .map(|text| json!({"type":"output_text","text":text})),
                Some("refusal") => part["refusal"]
                    .as_str()
                    .map(|text| json!({"type":"refusal","refusal":text})),
                _ => None,
            })
            .collect()
    })
}

fn normalize_replay_item(item: &Value) -> Option<Value> {
    match item["type"].as_str()? {
        "reasoning" => {
            let encrypted = item["encrypted_content"].as_str()?;
            valid_encrypted_content(encrypted).then(|| json!({"type":"reasoning","summary":[],"content":null,"encrypted_content":encrypted}))
        }
        "message" if item["role"] == "assistant" => {
            let parts = assistant_parts(item)?;
            (!parts.is_empty())
                .then(|| json!({"type":"message","role":"assistant","content":parts}))
        }
        "function_call" | "custom_tool_call" => {
            let kind = item["type"].as_str()?;
            let call = item["call_id"]
                .as_str()
                .filter(|id| !id.trim().is_empty())?;
            let name = item["name"]
                .as_str()
                .filter(|name| !name.trim().is_empty())?;
            let field = if kind == "function_call" {
                "arguments"
            } else {
                "input"
            };
            let payload = item[field].as_str()?;
            let mut result = json!({"type":kind,"call_id":call,"name":name});
            result[field] = Value::String(payload.to_owned());
            Some(result)
        }
        _ => None,
    }
}

/// Shape validation is not provider identification or proof of decryptability.
pub fn valid_encrypted_content(raw: &str) -> bool {
    if raw.len() > 8 * 1024 * 1024
        || raw.trim() != raw
        || raw.contains('=')
        || raw.starts_with("gAAAA")
    {
        return false;
    }
    let Ok(bytes) = STANDARD_NO_PAD.decode(raw) else {
        return false;
    };
    if bytes.len() < 32 || matches!(raw.len(), 4340 | 12946) {
        return false;
    }
    let mut counts = [0_u32; 256];
    for byte in &bytes {
        counts[usize::from(*byte)] += 1;
    }
    let length = f64::from(u32::try_from(bytes.len()).unwrap_or(u32::MAX));
    let entropy: f64 = counts
        .iter()
        .filter(|count| **count > 0)
        .map(|count| {
            let probability = f64::from(*count) / length;
            -probability * probability.log2()
        })
        .sum();
    entropy / length.min(256.0).log2() >= 0.85
}

fn object_only(value: &Value) -> bool {
    match value {
        Value::String(value) => value.trim().eq_ignore_ascii_case("object"),
        Value::Array(values) => !values.is_empty() && values.iter().all(object_only),
        _ => false,
    }
}

fn repair_tool(tool: &mut Value, namespace: &str) {
    if tool["type"] == "namespace" {
        let name = tool["name"].as_str().unwrap_or("").to_owned();
        if let Some(tools) = tool["tools"].as_array_mut() {
            for tool in tools {
                repair_tool(tool, &name);
            }
        }
        return;
    }
    let kind = tool["type"].as_str().unwrap_or("");
    if !matches!(kind, "function" | "custom") {
        return;
    }
    let name = tool["name"]
        .as_str()
        .unwrap_or("")
        .trim()
        .trim_start_matches("mcp__")
        .to_ascii_lowercase();
    let namespace = namespace
        .trim()
        .trim_start_matches("mcp__")
        .to_ascii_lowercase();
    let automation = kind == "function"
        && ((name == "automation_update"
            && matches!(namespace.as_str(), "codex_app" | "codex_apps"))
            || matches!(
                name.as_str(),
                "codex_app__automation_update" | "codex_apps__automation_update"
            ));
    let incompatible = ["anyOf", "oneOf"].iter().any(|union| {
        tool["parameters"][union]
            .as_array()
            .is_some_and(|branches| {
                branches
                    .iter()
                    .any(|branch| branch.get("$ref").is_some() || !object_only(&branch["type"]))
            })
    });
    if automation || incompatible {
        tool["parameters"] = json!({"type":"object","properties":{},"additionalProperties":true});
        if let Some(object) = tool.as_object_mut() {
            object.remove("strict");
        }
    }
}

/// Invoke before configured payload rules; those rules remain the final semantic barrier.
pub fn normalize_request(body: &mut Value) {
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            repair_tool(tool, "");
        }
    }
    if let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) {
        input.retain_mut(|item| {
            if item["type"] == "additional_tools" {
                if let Some(tools) = item["tools"].as_array_mut() {
                    for tool in tools {
                        repair_tool(tool, "");
                    }
                }
            }
            if matches!(item["type"].as_str(), Some("reasoning" | "compaction"))
                && item.get("encrypted_content").is_some()
                && !item["encrypted_content"]
                    .as_str()
                    .is_some_and(valid_encrypted_content)
            {
                if item["type"] == "compaction" {
                    return false;
                }
                if let Some(object) = item.as_object_mut() {
                    object.remove("encrypted_content");
                }
            }
            true
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completed() -> Value {
        let encrypted = STANDARD_NO_PAD.encode((0_u8..64).collect::<Vec<_>>());
        json!({"status":"completed","output":[{"type":"reasoning","encrypted_content":encrypted},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}]})
    }

    #[test]
    fn replay_when_same_scope_is_normalized_and_deduplicated() {
        let now = Instant::now();
        let scope = ReplayScope::new("caller", "account", "model", "session").unwrap();
        let mut cache = ReasoningReplay::default();
        cache.store_completed(scope.clone(), &completed(), now);
        let mut body = json!({"input":[{"role":"assistant","content":"answer"},{"role":"user","content":"next"}]});
        cache.apply(&scope, &mut body, false, now);
        assert_eq!(body["input"].as_array().unwrap().len(), 3);
        assert_eq!(body["input"][0]["type"], "reasoning");
        cache.apply(&scope, &mut body, false, now);
        assert_eq!(body["input"].as_array().unwrap().len(), 3);
        assert!(!scope.conversation_id().contains("session"));
    }

    #[test]
    fn replay_when_any_scope_component_differs_does_not_leak() {
        let now = Instant::now();
        let scope = ReplayScope::new("caller", "account", "model", "session").unwrap();
        let mut cache = ReasoningReplay::default();
        cache.store_completed(scope, &completed(), now);
        for parts in [
            ("other", "account", "model", "session"),
            ("caller", "other", "model", "session"),
            ("caller", "account", "other", "session"),
            ("caller", "account", "model", "other"),
        ] {
            let mut body = json!({"input":[]});
            cache.apply(
                &ReplayScope::new(parts.0, parts.1, parts.2, parts.3).unwrap(),
                &mut body,
                false,
                now,
            );
            assert_eq!(body["input"], json!([]));
        }
        assert!(ReplayScope::new("", "account", "model", "session").is_none());
    }

    #[test]
    fn replay_when_expired_or_websocket_continuation_is_not_inserted() {
        let now = Instant::now();
        let scope = ReplayScope::new("caller", "account", "model", "session").unwrap();
        let mut cache = ReasoningReplay::default();
        cache.store_completed(scope.clone(), &completed(), now);
        let mut body = json!({"input":[],"previous_response_id":"previous"});
        cache.apply(&scope, &mut body, true, now);
        assert_eq!(body["input"], json!([]));
        cache.apply(&scope, &mut body, false, now + REPLAY_TTL);
        assert_eq!(body["input"], json!([]));
    }

    #[test]
    fn replay_when_assistant_history_is_ambiguous_is_not_inserted() {
        let now = Instant::now();
        let scope = ReplayScope::new("caller", "account", "model", "session").unwrap();
        let mut cache = ReasoningReplay::default();
        cache.store_completed(scope.clone(), &completed(), now);
        let mut body = json!({"input":[{"role":"assistant","content":"different"}]});
        cache.apply(&scope, &mut body, false, now);
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn schema_when_exact_codex_app_or_invalid_union_is_repaired() {
        let schema = json!({"type":"object","properties":{"keep":{"type":"string"}}});
        let mut body = json!({"tools":[
            {"type":"namespace","name":"codex_app","tools":[{"type":"function","name":"automation_update","parameters":schema,"strict":true}]},
            {"type":"function","name":"automation_update","parameters":schema},
            {"type":"function","name":"other","parameters":{"oneOf":[{"$ref":"#/$defs/a"}]}}
        ]});
        normalize_request(&mut body);
        assert_eq!(
            body["tools"][0]["tools"][0]["parameters"],
            json!({"type":"object","properties":{},"additionalProperties":true})
        );
        assert!(body["tools"][0]["tools"][0].get("strict").is_none());
        assert_eq!(body["tools"][1]["parameters"], schema);
        assert_eq!(body["tools"][2]["parameters"]["additionalProperties"], true);
    }

    #[test]
    fn encrypted_content_when_invalid_is_removed_without_dropping_summary() {
        let mut body = json!({"input":[{"type":"reasoning","summary":[{"text":"keep"}],"encrypted_content":"invalid"},{"type":"compaction","encrypted_content":null}]});
        normalize_request(&mut body);
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
        assert!(body["input"][0].get("encrypted_content").is_none());
        assert_eq!(body["input"][0]["summary"][0]["text"], "keep");
        assert!(!valid_encrypted_content(
            &STANDARD_NO_PAD.encode([0_u8; 64])
        ));
    }
}
