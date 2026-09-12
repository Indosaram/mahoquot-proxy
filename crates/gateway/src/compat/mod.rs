pub mod claude;
pub mod cursor;
mod cursor_proto;
pub mod devin;
pub mod devin_proto;

#[doc(hidden)]
pub fn cursor_fixture_text(text: &str) -> cursor_proto::AgentServerMessage {
    cursor_proto::AgentServerMessage {
        message: Some(
            cursor_proto::agent_server_message::Message::InteractionUpdate(
                cursor_proto::InteractionUpdate {
                    message: Some(cursor_proto::interaction_update::Message::TextDelta(
                        cursor_proto::TextDeltaUpdate {
                            text: text.to_string(),
                        },
                    )),
                },
            ),
        ),
    }
}

#[doc(hidden)]
pub fn cursor_fixture_turn_end() -> cursor_proto::AgentServerMessage {
    cursor_proto::AgentServerMessage {
        message: Some(
            cursor_proto::agent_server_message::Message::InteractionUpdate(
                cursor_proto::InteractionUpdate {
                    message: Some(cursor_proto::interaction_update::Message::TurnEnded(
                        cursor_proto::TurnEndedUpdate::default(),
                    )),
                },
            ),
        ),
    }
}

#[doc(hidden)]
pub fn cursor_fixture_get_blob(id: u32) -> cursor_proto::AgentServerMessage {
    cursor_proto::AgentServerMessage {
        message: Some(
            cursor_proto::agent_server_message::Message::KvServerMessage(
                cursor_proto::KvServerMessage {
                    id,
                    message: Some(cursor_proto::kv_server_message::Message::GetBlobArgs(
                        cursor_proto::GetBlobArgs { blob_id: vec![1] },
                    )),
                },
            ),
        ),
    }
}

#[doc(hidden)]
pub fn cursor_is_get_blob_reply(frame: &[u8], expected_id: u32) -> bool {
    use prost::Message;
    if frame.len() < 5 {
        return false;
    }
    let Ok(message) = cursor_proto::AgentClientMessage::decode(&frame[5..]) else {
        return false;
    };
    matches!(
        message.message,
        Some(cursor_proto::agent_client_message::Message::KvClientMessage(reply))
            if reply.id == expected_id
                && matches!(reply.message, Some(cursor_proto::kv_client_message::Message::GetBlobResult(_)))
    )
}
pub mod events;
pub mod gemini;
pub mod kiro;
pub mod mimo;
pub mod render;
pub mod request;
pub mod responses;
pub mod signature_ledger;

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Body;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};

use events::{CodexEvent, SseParser};
use render::{Aggregator, ChunkRenderer, GeminiChunkRenderer, DONE_FRAME};

pub use claude::{anthropic_to_openai, estimate_input_tokens, messages_payload};
pub use gemini::{openai_to_antigravity, GeminiDecoder};
pub use request::{extract_model, openai_to_codex, TranslateError, TranslatedRequest};
pub use responses::{
    responses_response, responses_to_openai, ResponsesError, ResponsesStreamRenderer,
};

pub const CODEX_PATH: &str = "/backend-api/codex/responses";

pub type UpstreamStream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// Split an OpenAI `data:` URL into its MIME type and base64 payload.
pub fn split_data_url(url: &str) -> Option<(&str, &str)> {
    let data_url = url.strip_prefix("data:")?;
    let (media_type, data) = data_url.split_once(";base64,")?;
    Some((media_type, data))
}

pub fn looks_like_sse(bytes: &[u8]) -> bool {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .map_or(&[][..], |offset| &bytes[offset..]);
    if start.is_empty() {
        return true;
    }
    [&b"event:"[..], &b"data:"[..], &b": "[..], &b"retry:"[..]]
        .iter()
        .any(|marker| start.starts_with(marker) || marker.starts_with(start))
}

fn preview(bytes: &[u8]) -> String {
    let end = bytes.len().min(80);
    String::from_utf8_lossy(&bytes[..end])
        .replace(['\n', '\r'], " ")
        .trim()
        .to_string()
}

pub async fn open_stream(
    resp: reqwest::Response,
    protocol: Protocol,
) -> Result<(Bytes, UpstreamStream), String> {
    let mut stream: UpstreamStream = Box::pin(resp.bytes_stream());
    match stream.next().await {
        Some(Ok(first))
            if matches!(protocol, Protocol::Kiro | Protocol::Cursor | Protocol::Devin)
                || looks_like_sse(&first) =>
        {
            Ok((first, stream))
        }
        Some(Ok(other)) => Err(format!(
            "upstream body is not an event stream: {}",
            preview(&other)
        )),
        Some(Err(err)) => Err(err.to_string()),
        None => Err("upstream body is not an event stream: empty response".to_string()),
    }
}

pub async fn collect_stream(first: Bytes, mut stream: UpstreamStream) -> Result<Vec<u8>, String> {
    let mut raw = first.to_vec();
    while let Some(chunk) = stream.next().await {
        raw.extend_from_slice(&chunk.map_err(|e| e.to_string())?);
    }
    Ok(raw)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Protocol {
    Codex,
    Antigravity,
    Anthropic,
    Kiro,
    Cursor,
    Devin,
}

pub struct ProtocolSession {
    pub protocol: Protocol,
    pub cursor_reply: Option<tokio::sync::mpsc::UnboundedSender<Bytes>>,
}

struct ProtocolParser {
    sse: SseParser,
    gemini: Option<gemini::GeminiDecoder>,
    anthropic: Option<claude::AnthropicDecoder>,
    kiro: Option<kiro::KiroDecoder>,
    cursor: Option<cursor::CursorDecoder>,
    devin: Option<devin::DevinDecoder>,
}

impl ProtocolParser {
    fn new(protocol: Protocol) -> Self {
        Self::with_cursor_reply(protocol, None)
    }

    fn with_cursor_reply(
        protocol: Protocol,
        cursor_reply: Option<tokio::sync::mpsc::UnboundedSender<Bytes>>,
    ) -> Self {
        Self {
            sse: SseParser::default(),
            gemini: match protocol {
                Protocol::Codex => None,
                Protocol::Antigravity => Some(gemini::GeminiDecoder::new()),
                Protocol::Anthropic => None,
                Protocol::Kiro => None,
                Protocol::Cursor => None,
                Protocol::Devin => None,
            },
            anthropic: (protocol == Protocol::Anthropic).then(claude::AnthropicDecoder::new),
            kiro: (protocol == Protocol::Kiro).then(kiro::KiroDecoder::new),
            cursor: (protocol == Protocol::Cursor).then(|| match cursor_reply {
                Some(sender) => cursor::CursorDecoder::with_reply_sender(sender),
                None => cursor::CursorDecoder::new(),
            }),
            devin: (protocol == Protocol::Devin).then(devin::DevinDecoder::new),
        }
    }

    fn push(&mut self, chunk: &[u8], events: &mut Vec<CodexEvent>) {
        if let Some(decoder) = self.cursor.as_mut() {
            decoder.decode(chunk, events);
            return;
        }
        if let Some(decoder) = self.kiro.as_mut() {
            decoder.decode(chunk, events);
            return;
        }
        if let Some(decoder) = self.devin.as_mut() {
            decoder.decode(chunk, events);
            return;
        }
        if let Some(decoder) = self.anthropic.as_mut() {
            let mut frames = Vec::new();
            self.sse.push_raw_data(chunk, &mut frames);
            for frame in frames {
                decoder.decode(&frame, events);
            }
            return;
        }
        match self.gemini.as_mut() {
            None => self.sse.push(chunk, events),
            Some(decoder) => {
                let mut frames = Vec::new();
                self.sse.push_raw_data(chunk, &mut frames);
                for frame in frames {
                    decoder.decode(&frame, events);
                }
            }
        }
    }

    fn finish(&mut self, events: &mut Vec<CodexEvent>) {
        if let Some(decoder) = self.cursor.as_mut() {
            decoder.finish(events);
            return;
        }
        if let Some(decoder) = self.kiro.as_mut() {
            decoder.finish(events);
            return;
        }
        if let Some(decoder) = self.devin.as_mut() {
            decoder.finish(events);
            return;
        }
        if let Some(decoder) = self.anthropic.as_mut() {
            let mut frames = Vec::new();
            self.sse.finish_raw_data(&mut frames);
            for frame in frames {
                decoder.decode(&frame, events);
            }
            decoder.finish(events);
            return;
        }
        match self.gemini.as_mut() {
            None => self.sse.finish(events),
            Some(decoder) => {
                let mut frames = Vec::new();
                self.sse.finish_raw_data(&mut frames);
                for frame in frames {
                    decoder.decode(&frame, events);
                }
                decoder.finish(events);
            }
        }
    }
}

struct TranslateState {
    upstream: UpstreamStream,
    parser: ProtocolParser,
    renderer: StreamRenderer,
    pending: VecDeque<Bytes>,
    drained: bool,
}

/// Streaming surfaces differ per client protocol: OpenAI chunk objects versus
/// Gemini `candidates` envelopes. Upstream parsing is shared.
enum StreamRenderer {
    OpenAi(Box<ChunkRenderer>),
    Gemini(Box<GeminiChunkRenderer>),
    Anthropic(Box<claude::AnthropicStreamRenderer>),
    Responses(Box<responses::ResponsesStreamRenderer>),
}

impl StreamRenderer {
    fn render(&mut self, event: CodexEvent) -> Vec<Bytes> {
        match self {
            Self::OpenAi(r) => r.render(event),
            Self::Gemini(r) => r.render(event),
            Self::Anthropic(r) => r.render(event),
            Self::Responses(r) => r.render(event),
        }
    }

    fn close_unterminated(&mut self) -> Vec<Bytes> {
        match self {
            Self::OpenAi(r) => r.close_unterminated(),
            Self::Gemini(r) => r.close_unterminated(),
            Self::Anthropic(r) => r.close_unterminated(),
            Self::Responses(r) => r.close_unterminated(),
        }
    }

    fn terminated(&self) -> bool {
        match self {
            Self::OpenAi(r) => r.terminated(),
            Self::Gemini(r) => r.terminated(),
            Self::Anthropic(r) => r.terminated(),
            Self::Responses(r) => r.terminated(),
        }
    }
}

pub struct StreamingBodyParams {
    pub first: Bytes,
    pub upstream: UpstreamStream,
    pub model: String,
    pub created: i64,
    pub include_usage: bool,
    pub shape: ReplyShape,
    pub session: ProtocolSession,
    pub upstream_capture: Option<Arc<std::sync::Mutex<Option<crate::usage::ResponseTokenUsage>>>>,
    pub devin_outcome: Option<Arc<std::sync::Mutex<Option<devin::DevinOutcome>>>>,
}

pub fn streaming_body(params: StreamingBodyParams) -> Body {
    let StreamingBodyParams {
        first,
        upstream,
        model,
        created,
        include_usage,
        shape,
        session,
        upstream_capture,
        devin_outcome,
    } = params;
    let renderer = match shape {
        ReplyShape::Gemini => {
            StreamRenderer::Gemini(Box::new(GeminiChunkRenderer::new(model, created)))
        }
        ReplyShape::Anthropic => StreamRenderer::Anthropic(Box::new(
            claude::AnthropicStreamRenderer::new(model, created),
        )),
        ReplyShape::Responses => StreamRenderer::Responses(Box::new(
            responses::ResponsesStreamRenderer::new(model, created),
        )),
        _ => StreamRenderer::OpenAi(Box::new(ChunkRenderer::new(model, created, include_usage))),
    };
    let mut state = TranslateState {
        upstream,
        parser: ProtocolParser::with_cursor_reply(session.protocol, session.cursor_reply),
        renderer,
        pending: VecDeque::new(),
        drained: false,
    };
    let mut events = Vec::new();
    state.parser.push(&first, &mut events);
    for event in events {
        handle_stream_event(event, &mut state, upstream_capture.as_ref());
    }

    Body::from_stream(futures::stream::unfold(
        (state, upstream_capture, devin_outcome),
        |(mut state, upstream_capture, devin_outcome)| async move {
            loop {
                if let Some(frame) = state.pending.pop_front() {
                    return Some((
                        Ok::<Bytes, std::io::Error>(frame),
                        (state, upstream_capture, devin_outcome),
                    ));
                }
                if state.drained || state.renderer.terminated() {
                    capture_devin_outcome(&state.parser, devin_outcome.as_ref());
                    return None;
                }
                match state.upstream.next().await {
                    Some(Ok(chunk)) => {
                        let mut events = Vec::new();
                        state.parser.push(&chunk, &mut events);
                        for event in events {
                            handle_stream_event(event, &mut state, upstream_capture.as_ref());
                        }
                    }
                    Some(Err(err)) => {
                        state.drained = true;
                        state
                            .pending
                            .extend(error_frames(&mut state.renderer, &err.to_string()));
                        capture_devin_outcome(&state.parser, devin_outcome.as_ref());
                    }
                    None => {
                        state.drained = true;
                        let mut events = Vec::new();
                        state.parser.finish(&mut events);
                        for event in events {
                            handle_stream_event(event, &mut state, upstream_capture.as_ref());
                        }
                        state.pending.extend(state.renderer.close_unterminated());
                        capture_devin_outcome(&state.parser, devin_outcome.as_ref());
                    }
                }
            }
        },
    ))
}

fn capture_devin_outcome(
    parser: &ProtocolParser,
    devin_outcome: Option<&Arc<std::sync::Mutex<Option<devin::DevinOutcome>>>>,
) {
    if let (Some(target), Some(devin)) = (devin_outcome, parser.devin.as_ref()) {
        *target.lock().unwrap_or_else(|p| p.into_inner()) = Some(devin.outcome().clone());
    }
}

fn handle_stream_event(
    event: CodexEvent,
    state: &mut TranslateState,
    upstream_capture: Option<&Arc<std::sync::Mutex<Option<crate::usage::ResponseTokenUsage>>>>,
) {
    capture_stream_usage(&event, upstream_capture);
    state.pending.extend(state.renderer.render(event));
}

fn capture_stream_usage(
    event: &CodexEvent,
    capture: Option<&Arc<std::sync::Mutex<Option<crate::usage::ResponseTokenUsage>>>>,
) {
    let (Some(capture), CodexEvent::Completed { usage: Some(usage) }) = (capture, event) else {
        return;
    };
    *capture
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
        Some(crate::usage::ResponseTokenUsage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
            cached_input_tokens: usage.cached_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            reasoning_tokens: usage.reasoning_tokens,
        });
}

pub async fn collect_stream_with_replies(
    first: Bytes,
    mut stream: UpstreamStream,
    session: ProtocolSession,
) -> Result<(Vec<u8>, Option<crate::usage::ResponseTokenUsage>, Option<devin::DevinOutcome>), String> {
    let mut parser = ProtocolParser::with_cursor_reply(session.protocol, session.cursor_reply);
    let mut raw = first.to_vec();
    let mut events = Vec::new();
    parser.push(&first, &mut events);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        parser.push(&chunk, &mut events);
        raw.extend_from_slice(&chunk);
    }
    parser.finish(&mut events);
    let usage = events.iter().rev().find_map(|event| match event {
        CodexEvent::Completed {
            usage: Some(completed),
        } => Some(crate::usage::ResponseTokenUsage {
            input_tokens: completed.prompt_tokens,
            output_tokens: completed.completion_tokens,
            cached_input_tokens: completed.cached_tokens,
            cache_write_tokens: completed.cache_write_tokens,
            reasoning_tokens: completed.reasoning_tokens,
        }),
        _ => None,
    });
    let devin_outcome = parser.devin.as_ref().map(|d| d.outcome().clone());
    Ok((raw, usage, devin_outcome))
}

fn error_frames(renderer: &mut StreamRenderer, message: &str) -> Vec<Bytes> {
    if renderer.terminated() {
        return Vec::new();
    }
    renderer.render(CodexEvent::Failed {
        message: message.to_string(),
    })
}

/// Client-visible JSON shape for a non-streaming reply. The upstream parsing is
/// identical for all three; only the final envelope differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyShape {
    Chat,
    TextCompletion,
    Gemini,
    Anthropic,
    Responses,
}

pub fn aggregate(
    raw: &[u8],
    model: String,
    created: i64,
    protocol: Protocol,
    shape: ReplyShape,
) -> Result<Value, String> {
    let mut parser = ProtocolParser::new(protocol);
    let mut events = Vec::new();
    parser.push(raw, &mut events);
    parser.finish(&mut events);

    let mut aggregator = Aggregator::new(model, created);
    for event in events {
        aggregator.push(event);
    }
    match aggregator.failure() {
        Some(message) => Err(message.to_string()),
        None => match shape {
            ReplyShape::Chat => Ok(aggregator.into_completion()),
            ReplyShape::TextCompletion => Ok(aggregator.into_text_completion()),
            ReplyShape::Gemini => Ok(aggregator.into_gemini()),
            ReplyShape::Anthropic => Ok(aggregator.into_completion()),
            ReplyShape::Responses => aggregator.into_responses(),
        },
    }
}

pub fn anthropic_response(
    raw: &[u8],
    model: &str,
    created: i64,
    protocol: Protocol,
    stream: bool,
) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;

    let mut parser = ProtocolParser::new(protocol);
    let mut events = Vec::new();
    parser.push(raw, &mut events);
    parser.finish(&mut events);

    for event in &events {
        if let CodexEvent::Failed { message } = event {
            return (
                StatusCode::BAD_GATEWAY,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": message,
                    }
                })
                .to_string(),
            )
                .into_response();
        }
    }

    let id = format!("msg_{created}");

    if stream {
        let (frames, _) = claude::render_anthropic_stream(&events, &id, model);
        let body = frames.concat();
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/event-stream"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            body,
        )
            .into_response();
    }

    let mut text = String::new();
    let mut thinking_text = String::new();
    let mut usage: Option<events::Usage> = None;
    let mut tool_calls_by_index: std::collections::BTreeMap<u64, (String, String, String)> =
        std::collections::BTreeMap::new();
    let mut finish = "stop";
    let mut reasoning_signature: Option<String> = None;

    for event in events {
        match event {
            CodexEvent::TextDelta(t) => text.push_str(&t),
            CodexEvent::ReasoningDelta(r) => thinking_text.push_str(&r),
            CodexEvent::ReasoningSignature(sig) => reasoning_signature = Some(sig),
            CodexEvent::Completed { usage: u } => usage = u,
            CodexEvent::OutputLimitReached => finish = "length",
            CodexEvent::ToolCallBegin {
                call_id,
                name,
                output_index,
            } => {
                if finish != "length" {
                    finish = "tool_calls";
                }
                tool_calls_by_index.insert(output_index, (call_id, name, String::new()));
            }
            CodexEvent::ToolArgsDelta {
                delta,
                output_index,
            } => {
                if let Some(tool) = tool_calls_by_index.get_mut(&output_index) {
                    tool.2.push_str(&delta);
                } else if let Some(last) = tool_calls_by_index.values_mut().last() {
                    last.2.push_str(&delta);
                }
            }
            _ => {}
        }
    }

    let tool_calls: Vec<(String, String, String)> =
        tool_calls_by_index.into_values().collect();

    let payload = claude::messages_payload_full(
        &id,
        model,
        &text,
        &tool_calls,
        finish,
        usage.as_ref(),
        if thinking_text.is_empty() {
            None
        } else {
            Some(&thinking_text)
        },
        reasoning_signature.as_deref(),
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        payload.to_string(),
    )
        .into_response()
}

pub fn error_stream_body(message: &str) -> Body {
    let mut buf = Vec::with_capacity(160);
    buf.extend_from_slice(b"data: ");
    let payload = json!({"error": {"message": message, "type": "upstream_error"}});
    if serde_json::to_writer(&mut buf, &payload).is_ok() {
        buf.extend_from_slice(b"\n\n");
        buf.extend_from_slice(DONE_FRAME);
    }
    Body::from(buf)
}
