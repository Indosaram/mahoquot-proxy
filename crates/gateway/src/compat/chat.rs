//! Decoder for a generic OpenAI chat-completions upstream.
//!
//! A generic account can be configured with an `openai-chat` adapter, and a
//! Responses client can still call it: the request is translated to chat shape
//! by `responses_to_openai`, so the reply is chat-shaped (`choices[].delta`)
//! while the client expects Responses events. Feeding chat frames to the
//! Responses decoder yields no text at all, so this adapter performs the one
//! conversion the Responses renderer cannot: chat wire into canonical events.

use serde_json::Value;

use super::events::{CodexEvent, Usage};

/// Incremental chat-completions decoder.
///
/// Tool calls arrive as indexed deltas whose `id`/`name` appear on the first
/// fragment only, so the decoder remembers the per-index identity and reports
/// it with the first arguments delta. `ToolCallBegin` is therefore emitted at
/// the first fragment that carries either an id or a name, which keeps a
/// provider that splits identity across frames working.
#[derive(Default)]
pub struct ChatDecoder {
     started: bool,
    finished: bool,
    usage: Option<Usage>,
    tools: Vec<ChatTool>,
    announced: Vec<u64>,
}

#[derive(Default)]
struct ChatTool {
    index: u64,
    id: String,
    name: String,
}

impl ChatDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decodes one chat-completions JSON payload, which may be a stream delta
    /// or a whole buffered completion.
    pub fn decode_value(&mut self, value: &Value, out: &mut Vec<CodexEvent>) {
        if self.finished {
            return;
        }
        if let Some(error) = value.get("error") {
            self.fail(error_message(error), out);
            return;
        }
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            if !self.started {
                self.started = true;
                out.push(CodexEvent::Created {
                    response_id: id.to_string(),
                });
            }
        }
        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            self.record_usage(usage);
        }
        let Some(choices) = value.get("choices").and_then(Value::as_array) else {
            return;
        };
        for choice in choices {
            if let Some(delta) = choice.get("delta") {
                self.message(delta, out);
            }
            if let Some(message) = choice.get("message") {
                // A buffered reply puts the whole assistant turn in one frame.
                self.message(message, out);
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                if reason == "content_filter" {
                    out.push(CodexEvent::ContentFilterReached);
                } else if reason == "length" {
                    out.push(CodexEvent::OutputLimitReached);
                }
            }
        }
    }

    /// Decodes one SSE frame. The shared parser hands over the joined
    /// `data:` payload already stripped, so a frame that still carries the
    /// field name is accepted too: the same decoder serves the streaming path
    /// and a caller holding a raw frame.
    pub fn decode_frame(&mut self, frame: &[u8], out: &mut Vec<CodexEvent>) {
        let mut payload = Vec::new();
        let mut saw_data = false;
        for line in frame.split(|byte| *byte == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let Some(rest) = line.strip_prefix(b"data:") else {
                continue;
            };
            saw_data = true;
            payload.extend_from_slice(rest.trim_ascii_start());
            payload.push(b'\n');
        }
        let payload = if saw_data {
            payload.trim_ascii()
        } else {
            frame.trim_ascii()
        };
        if payload.is_empty() {
            return;
        }
        if payload == b"[DONE]" {
            self.finish(out);
            return;
        }
        match serde_json::from_slice::<Value>(payload) {
            Ok(value) => self.decode_value(&value, out),
            Err(error) => self.fail(format!("invalid chat completion frame: {error}"), out),
        }
    }

    /// Flushes the terminal event. A chat stream ends with `data: [DONE]` and a
    /// buffered reply has no event at all, so the caller must close it.
    pub fn finish(&mut self, out: &mut Vec<CodexEvent>) {
        if self.finished {
            return;
        }
        self.finished = true;
        if !self.started {
            out.push(CodexEvent::Created {
                response_id: String::new(),
            });
        }
        out.push(CodexEvent::Completed {
            usage: self.usage.clone(),
        });
    }

    fn message(&mut self, message: &Value, out: &mut Vec<CodexEvent>) {
        if let Some(reasoning) = message
            .get("reasoning_content")
            .or_else(|| message.get("reasoning"))
            .and_then(Value::as_str)
        {
            if !reasoning.is_empty() {
                out.push(CodexEvent::ReasoningDelta(reasoning.to_string()));
            }
        }
        if let Some(content) = message.get("content") {
            match content {
                Value::String(text) if !text.is_empty() => {
                    out.push(CodexEvent::TextDelta(text.clone()));
                }
                // Some providers emit content as an array of typed parts.
                Value::Array(parts) => {
                    for part in parts {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                out.push(CodexEvent::TextDelta(text.to_string()));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let Some(calls) = message.get("tool_calls").and_then(Value::as_array) else {
            return;
        };
        for (position, call) in calls.iter().enumerate() {
            let index = call
                .get("index")
                .and_then(Value::as_u64)
                .unwrap_or(position as u64);
            let function = call.get("function");
            let slot = match self.tools.iter_mut().find(|tool| tool.index == index) {
                Some(slot) => slot,
                None => {
                    self.tools.push(ChatTool {
                        index,
                        ..ChatTool::default()
                    });
                    self.tools.last_mut().expect("just pushed")
                }
            };
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                if !id.is_empty() {
                    slot.id = id.to_string();
                }
            }
            if let Some(name) = function.and_then(|f| f.get("name")).and_then(Value::as_str) {
                if !name.is_empty() {
                    slot.name = name.to_string();
                }
            }
            let arguments = function
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if slot.id.is_empty() && slot.name.is_empty() && arguments.is_empty() {
                continue;
            }
            if !self.announced.contains(&index) {
                self.announced.push(index);
                out.push(CodexEvent::ToolCallBegin {
                    output_index: index,
                    call_id: if slot.id.is_empty() {
                        format!("call_{index}")
                    } else {
                        slot.id.clone()
                    },
                    name: slot.name.clone(),
                });
            }
            if !arguments.is_empty() {
                out.push(CodexEvent::ToolArgsDelta {
                    output_index: index,
                    delta: arguments.to_string(),
                });
            }
        }
    }

    fn record_usage(&mut self, usage: &Value) {
        self.usage = Some(chat_usage(usage));
    }

    fn fail(&mut self, message: String, out: &mut Vec<CodexEvent>) {
        if self.finished {
            return;
        }
        self.finished = true;
        out.push(CodexEvent::Failed { message });
    }
}

fn error_message(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or("upstream chat error")
        .to_string()
}

/// Chat usage is OpenAI-shaped, so the shared parser applies unchanged. It is
/// private to `events`, so the fields are mapped here.
pub(crate) fn chat_usage(usage: &Value) -> Usage {
    let prompt_tokens = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion_tokens = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total_tokens = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(prompt_tokens + completion_tokens);
    let cached_tokens = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .or_else(|| usage.get("cache_read_input_tokens"))
        .and_then(Value::as_u64);
    let cache_write_tokens = usage
        .pointer("/prompt_tokens_details/cache_creation_tokens")
        .or_else(|| usage.get("cache_creation_input_tokens"))
        .and_then(Value::as_u64);
    let reasoning_tokens = usage
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
        cached_tokens: cached_tokens.unwrap_or(0),
        cached_tokens_known: cached_tokens.is_some(),
        reasoning_tokens,
        cache_write_tokens: cache_write_tokens.unwrap_or(0),
        cache_write_tokens_known: cache_write_tokens.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::events::CodexEvent;
    use serde_json::json;

    fn decode(frames: &[&str]) -> Vec<CodexEvent> {
        let mut decoder = ChatDecoder::new();
        let mut out = Vec::new();
        for frame in frames {
            decoder.decode_frame(frame.as_bytes(), &mut out);
        }
        decoder.finish(&mut out);
        out
    }

    #[test]
    fn chat_stream_deltas_reach_the_responses_renderer_as_text() {
        let events = decode(&[
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}",
            "data: [DONE]",
        ]);
        assert!(matches!(events.first(), Some(CodexEvent::Created { .. })));
        assert_eq!(
            events
                .iter()
                .filter_map(|event| match event {
                    CodexEvent::TextDelta(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>(),
            "Hello"
        );
        let mut renderer = crate::compat::responses::ResponsesStreamRenderer::new("m".into(), 0);
        for event in events {
            renderer.render(event);
        }
        assert!(renderer.terminated(), "a chat stream must terminate the renderer");
        let body = renderer.current_response().to_string();
        assert!(body.contains("Hello"), "{body}");
    }

    #[test]
    fn chat_stream_tool_fragments_keep_their_identity() {
        let events = decode(&[
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":\"}}]}}]}",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"x\\\"}\"}}]}}]}",
            "data: [DONE]",
        ]);
        let begin = events
            .iter()
            .find_map(|event| match event {
                CodexEvent::ToolCallBegin { call_id, name, .. } => Some((call_id, name)),
                _ => None,
            })
            .expect("a tool call begins");
        assert_eq!(begin.0, "call_a");
        assert_eq!(begin.1, "lookup");
        let args = events
            .iter()
            .filter_map(|event| match event {
                CodexEvent::ToolArgsDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(args, "{\"q\":\"x\"}");
        assert_eq!(events.iter().filter(|e| matches!(e, CodexEvent::ToolCallBegin { .. })).count(), 1);
    }

    #[test]
    fn buffered_chat_completion_carries_text_usage_and_tools() {
        let mut decoder = ChatDecoder::new();
        let mut events = Vec::new();
        decoder.decode_value(
            &json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "done",
                        "tool_calls": [{
                            "id": "call_b", "type": "function",
                            "function": {"name": "f", "arguments": "{}"}
                        }]
                    },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18,
                    "prompt_tokens_details": {"cached_tokens": 5},
                    "completion_tokens_details": {"reasoning_tokens": 3}
                }
            }),
            &mut events,
        );
        decoder.finish(&mut events);
        assert!(events.iter().any(|e| matches!(e, CodexEvent::TextDelta(t) if t == "done")));
        let usage = events
            .iter()
            .find_map(|event| match event {
                CodexEvent::Completed { usage } => usage.clone(),
                _ => None,
            })
            .expect("usage is reported");
        assert_eq!(usage.prompt_tokens, 11);
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(usage.cached_tokens, 5);
        assert!(usage.cached_tokens_known);
        assert_eq!(usage.reasoning_tokens, 3);
    }

    #[test]
    fn a_malformed_frame_fails_the_stream_instead_of_vanishing() {
        let events = decode(&["data: {oops", "data: [DONE]"]);
        assert!(matches!(events.last(), Some(CodexEvent::Failed { .. })));
        assert!(!events
            .iter()
            .any(|event| matches!(event, CodexEvent::Completed { .. })));
    }
}
