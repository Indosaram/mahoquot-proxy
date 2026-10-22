//! Diagnostic for Claude prompt-cache prefix breaks.
//!
//! Anthropic caches a request by exact byte prefix. When a follow-up turn in
//! the same session rewrites most of its prompt instead of reading it back,
//! something earlier in the conversation changed between turns. This module
//! fingerprints each request as an ordered list of block hashes (system,
//! tools, then every message content block) and, per session, compares it
//! with the previous request. When the previous request's blocks are not a
//! prefix of the current one, it logs the first index that differs, which
//! section it falls in and the kind of block on both sides. Only hashes and
//! block kinds are recorded, never content.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

/// Sessions idle this long are dropped; the Anthropic cache TTL is 5 minutes,
/// so an older predecessor cannot explain a miss anyway.
const SESSION_TTL: Duration = Duration::from_secs(15 * 60);
/// Upper bound on tracked sessions so a burst of one-shot callers cannot grow
/// the map without limit.
const MAX_SESSIONS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub section: &'static str,
    pub kind: String,
    pub hash: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct PrefixBreak {
    pub index: usize,
    pub previous_len: usize,
    pub current_len: usize,
    pub section: &'static str,
    pub previous_kind: String,
    pub current_kind: String,
}

#[derive(Debug, Default)]
pub struct ClaudePrefixWatch {
    sessions: Mutex<HashMap<String, (Instant, Vec<Block>)>>,
}

fn hash_value(value: &Value) -> u64 {
    let mut value = value.clone();
    if let Some(map) = value.as_object_mut() {
        map.remove("cache_control");
    }
    let digest = crate::request_history::sha256(value.to_string().as_bytes());
    u64::from_be_bytes(digest[..8].try_into().unwrap_or([0; 8]))
}

fn kind_of(value: &Value) -> String {
    match value {
        Value::String(_) => "text".to_string(),
        Value::Object(map) => map
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("object")
            .to_string(),
        _ => "other".to_string(),
    }
}

pub fn fingerprint(body: &Value) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut push = |section: &'static str, kind: String, value: &Value| {
        blocks.push(Block {
            section,
            kind,
            hash: hash_value(value),
        });
    };
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for tool in tools {
            push("tools", "tool".to_string(), tool);
        }
    }
    match body.get("system") {
        Some(Value::Array(items)) => {
            for item in items {
                push("system", kind_of(item), item);
            }
        }
        Some(other @ Value::String(_)) => push("system", "text".to_string(), other),
        _ => {}
    }
    for message in body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("?");
        match message.get("content") {
            Some(Value::Array(parts)) => {
                for part in parts {
                    push("messages", format!("{role}:{}", kind_of(part)), part);
                }
            }
            Some(content) => push("messages", format!("{role}:text"), content),
            None => {}
        }
    }
    blocks
}

pub fn first_break(previous: &[Block], current: &[Block]) -> Option<PrefixBreak> {
    let index = previous
        .iter()
        .zip(current)
        .position(|(a, b)| a.hash != b.hash)
        .or_else(|| (current.len() < previous.len()).then_some(current.len()))?;
    let before = previous.get(index);
    let after = current.get(index);
    Some(PrefixBreak {
        index,
        previous_len: previous.len(),
        current_len: current.len(),
        section: before.or(after).map(|b| b.section).unwrap_or("end"),
        previous_kind: before
            .map(|b| b.kind.clone())
            .unwrap_or_else(|| "end".to_string()),
        current_kind: after
            .map(|b| b.kind.clone())
            .unwrap_or_else(|| "end".to_string()),
    })
}

impl ClaudePrefixWatch {
    pub fn observe(&self, session: &str, body: &Value) -> Option<PrefixBreak> {
        let current = fingerprint(body);
        let now = Instant::now();
        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        if sessions.len() >= MAX_SESSIONS {
            sessions.retain(|_, (seen, _)| now.duration_since(*seen) < SESSION_TTL);
            if sessions.len() >= MAX_SESSIONS {
                sessions.clear();
            }
        }
        let previous = sessions.insert(session.to_string(), (now, current.clone()));
        let (seen, previous) = previous?;
        if now.duration_since(seen) >= SESSION_TTL {
            return None;
        }
        first_break(&previous, &current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(messages: Value) -> Value {
        json!({"system": "rules", "tools": [{"name": "read"}], "messages": messages})
    }

    #[test]
    fn an_appended_turn_is_not_a_break() {
        let watch = ClaudePrefixWatch::default();
        let first = body(json!([{"role": "user", "content": "hi"}]));
        let second = body(json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [{"type": "text", "text": "hello"}]},
            {"role": "user", "content": "next"}
        ]));
        assert_eq!(watch.observe("s", &first), None);
        assert_eq!(watch.observe("s", &second), None);
    }

    #[test]
    fn an_edited_earlier_block_reports_its_position_and_kind() {
        let watch = ClaudePrefixWatch::default();
        let first = body(json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [{"type": "thinking", "thinking": "t", "signature": "a"}]}
        ]));
        let second = body(json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [{"type": "text", "text": "dropped thinking"}]},
            {"role": "user", "content": "next"}
        ]));
        watch.observe("s", &first);
        let found = watch.observe("s", &second).expect("break");
        // tools(1) + system(1) + first user message(1) precede the edit.
        assert_eq!(found.index, 3);
        assert_eq!(found.section, "messages");
        assert_eq!(found.previous_kind, "assistant:thinking");
        assert_eq!(found.current_kind, "assistant:text");
    }

    #[test]
    fn a_shorter_request_breaks_where_it_ends() {
        let previous = fingerprint(&body(json!([
            {"role": "user", "content": "a"},
            {"role": "user", "content": "b"}
        ])));
        let current = fingerprint(&body(json!([{"role": "user", "content": "a"}])));
        let found = first_break(&previous, &current).expect("break");
        assert_eq!(found.index, current.len());
        assert_eq!(found.current_kind, "end");
    }

    #[test]
    fn a_moved_cache_breakpoint_is_not_a_break() {
        let watch = ClaudePrefixWatch::default();
        let first = body(json!([
            {"role": "user", "content": [{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}]}
        ]));
        let second = body(json!([
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            {"role": "user", "content": [{"type": "text", "text": "next", "cache_control": {"type": "ephemeral"}}]}
        ]));
        watch.observe("s", &first);
        assert_eq!(watch.observe("s", &second), None);
    }

    #[test]
    fn sessions_are_tracked_independently() {
        let watch = ClaudePrefixWatch::default();
        watch.observe("a", &body(json!([{"role": "user", "content": "x"}])));
        assert_eq!(
            watch.observe("b", &body(json!([{"role": "user", "content": "y"}]))),
            None
        );
    }
}
