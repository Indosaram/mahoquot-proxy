use bytes::Bytes;
use serde_json::{json, Value};

use super::events::{CodexEvent, Usage};

pub const DONE_FRAME: &[u8] = b"data: [DONE]\n\n";

fn frame(payload: &Value) -> Bytes {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(b"data: ");
    if serde_json::to_writer(&mut buf, payload).is_err() {
        return Bytes::new();
    }
    buf.extend_from_slice(b"\n\n");
    Bytes::from(buf)
}

fn usage_value(usage: &Usage) -> Value {
    json!({
        "prompt_tokens": usage.prompt_tokens,
        "completion_tokens": usage.completion_tokens,
        "total_tokens": usage.total_tokens,
        "prompt_tokens_details": {
            "cached_tokens": usage.cached_tokens,
        },
        "completion_tokens_details": {
            "reasoning_tokens": usage.reasoning_tokens,
        },
    })
}

/// Gemini-native SSE. CP streams upstream-shaped frames here rather than
/// OpenAI chunks: every frame carries `candidates`/`modelVersion`/`responseId`,
/// usage rides on the terminal frame, and the stream ends without `[DONE]`.
pub struct GeminiChunkRenderer {
    id: String,
    model: String,
    terminated: bool,
    /// Gemini functionCall parts must carry a complete args object, while the
    /// decoded stream supplies incremental JSON deltas. Open calls hold only
    /// the growing argument string until the call closes (the next begin or
    /// the terminal event); text and reasoning deltas stream out immediately.
    open_calls: Vec<(String, String)>,
}

impl GeminiChunkRenderer {
    pub fn new(model: String, created: i64) -> Self {
        Self {
            id: format!("resp-{created}"),
            model,
            terminated: false,
            open_calls: Vec::new(),
        }
    }

    pub fn terminated(&self) -> bool {
        self.terminated
    }

    fn frame_for(&self, parts: Value, finish: Option<&str>, usage: Option<&Usage>) -> Bytes {
        let mut candidate = json!({"content": {"role": "model", "parts": parts}});
        if let Some(reason) = finish {
            candidate["finishReason"] = Value::String(reason.to_string());
            candidate["index"] = json!(0);
        }
        let mut payload = json!({
            "candidates": [candidate],
            "modelVersion": self.model,
            "responseId": self.id,
        });
        if let Some(u) = usage {
            payload["usageMetadata"] = json!({
                "promptTokenCount": u.prompt_tokens,
                "candidatesTokenCount": u.completion_tokens,
                "totalTokenCount": u.total_tokens,
                "thoughtsTokenCount": u.reasoning_tokens,
            });
        }
        frame(&payload)
    }

    /// Emit every closed call as a functionCall part, restoring its arguments.
    fn close_open_calls(&mut self, out: &mut Vec<Bytes>) {
        for (name, args) in std::mem::take(&mut self.open_calls) {
            let parsed: Value = if args.is_empty() {
                json!({})
            } else {
                serde_json::from_str(&args).unwrap_or_else(|_| json!({}))
            };
            out.push(self.frame_for(
                json!([{ "functionCall": { "name": name, "args": parsed } }]),
                None,
                None,
            ));
        }
    }

    pub fn render(&mut self, event: CodexEvent) -> Vec<Bytes> {
        match event {
            CodexEvent::Created { response_id } => {
                if !response_id.is_empty() {
                    self.id = response_id;
                }
                Vec::new()
            }
            CodexEvent::TextDelta(text) => {
                vec![self.frame_for(json!([{"text": text}]), None, None)]
            }
            CodexEvent::ReasoningDelta(text) => {
                vec![self.frame_for(json!([{"text": text, "thought": true}]), None, None)]
            }
            CodexEvent::ReasoningSignature(sig) => {
                vec![self.frame_for(json!([{"thoughtSignature": sig}]), None, None)]
            }
            CodexEvent::ToolCallBegin { name, .. } => {
                let mut out = Vec::new();
                self.close_open_calls(&mut out);
                self.open_calls.push((name, String::new()));
                out
            }
            CodexEvent::ToolArgsDelta { delta, .. } => {
                if let Some((_, args)) = self.open_calls.last_mut() {
                    args.push_str(&delta);
                }
                Vec::new()
            }
            CodexEvent::Completed { usage } => {
                self.terminated = true;
                let mut out = Vec::new();
                self.close_open_calls(&mut out);
                out.push(self.frame_for(json!([]), Some("STOP"), usage.as_ref()));
                out
            }
            CodexEvent::Failed { message } => {
                self.terminated = true;
                self.open_calls.clear();
                vec![frame(&json!({
                    "error": {"code": 500, "message": message, "status": "INTERNAL"},
                }))]
            }
        }
    }

    pub fn close_unterminated(&mut self) -> Vec<Bytes> {
        if self.terminated {
            return Vec::new();
        }
        self.terminated = true;
        let mut out = Vec::new();
        self.close_open_calls(&mut out);
        out.push(self.frame_for(json!([]), Some("STOP"), None));
        out
    }
}

pub struct ChunkRenderer {
    id: String,
    model: String,
    created: i64,
    include_usage: bool,
    role_sent: bool,
    saw_tool_call: bool,
    terminated: bool,
    tool_slots: Vec<u64>,
}

impl ChunkRenderer {
    pub fn new(model: String, created: i64, include_usage: bool) -> Self {
        Self {
            id: format!("chatcmpl-{created}"),
            model,
            created,
            include_usage,
            role_sent: false,
            saw_tool_call: false,
            terminated: false,
            tool_slots: Vec::new(),
        }
    }

    pub fn terminated(&self) -> bool {
        self.terminated
    }

    fn slot(&mut self, output_index: u64) -> usize {
        match self.tool_slots.iter().position(|i| *i == output_index) {
            Some(pos) => pos,
            None => {
                self.tool_slots.push(output_index);
                self.tool_slots.len() - 1
            }
        }
    }

    fn chunk(&self, delta: Value, finish_reason: Option<&str>) -> Bytes {
        frame(&json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
        }))
    }

    fn role_prelude(&mut self, out: &mut Vec<Bytes>) {
        if !self.role_sent {
            self.role_sent = true;
            out.push(self.chunk(json!({"role": "assistant", "content": ""}), None));
        }
    }

    pub fn render(&mut self, event: CodexEvent) -> Vec<Bytes> {
        let mut out = Vec::new();
        match event {
            CodexEvent::Created { response_id } => {
                if !response_id.is_empty() {
                    self.id = format!("chatcmpl-{response_id}");
                }
            }
            CodexEvent::TextDelta(text) => {
                self.role_prelude(&mut out);
                out.push(self.chunk(json!({"content": text}), None));
            }
            CodexEvent::ReasoningDelta(text) => {
                // Forward reasoning as the de-facto `reasoning_content` delta
                // field. Dropping it left the client byte-silent for the whole
                // thinking phase (minutes on GLM-5.x), tripping idle timeouts —
                // the "responds nothing and stalls" symptom. Parsers that
                // don't know the field simply ignore it, but bytes still flow.
                self.role_prelude(&mut out);
                out.push(self.chunk(json!({"reasoning_content": text}), None));
            }
            CodexEvent::ReasoningSignature(_) => {}
            CodexEvent::ToolCallBegin {
                output_index,
                call_id,
                name,
            } => {
                self.role_prelude(&mut out);
                self.saw_tool_call = true;
                let index = self.slot(output_index);
                out.push(self.chunk(
                    json!({"tool_calls": [{
                        "index": index,
                        "id": call_id,
                        "type": "function",
                        "function": {"name": name, "arguments": ""},
                    }]}),
                    None,
                ));
            }
            CodexEvent::ToolArgsDelta {
                output_index,
                delta,
            } => {
                self.role_prelude(&mut out);
                let index = self.slot(output_index);
                out.push(self.chunk(
                    json!({"tool_calls": [{
                        "index": index,
                        "function": {"arguments": delta},
                    }]}),
                    None,
                ));
            }
            CodexEvent::Completed { usage } => {
                self.role_prelude(&mut out);
                let reason = if self.saw_tool_call {
                    "tool_calls"
                } else {
                    "stop"
                };
                out.push(self.chunk(json!({}), Some(reason)));
                if self.include_usage {
                    let usage = usage.as_ref().map(usage_value).unwrap_or(Value::Null);
                    out.push(frame(&json!({
                        "id": self.id,
                        "object": "chat.completion.chunk",
                        "created": self.created,
                        "model": self.model,
                        "choices": [],
                        "usage": usage,
                    })));
                }
                out.push(Bytes::from_static(DONE_FRAME));
                self.terminated = true;
            }
            CodexEvent::Failed { message } => {
                out.push(frame(&json!({
                    "error": {"message": message, "type": "upstream_error"},
                })));
                out.push(Bytes::from_static(DONE_FRAME));
                self.terminated = true;
            }
        }
        out
    }

    pub fn close_unterminated(&mut self) -> Vec<Bytes> {
        if self.terminated {
            return Vec::new();
        }
        self.terminated = true;
        let mut out = Vec::new();
        self.role_prelude(&mut out);
        let reason = if self.saw_tool_call {
            "tool_calls"
        } else {
            "stop"
        };
        out.push(self.chunk(json!({}), Some(reason)));
        out.push(Bytes::from_static(DONE_FRAME));
        out
    }
}

#[derive(Default)]
struct ToolAccumulator {
    output_index: u64,
    call_id: String,
    name: String,
    arguments: String,
}

pub struct Aggregator {
    id: String,
    model: String,
    created: i64,
    text: String,
    reasoning_signature: Option<String>,
    tools: Vec<ToolAccumulator>,
    usage: Option<Usage>,
    failure: Option<String>,
}

impl Aggregator {
    pub fn new(model: String, created: i64) -> Self {
        Self {
            id: format!("chatcmpl-{created}"),
            model,
            created,
            text: String::new(),
            reasoning_signature: None,
            tools: Vec::new(),
            usage: None,
            failure: None,
        }
    }

    pub fn push(&mut self, event: CodexEvent) {
        match event {
            CodexEvent::Created { response_id } => {
                if !response_id.is_empty() {
                    self.id = format!("chatcmpl-{response_id}");
                }
            }
            CodexEvent::TextDelta(text) => self.text.push_str(&text),
            CodexEvent::ReasoningDelta(_) => {}
            CodexEvent::ToolCallBegin {
                output_index,
                call_id,
                name,
            } => self.tools.push(ToolAccumulator {
                output_index,
                call_id,
                name,
                arguments: String::new(),
            }),
            CodexEvent::ToolArgsDelta {
                output_index,
                delta,
            } => {
                if let Some(tool) = self
                    .tools
                    .iter_mut()
                    .find(|t| t.output_index == output_index)
                {
                    tool.arguments.push_str(&delta);
                }
            }
            CodexEvent::ReasoningSignature(sig) => self.reasoning_signature = Some(sig),
            CodexEvent::Completed { usage } => self.usage = usage,
            CodexEvent::Failed { message } => self.failure = Some(message),
        }
    }

    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// Legacy `/v1/completions` shape: `text` on the choice and no `message`,
    /// with `object` set to `text_completion` rather than `chat.completion`.
    pub fn into_text_completion(self) -> Value {
        let mut payload = json!({
            "id": self.id,
            "object": "text_completion",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "text": self.text, "finish_reason": "stop"}],
        });
        if let Some(usage) = self.usage.as_ref() {
            payload["usage"] = usage_value(usage);
        }
        payload
    }

    /// Gemini-native shape for the `/v1beta` surface, which nests text under
    /// `candidates[].content.parts[]` instead of `choices[]`.
    pub fn into_gemini(self) -> Value {
        let mut part = json!({"text": self.text});
        if let Some(sig) = self.reasoning_signature.as_ref() {
            part["thoughtSignature"] = Value::String(sig.clone());
        }
        let mut payload = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [part]},
                "finishReason": "STOP",
                "index": 0,
            }],
            "modelVersion": self.model,
            "responseId": self.id,
        });
        if let Some(usage) = self.usage.as_ref() {
            payload["usageMetadata"] = json!({
                "promptTokenCount": usage.prompt_tokens,
                "candidatesTokenCount": usage.completion_tokens,
                "totalTokenCount": usage.total_tokens,
                "thoughtsTokenCount": usage.reasoning_tokens,
            });
        }
        payload
    }

    pub fn into_completion(self) -> Value {
        let mut message = json!({"role": "assistant", "content": Value::Null});
        if !self.text.is_empty() {
            message["content"] = Value::String(self.text);
        }
        let finish_reason = if self.tools.is_empty() {
            "stop"
        } else {
            message["tool_calls"] = Value::Array(
                self.tools
                    .iter()
                    .map(|t| {
                        json!({
                            "id": t.call_id,
                            "type": "function",
                            "function": {"name": t.name, "arguments": t.arguments},
                        })
                    })
                    .collect(),
            );
            "tool_calls"
        };

        let mut payload = json!({
            "id": self.id,
            "object": "chat.completion",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
        });
        if let Some(usage) = self.usage.as_ref() {
            payload["usage"] = usage_value(usage);
        }
        payload
    }
}

#[cfg(test)]
mod openai_stream_tests {
    use super::*;

    fn payloads(frames: Vec<Bytes>) -> Vec<Value> {
        frames
            .iter()
            .filter_map(|f| {
                let text = String::from_utf8_lossy(f);
                let body = text.strip_prefix("data: ")?.trim();
                serde_json::from_str(body).ok()
            })
            .collect()
    }

    // The thinking phase can run for minutes on GLM-5.x. Forwarding reasoning
    // as `reasoning_content` keeps bytes flowing so client idle timeouts do not
    // fire; clients that don't know the field ignore it.
    #[test]
    fn reasoning_deltas_forward_as_reasoning_content() {
        let mut r = ChunkRenderer::new("glm-5.3".into(), 0, false);
        let mut frames = r.render(CodexEvent::ReasoningDelta(" pondering".into()));
        frames.extend(r.render(CodexEvent::TextDelta("answer".into())));
        let out = payloads(frames);
        let reasoning: Vec<&Value> = out
            .iter()
            .filter(|frame| {
                frame["choices"][0]["delta"]
                    .get("reasoning_content")
                    .is_some()
            })
            .collect();
        assert_eq!(reasoning.len(), 1);
        assert_eq!(
            reasoning[0]["choices"][0]["delta"]["reasoning_content"],
            " pondering"
        );
        let text = out
            .iter()
            .find(|frame| frame["choices"][0]["delta"]["content"] == "answer")
            .expect("text delta");
        assert_eq!(text["choices"][0]["delta"]["content"], "answer");
        // The role prelude must precede both.
        assert_eq!(out[0]["choices"][0]["delta"]["role"], "assistant");
    }
}

#[cfg(test)]
mod gemini_stream_tests {
    use super::*;

    fn payloads(frames: Vec<Bytes>) -> Vec<Value> {
        frames
            .iter()
            .filter_map(|f| {
                let text = String::from_utf8_lossy(f);
                let body = text.strip_prefix("data: ")?.trim();
                serde_json::from_str(body).ok()
            })
            .collect()
    }

    fn usage() -> Usage {
        Usage {
            prompt_tokens: 5,
            completion_tokens: 1,
            total_tokens: 92,
            cached_tokens: 0,
            reasoning_tokens: 86,
        }
    }

    // Gemini-native streams carry usageMetadata only on the terminal frame;
    // earlier chunks stream out immediately so TTFT survives translation.
    #[test]
    fn text_deltas_stream_immediately_and_usage_rides_the_terminal_frame() {
        let mut r = GeminiChunkRenderer::new("gemini-3-flash".into(), 1);
        let immediate = payloads(r.render(CodexEvent::TextDelta("OK".into())));
        assert_eq!(immediate.len(), 1);
        assert!(immediate[0].get("usageMetadata").is_none());
        assert_eq!(immediate[0]["candidates"][0]["content"]["parts"][0]["text"], "OK");

        let out = payloads(r.render(CodexEvent::Completed {
            usage: Some(usage()),
        }));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["usageMetadata"]["promptTokenCount"], 5);
        assert_eq!(out[0]["usageMetadata"]["thoughtsTokenCount"], 86);
        assert_eq!(out[0]["modelVersion"], "gemini-3-flash");
        assert!(out[0].get("responseId").is_some());
        assert!(out[0].get("choices").is_none());
    }

    #[test]
    fn terminal_frame_sets_stop_and_stream_has_no_done_sentinel() {
        let mut r = GeminiChunkRenderer::new("gemini-3-flash".into(), 1);
        let mut frames = r.render(CodexEvent::TextDelta("a".into()));
        frames.extend(r.render(CodexEvent::TextDelta("b".into())));
        frames.extend(r.render(CodexEvent::Completed {
            usage: Some(usage()),
        }));
        let rendered: Vec<String> = frames
            .iter()
            .map(|f| String::from_utf8_lossy(f).to_string())
            .collect();
        assert!(!rendered.iter().any(|f| f.contains("[DONE]")));
        let out = payloads(frames);
        assert_eq!(out.len(), 3);
        assert!(out[0]["candidates"][0].get("finishReason").is_none());
        assert_eq!(out[2]["candidates"][0]["finishReason"], "STOP");
        assert!(r.terminated());
    }

    #[test]
    fn tool_calls_stream_as_function_call_parts_with_restored_arguments() {
        let mut r = GeminiChunkRenderer::new("gemini-3-flash".into(), 1);
        let mut frames = r.render(CodexEvent::ToolCallBegin {
            output_index: 0,
            call_id: "call_1".into(),
            name: "weather".into(),
        });
        frames.extend(r.render(CodexEvent::ToolArgsDelta {
            output_index: 0,
            delta: "{\"city\":".into(),
        }));
        frames.extend(r.render(CodexEvent::ToolArgsDelta {
            output_index: 0,
            delta: "\"seoul\"}".into(),
        }));
        frames.extend(r.render(CodexEvent::ToolCallBegin {
            output_index: 1,
            call_id: "call_2".into(),
            name: "clock".into(),
        }));
        frames.extend(r.render(CodexEvent::ToolArgsDelta {
            output_index: 1,
            delta: "{}".into(),
        }));
        frames.extend(r.render(CodexEvent::Completed { usage: None }));

        let out = payloads(frames);
        assert_eq!(out.len(), 3);
        assert_eq!(
            out[0]["candidates"][0]["content"]["parts"][0]["functionCall"],
            json!({"name": "weather", "args": {"city": "seoul"}})
        );
        assert_eq!(
            out[1]["candidates"][0]["content"]["parts"][0]["functionCall"],
            json!({"name": "clock", "args": {}})
        );
        assert_eq!(out[2]["candidates"][0]["finishReason"], "STOP");
    }

    #[test]
    fn reasoning_signature_is_emitted_as_thought_signature_part() {
        let mut r = GeminiChunkRenderer::new("m".into(), 1);
        let mut frames = r.render(CodexEvent::ReasoningSignature("SIG".into()));
        frames.extend(r.render(CodexEvent::Completed { usage: None }));
        let out = payloads(frames);
        assert_eq!(
            out[0]["candidates"][0]["content"]["parts"][0]["thoughtSignature"],
            "SIG"
        );
    }

    #[test]
    fn created_event_overrides_response_id() {
        let mut r = GeminiChunkRenderer::new("m".into(), 1);
        r.render(CodexEvent::Created {
            response_id: "resp-xyz".into(),
        });
        r.render(CodexEvent::TextDelta("x".into()));
        let out = payloads(r.render(CodexEvent::Completed { usage: None }));
        assert_eq!(out[0]["responseId"], "resp-xyz");
    }
}
