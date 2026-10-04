use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cached_tokens: u64,
    pub cached_tokens_known: bool,
    pub reasoning_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_tokens_known: bool,
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

/// Upper bound on one accumulated SSE line (a line that has not been
/// terminated yet). The cap applies to accumulation only, never to the size of
/// an incoming chunk: a chunk carrying many complete frames may exceed it and
/// is still accepted.
pub const MAX_SSE_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Upper bound on one accumulated SSE frame: the `data:` payloads joined for a
/// single event, before its terminating blank line arrives.
pub const MAX_SSE_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// Which accumulation limit a parser breached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseLimitError {
    Line,
    Frame,
}

impl SseLimitError {
    pub fn message(self) -> &'static str {
        match self {
            SseLimitError::Line => "upstream SSE line exceeds the size limit",
            SseLimitError::Frame => "upstream SSE event exceeds the size limit",
        }
    }
}

#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    data: Vec<u8>,
    after_cr: bool,
    failure: Option<SseLimitError>,
}

impl SseParser {
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<CodexEvent>) {
        if self.failure.is_some() {
            return;
        }
        let mut frames = Vec::new();
        let result = self.push_raw_data(chunk, &mut frames);
        for frame in frames {
            Self::decode(&frame, out);
        }
        if let Err(error) = result {
            out.push(CodexEvent::Failed {
                message: error.message().to_string(),
            });
        }
    }

    pub fn finish(&mut self, out: &mut Vec<CodexEvent>) {
        let mut frames = Vec::new();
        let result = self.finish_raw_data(&mut frames);
        for frame in frames {
            Self::decode(&frame, out);
        }
        if let Err(error) = result {
            out.push(CodexEvent::Failed {
                message: error.message().to_string(),
            });
        }
    }

    /// Feeds raw bytes and reports the first accumulation breach so raw-data
    /// callers (`ProtocolParser`) can surface it. Once breached the parser
    /// stops accumulating, and later calls report `Ok(())` so a single failure
    /// is emitted exactly once instead of on every remaining chunk.
    pub fn push_raw_data(
        &mut self,
        chunk: &[u8],
        out: &mut Vec<Vec<u8>>,
    ) -> Result<(), SseLimitError> {
        if self.failure.is_some() {
            return Ok(());
        }
        for &byte in chunk {
            if self.after_cr && byte == b'\n' {
                self.after_cr = false;
                continue;
            }
            self.after_cr = byte == b'\r';
            if byte == b'\r' || byte == b'\n' {
                let line = std::mem::take(&mut self.buf);
                if let Err(error) = self.consume_line(&line, out) {
                    return Err(self.record_failure(error));
                }
            } else {
                self.buf.push(byte);
                if self.buf.len() > MAX_SSE_LINE_BYTES {
                    return Err(self.record_failure(SseLimitError::Line));
                }
            }
        }
        Ok(())
    }

    pub fn finish_raw_data(&mut self, out: &mut Vec<Vec<u8>>) -> Result<(), SseLimitError> {
        if self.failure.is_some() {
            return Ok(());
        }
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            if let Err(error) = self.consume_line(&line, out) {
                return Err(self.record_failure(error));
            }
        }
        if let Err(error) = self.consume_line(b"", out) {
            return Err(self.record_failure(error));
        }
        self.after_cr = false;
        Ok(())
    }

    /// The accumulation breach observed so far, if any.
    pub fn failure(&self) -> Option<&SseLimitError> {
        self.failure.as_ref()
    }

    fn record_failure(&mut self, error: SseLimitError) -> SseLimitError {
        self.failure = Some(error);
        // Release the buffers immediately: a breached stream must not keep the
        // memory it already accumulated while the failure works downstream.
        self.buf.clear();
        self.data.clear();
        error
    }

    fn consume_line(&mut self, line: &[u8], out: &mut Vec<Vec<u8>>) -> Result<(), SseLimitError> {
        if line.is_empty() {
            if !self.data.is_empty() {
                self.data.pop();
                out.push(std::mem::take(&mut self.data));
            }
        } else if let Some(payload) = line.strip_prefix(b"data:") {
            self.data
                .extend_from_slice(payload.strip_prefix(b" ").unwrap_or(payload));
            self.data.push(b'\n');
            if self.data.len() > MAX_SSE_FRAME_BYTES {
                return Err(SseLimitError::Frame);
            }
        } else if line == b"data" {
            self.data.push(b'\n');
            if self.data.len() > MAX_SSE_FRAME_BYTES {
                return Err(SseLimitError::Frame);
            }
        }
        Ok(())
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
        cached_tokens_known: usage.pointer("/input_tokens_details/cached_tokens").and_then(Value::as_u64).is_some(),
        cache_write_tokens_known: false,
    }
}

#[cfg(test)]
mod bounded_accumulation_tests {
    use super::*;

    #[test]
    fn push_raw_data_reports_a_line_beyond_the_cap() {
        let mut parser = SseParser::default();
        let mut frames = Vec::new();
        let oversized = vec![b'a'; MAX_SSE_LINE_BYTES + 1];
        assert_eq!(
            parser.push_raw_data(&oversized, &mut frames),
            Err(SseLimitError::Line)
        );
        assert!(frames.is_empty());
        assert_eq!(parser.failure(), Some(&SseLimitError::Line));
        // The parser stops accumulating: later chunks neither grow the buffer
        // nor report the failure again (exactly one failure is ever emitted).
        assert_eq!(parser.push_raw_data(b"data: {}\n\n", &mut frames), Ok(()));
        assert!(frames.is_empty());
        assert!(parser.buf.is_empty() && parser.data.is_empty());
    }

    #[test]
    fn push_raw_data_reports_a_frame_beyond_the_cap() {
        // One `data:` line at a time stays under the per-line cap; the joined
        // frame is what exceeds the frame cap.
        let mut line = b"data: ".to_vec();
        line.extend(std::iter::repeat(b'a').take(1024 * 1024));
        line.push(b'\n');

        let mut parser = SseParser::default();
        let mut frames = Vec::new();
        let mut observed = None;
        for _ in 0..(MAX_SSE_FRAME_BYTES / (1024 * 1024) + 2) {
            if let Err(error) = parser.push_raw_data(&line, &mut frames) {
                observed = Some(error);
                break;
            }
        }
        assert_eq!(observed, Some(SseLimitError::Frame));
        assert_eq!(parser.failure(), Some(&SseLimitError::Frame));
    }

    #[test]
    fn push_raw_data_accepts_a_chunk_holding_many_valid_frames() {
        // A chunk may contain many valid frames and is never rejected for its
        // own size (the retracted revision-1 bug capped the incoming chunk).
        let frames_in: Vec<Vec<u8>> = (0..4096)
            .map(|i| {
                format!(
                    "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{i}\"}}\n\n"
                )
                .into_bytes()
            })
            .collect();
        let chunk: Vec<u8> = frames_in.concat();
        assert!(chunk.len() > 64 * 1024, "fixture must be substantial");

        let mut parser = SseParser::default();
        let mut frames = Vec::new();
        assert_eq!(parser.push_raw_data(&chunk, &mut frames), Ok(()));
        assert_eq!(frames.len(), 4096);
        assert!(parser.failure().is_none());
    }
}

#[cfg(test)]
mod cache_presence_tests {
    use super::*;

    #[test]
    fn cache_presence_distinguishes_response_usage_values() {
        for value in [None, Some(0), Some(17)] {
            // Given optional Responses cache details.
            let mut wire = serde_json::json!({"input_tokens": 30, "output_tokens": 2});
            if let Some(value) = value {
                wire["input_tokens_details"] = serde_json::json!({"cached_tokens": value});
            }
            // When usage is normalized.
            let usage = parse_usage(&wire);
            // Then absence is not a reported zero.
            assert_eq!(usage.cached_tokens, value.unwrap_or(0));
            assert_eq!(usage.cached_tokens_known, value.is_some());
            assert!(!usage.cache_write_tokens_known);
        }
    }
}
