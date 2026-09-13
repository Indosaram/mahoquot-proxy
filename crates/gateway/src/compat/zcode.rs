//! Request-body shaping the zcode.z.ai plan gateway requires.
//!
//! The gateway inspects the POST body and rejects requests that do not carry
//! the official ZCode identity with biz code 3012 even when the bearer JWT is
//! valid. Three transforms mirror the official client's LLM calls:
//!
//!   1. The static ZCode identity system blocks are prepended to `system` (the
//!      dynamic powered-by line is merged into the trailing Environment block's
//!      text, never sent as a separate block), and a Claude-Code identity block
//!      is dropped so the model sees exactly one identity.
//!   2. Two-phase `cache_control`: stray markers on non-system content blocks
//!      are stripped, then the last content block of the last non-system
//!      message is marked ephemeral — the breakpoint the official client keeps.
//!   3. `metadata.user_id` is set from the plan JWT's `user_id` claim, keeping
//!      any other metadata fields.

use serde_json::{json, Value};

use super::claude::CLAUDE_CODE_SYSTEM_INSTRUCTION;

const SYSTEM_BLOCKS_JSON: &str = include_str!("zcode-system-blocks.json");

fn system_blocks() -> Vec<Value> {
    serde_json::from_str(SYSTEM_BLOCKS_JSON).expect("embedded zcode system blocks are valid JSON")
}

fn build_plan_system(existing_system: Option<&Value>, model: &str) -> Vec<Value> {
    let mut official = system_blocks();
    if let Some(env) = official.last_mut() {
        if let Some(text) = env.get("text").and_then(Value::as_str) {
            let base = text.to_string();
            env["text"] = json!(format!(
                "{base}\n- You are powered by the model named {model}."
            ));
        }
    }
    official.extend(normalize_user_system(existing_system));
    official
}

/// Caller system entries become text blocks. Caller content is never dropped,
/// except the Claude-Code identity block: a dual identity breaks the gateway
/// fingerprint the same way a missing identity does.
fn normalize_user_system(system: Option<&Value>) -> Vec<Value> {
    let Some(system) = system else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(text) = system.as_str() {
        let text = text.trim();
        if !text.is_empty() {
            out.push(json!({ "type": "text", "text": text }));
        }
        return out;
    }
    let Some(items) = system.as_array() else {
        return out;
    };
    for item in items {
        if let Some(text) = item.as_str() {
            if !text.trim().is_empty() {
                out.push(json!({ "type": "text", "text": text }));
            }
            continue;
        }
        let Some(obj) = item.as_object() else {
            continue;
        };
        let text = obj.get("text").and_then(Value::as_str);
        if obj.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(text) =
                text.filter(|t| !t.trim().is_empty() && *t != CLAUDE_CODE_SYSTEM_INSTRUCTION)
            {
                let mut block = json!({ "type": "text", "text": text });
                if let Some(cc) = obj.get("cache_control") {
                    block["cache_control"] = cc.clone();
                }
                out.push(block);
            }
            continue;
        }
        match text {
            Some(text) if !text.trim().is_empty() => {
                out.push(json!({ "type": "text", "text": text }));
            }
            None if !item.to_string().trim().is_empty() => {
                out.push(json!({ "type": "text", "text": item.to_string() }));
            }
            _ => {}
        }
    }
    out
}

fn strip_message_cache_control(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for msg in messages.iter_mut() {
        let Some(obj) = msg.as_object_mut() else {
            continue;
        };
        if obj.get("role").and_then(Value::as_str) == Some("system") {
            continue;
        }
        if let Some(content) = obj.get_mut("content").and_then(Value::as_array_mut) {
            for block in content.iter_mut() {
                if let Some(block_obj) = block.as_object_mut() {
                    block_obj.remove("cache_control");
                }
            }
        }
    }
}

fn mark_last_message_ephemeral(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for msg in messages.iter_mut().rev() {
        let Some(obj) = msg.as_object_mut() else {
            continue;
        };
        if obj.get("role").and_then(Value::as_str) == Some("system") {
            continue;
        }
        match obj.get_mut("content") {
            Some(Value::String(text)) => {
                let text = text.clone();
                obj["content"] = json!([{
                    "type": "text",
                    "text": text,
                    "cache_control": { "type": "ephemeral" }
                }]);
                return;
            }
            Some(Value::Array(blocks)) if !blocks.is_empty() => {
                let last = blocks.len() - 1;
                if let Some(block) = blocks[last].as_object_mut() {
                    if block.get("cache_control").is_none() {
                        block.insert("cache_control".to_string(), json!({ "type": "ephemeral" }));
                    }
                }
                return;
            }
            _ => return,
        }
    }
}

pub fn apply_zcode_plan_identity(body: &mut Value, requested_model: &str, user_id: Option<&str>) {
    let model = mahoquot_providers::zcode::normalize_plan_model(requested_model);
    if !model.is_empty() {
        body["model"] = json!(model);
    }
    let existing_system = body.get("system").cloned();
    body["system"] = Value::Array(build_plan_system(existing_system.as_ref(), &model));
    strip_message_cache_control(body);
    mark_last_message_ephemeral(body);
    if let Some(user_id) = user_id {
        let metadata = body.get("metadata").cloned().unwrap_or_else(|| json!({}));
        let mut metadata = match metadata {
            Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        metadata.insert("user_id".to_string(), json!(user_id));
        body["metadata"] = Value::Object(metadata);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JWT_USER_ID: &str = "usr-42";

    fn sample_body() -> Value {
        json!({
            "model": "glm-5.3-flash",
            "system": "You are a helpful coding assistant.",
            "messages": [
                { "role": "user", "content": [
                    { "type": "text", "text": "hi", "cache_control": { "type": "ephemeral" } }
                ]},
                { "role": "assistant", "content": [
                    { "type": "text", "text": "hello" }
                ]}
            ]
        })
    }

    fn block_texts(system: &[Value]) -> Vec<String> {
        system
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str).map(str::to_string))
            .collect()
    }

    #[test]
    fn prepends_official_blocks_and_merges_powered_by() {
        let mut body = sample_body();
        apply_zcode_plan_identity(&mut body, "glm-5.3-flash", Some(JWT_USER_ID));
        let system = body["system"].as_array().unwrap();
        let texts = block_texts(system);
        assert_eq!(system.len(), 4);
        assert!(texts[0].starts_with("You are ZCode"));
        assert!(system[2]["text"]
            .as_str()
            .unwrap()
            .contains("You are powered by the model named GLM-5.3-Flash."));
        assert_eq!(texts[3], "You are a helpful coding assistant.");
        assert_eq!(body["model"], "GLM-5.3-Flash");
    }

    #[test]
    fn drops_claude_code_identity_but_keeps_caller_content() {
        let mut body = sample_body();
        body["system"] = json!([
            { "type": "text", "text": CLAUDE_CODE_SYSTEM_INSTRUCTION },
            { "type": "text", "text": "Custom identity: Pip the penguin." }
        ]);
        apply_zcode_plan_identity(&mut body, "glm-5.2", None);
        let texts = block_texts(body["system"].as_array().unwrap());
        assert!(!texts.iter().any(|t| t == CLAUDE_CODE_SYSTEM_INSTRUCTION));
        assert!(texts.iter().any(|t| t.contains("Pip the penguin")));
    }

    #[test]
    fn coerces_unrecognized_caller_system_entries_to_text() {
        let mut body = sample_body();
        body["system"] = json!([ { "type": "other", "text": "tool-ish entry" } ]);
        apply_zcode_plan_identity(&mut body, "glm-5.2", None);
        let texts = block_texts(body["system"].as_array().unwrap());
        assert!(texts.iter().any(|t| t.contains("tool-ish entry")));
    }

    #[test]
    fn cache_control_marks_last_message_block_only() {
        let mut body = sample_body();
        apply_zcode_plan_identity(&mut body, "glm-5.2", None);
        let first = &body["messages"][0]["content"][0];
        assert!(first.get("cache_control").is_none());
        let last = &body["messages"][1]["content"][0];
        assert_eq!(last["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn metadata_user_id_preserves_other_fields() {
        let mut body = sample_body();
        body["metadata"] = json!({ "request_id": "r-9" });
        apply_zcode_plan_identity(&mut body, "glm-5.2", Some(JWT_USER_ID));
        assert_eq!(body["metadata"]["user_id"], JWT_USER_ID);
        assert_eq!(body["metadata"]["request_id"], "r-9");
    }

    #[test]
    fn official_blocks_carry_cache_control() {
        let mut body = sample_body();
        apply_zcode_plan_identity(&mut body, "glm-5.2", None);
        for block in &body["system"].as_array().unwrap()[..3] {
            assert_eq!(block["cache_control"]["type"], "ephemeral");
        }
    }

    #[test]
    fn unknown_model_ids_pass_through() {
        let mut body = sample_body();
        body["model"] = json!("glm-4.6");
        apply_zcode_plan_identity(&mut body, "glm-4.6", None);
        assert_eq!(body["model"], "glm-4.6");
    }
}
