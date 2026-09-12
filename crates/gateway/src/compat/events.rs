use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cached_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_write_tokens: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CodexEvent {
    Created {
        response_id: String,
    },
    TextDelta(String),
    ReasoningDelta(String),
    /// Opaque provider-side reasoning marker. Preserved rather than parsed so it
    /// can be handed back in whichever shape the client surface expects.
    ReasoningSignature(String),
    /// Upstream reports the reasoning was redacted (Devin
    /// `thinking_redacted`). Preserved as an explicit state event so surfaces
    /// can decide how to represent it instead of silently dropping it.
    ReasoningRedacted,
    ToolCallBegin {
        output_index: u64,
        call_id: String,
        name: String,
    },
    ToolArgsDelta {
        output_index: u64,
        delta: String,
    },
    OutputLimitReached,
    Completed {
        usage: Option<Usage>,
    },
    Failed {
        message: String,
    },
}

#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    data: Vec<u8>,
    after_cr: bool,
}

impl SseParser {
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<CodexEvent>) {
        let mut frames = Vec::new();
        self.push_raw_data(chunk, &mut frames);
        for frame in frames {
            Self::decode(&frame, out);
        }
    }

    pub fn finish(&mut self, out: &mut Vec<CodexEvent>) {
        let mut frames = Vec::new();
        self.finish_raw_data(&mut frames);
        for frame in frames {
            Self::decode(&frame, out);
        }
    }

    pub fn push_raw_data(&mut self, chunk: &[u8], out: &mut Vec<Vec<u8>>) {
        for &byte in chunk {
            if self.after_cr && byte == b'\n' {
                self.after_cr = false;
                continue;
            }
            self.after_cr = byte == b'\r';
            if byte == b'\r' || byte == b'\n' {
                let line = std::mem::take(&mut self.buf);
                self.consume_line(&line, out);
            } else {
                self.buf.push(byte);
            }
        }
    }

    pub fn finish_raw_data(&mut self, out: &mut Vec<Vec<u8>>) {
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            self.consume_line(&line, out);
        }
        self.consume_line(b"", out);
        self.after_cr = false;
    }

    fn consume_line(&mut self, line: &[u8], out: &mut Vec<Vec<u8>>) {
        if line.is_empty() {
            if !self.data.is_empty() {
                self.data.pop();
                out.push(std::mem::take(&mut self.data));
            }
        } else if let Some(payload) = line.strip_prefix(b"data:") {
            self.data
                .extend_from_slice(payload.strip_prefix(b" ").unwrap_or(payload));
            self.data.push(b'\n');
        } else if line == b"data" {
            self.data.push(b'\n');
        }
    }

    fn decode(payload: &[u8], out: &mut Vec<CodexEvent>) {
        if payload == b"[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_slice::<Value>(payload) else {
            return;
        };
        if value.get("type").and_then(Value::as_str) == Some("response.incomplete")
            && value.pointer("/response/error").is_none_or(Value::is_null)
            && value.pointer("/response/incomplete_details/reason").and_then(Value::as_str)
                == Some("max_output_tokens")
        {
            out.push(CodexEvent::OutputLimitReached);
            out.push(CodexEvent::Completed {
                usage: value.pointer("/response/usage").map(parse_usage),
            });
            return;
        }
        if let Some(event) = classify(&value) {
            out.push(event);
        }
    }
}

fn classify(value: &Value) -> Option<CodexEvent> {
    match value.get("type").and_then(Value::as_str)? {
        "response.created" => Some(CodexEvent::Created {
            response_id: value
                .get("response")
                .and_then(|r| r.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        "response.output_text.delta" => value
            .get("delta")
            .and_then(Value::as_str)
            .map(|d| CodexEvent::TextDelta(d.to_string())),
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => value
            .get("delta")
            .and_then(Value::as_str)
            .map(|d| CodexEvent::ReasoningDelta(d.to_string())),
        "response.output_item.added" => {
            let item = value.get("item")?;
            if item.get("type").and_then(Value::as_str) != Some("function_call") {
                return None;
            }
            Some(CodexEvent::ToolCallBegin {
                output_index: value
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                call_id: item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        }
        "response.function_call_arguments.delta" => Some(CodexEvent::ToolArgsDelta {
            output_index: value
                .get("output_index")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            delta: value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        "response.completed" => Some(CodexEvent::Completed {
            usage: value
                .get("response")
                .and_then(|r| r.get("usage"))
                .map(parse_usage),
        }),
        "response.failed" | "response.incomplete" | "response.cancelled" => Some(CodexEvent::Failed {
            message: value
                .get("response")
                .and_then(|r| r.get("error"))
                .and_then(|e| e.get("message"))
                .or_else(|| value.pointer("/response/incomplete_details/reason"))
                .or_else(|| value.pointer("/response/status"))
                .and_then(Value::as_str)
                .unwrap_or("upstream response failed")
                .to_string(),
        }),
        "error" => Some(CodexEvent::Failed {
            message: value
                .get("message")
                .or_else(|| value.get("error").and_then(|e| e.get("message")))
                .and_then(Value::as_str)
                .unwrap_or("upstream error")
                .to_string(),
        }),
        _ => None,
    }
}

fn parse_usage(usage: &Value) -> Usage {
    let prompt_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion_tokens = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(prompt_tokens + completion_tokens),
        cached_tokens: usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        reasoning_tokens: usage
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_write_tokens: 0,
    }
}
