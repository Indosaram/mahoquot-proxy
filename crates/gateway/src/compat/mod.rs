pub mod claude;
pub mod cursor;
mod cursor_proto;
pub mod devin;
pub mod devin_proto;
pub mod zcode;

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
mod tool_schema;

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
pub use gemini::{openai_to_antigravity, openai_to_antigravity_with_replay, GeminiDecoder};
pub use request::{
    extract_model, openai_to_codex, openai_to_codex_with_cache_key, TranslateError,
    TranslatedRequest,
};
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
            if matches!(
                protocol,
                Protocol::Kiro | Protocol::Cursor | Protocol::Devin
            ) || looks_like_sse(&first) =>
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
    let mut raw = Vec::new();
    extend_nonstream_bounded(&mut raw, &first)?;
    while let Some(chunk) = stream.next().await {
        extend_nonstream_bounded(&mut raw, &chunk.map_err(|e| e.to_string())?)?;
    }
    Ok(raw)
}

/// Upper bound on a total non-streaming upstream body buffered in memory. The
/// streaming path never buffers; only the collect path can, so it is bounded
/// here. Request-body support (512 MiB) is unrelated and unchanged.
pub const MAX_NONSTREAM_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// Appends `chunk` to the buffered non-streaming `raw` body, refusing a total
/// beyond [`MAX_NONSTREAM_RESPONSE_BYTES`]. Every parsed event is derived from
/// `raw`, so bounding `raw` bounds the accumulated event list too.
pub(crate) fn extend_nonstream_bounded(raw: &mut Vec<u8>, chunk: &[u8]) -> Result<(), String> {
    if raw.len().saturating_add(chunk.len()) > MAX_NONSTREAM_RESPONSE_BYTES {
        return Err(format!(
            "upstream non-streaming response exceeds the {MAX_NONSTREAM_RESPONSE_BYTES}-byte limit"
        ));
    }
    raw.extend_from_slice(chunk);
    Ok(())
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
    pub cursor_reply: Option<tokio::sync::mpsc::Sender<Bytes>>,
    pub replay: Option<signature_ledger::ReplayScope>,
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

    /// Builds the parser a relayed reply is decoded with. An Antigravity
    /// upstream emits the `thoughtSignature` that must come back on the next
    /// turn, so the decoder writes it into the same scope the request was
    /// translated with.
    fn with_session(session: &ProtocolSession) -> Self {
        Self::with_parts(
            session.protocol,
            session.cursor_reply.clone(),
            session.replay.clone(),
        )
    }

    fn with_cursor_reply(
        protocol: Protocol,
        cursor_reply: Option<tokio::sync::mpsc::Sender<Bytes>>,
    ) -> Self {
        Self::with_parts(protocol, cursor_reply, None)
    }

    fn with_parts(
        protocol: Protocol,
        cursor_reply: Option<tokio::sync::mpsc::Sender<Bytes>>,
        replay: Option<signature_ledger::ReplayScope>,
    ) -> Self {
        Self {
            sse: SseParser::default(),
            gemini: match protocol {
                Protocol::Codex => None,
                Protocol::Antigravity => Some(match replay {
                    Some(replay) => gemini::GeminiDecoder::with_replay(replay),
                    None => gemini::GeminiDecoder::new(),
                }),
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
            let result = self.sse.push_raw_data(chunk, &mut frames);
            for frame in frames {
                decoder.decode(&frame, events);
            }
            push_limit_failure(result, events);
            return;
        }
        match self.gemini.as_mut() {
            None => self.sse.push(chunk, events),
            Some(decoder) => {
                let mut frames = Vec::new();
                let result = self.sse.push_raw_data(chunk, &mut frames);
                for frame in frames {
                    decoder.decode(&frame, events);
                }
                push_limit_failure(result, events);
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
            let result = self.sse.finish_raw_data(&mut frames);
            for frame in frames {
                decoder.decode(&frame, events);
            }
            push_limit_failure(result, events);
            decoder.finish(events);
            return;
        }
        match self.gemini.as_mut() {
            None => self.sse.finish(events),
            Some(decoder) => {
                let mut frames = Vec::new();
                let result = self.sse.finish_raw_data(&mut frames);
                for frame in frames {
                    decoder.decode(&frame, events);
                }
                push_limit_failure(result, events);
                decoder.finish(events);
            }
        }
    }
}

/// Surfaces a raw SSE accumulation breach as a terminal failure event. The
/// parser never reports the same breach twice, so at most one `Failed` is
/// emitted per stream.
fn push_limit_failure(result: Result<(), events::SseLimitError>, events: &mut Vec<CodexEvent>) {
    if let Err(error) = result {
        events.push(CodexEvent::Failed {
            message: error.message().to_string(),
        });
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
    let parser = ProtocolParser::with_session(&session);
    let renderer = match shape {
        ReplyShape::Gemini => StreamRenderer::Gemini(Box::new(match session.replay {
            Some(replay) => {
                GeminiChunkRenderer::with_replay(model, created, replay)
            }
            None => GeminiChunkRenderer::new(model, created),
        })),
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
        parser,
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
            cached_input_tokens_known: usage.cached_tokens_known,
            cache_write_tokens: usage.cache_write_tokens,
            cache_write_tokens_known: usage.cache_write_tokens_known,
            reasoning_tokens: usage.reasoning_tokens,
        });
}

pub async fn collect_stream_with_replies(
    first: Bytes,
    mut stream: UpstreamStream,
    session: ProtocolSession,
) -> Result<
    (
        Vec<u8>,
        Option<crate::usage::ResponseTokenUsage>,
        Option<devin::DevinOutcome>,
    ),
    String,
> {
    let mut parser = ProtocolParser::with_session(&session);
    let mut raw = Vec::new();
    let mut usage: Option<crate::usage::ResponseTokenUsage> = None;
    // Only the events of the chunk under hand are retained. `raw` is the
    // bounded source of truth, so keeping a second, whole-stream copy of the
    // parsed events would double the retained memory for no benefit.
    let mut events = Vec::new();
    extend_nonstream_bounded(&mut raw, &first)?;
    parser.push(&first, &mut events);
    let mut failed = record_collected_events(&mut events, &mut usage);
    while !failed {
        let Some(chunk) = stream.next().await else {
            break;
        };
        let chunk = chunk.map_err(|e| e.to_string())?;
        extend_nonstream_bounded(&mut raw, &chunk)?;
        parser.push(&chunk, &mut events);
        failed = record_collected_events(&mut events, &mut usage);
    }
    // A terminal failure stops the read and drops the upstream instead of
    // consuming the remaining body forever; `finish` is skipped so no
    // `Completed` is published after the failure.
    if !failed {
        parser.finish(&mut events);
        record_collected_events(&mut events, &mut usage);
    }
    let devin_outcome = parser.devin.as_ref().map(|d| d.outcome().clone());
    Ok((raw, usage, devin_outcome))
}

/// Records the latest completion usage and reports whether a terminal failure
/// was observed. Draining the per-chunk events each call keeps the collector
/// from retaining a whole-stream copy of the parsed data.
fn record_collected_events(
    events: &mut Vec<CodexEvent>,
    usage: &mut Option<crate::usage::ResponseTokenUsage>,
) -> bool {
    let mut failed = false;
    for event in events.drain(..) {
        match event {
            CodexEvent::Completed {
                usage: Some(completed),
            } => {
                *usage = Some(crate::usage::ResponseTokenUsage {
                    input_tokens: completed.prompt_tokens,
                    output_tokens: completed.completion_tokens,
                    cached_input_tokens: completed.cached_tokens,
                    cached_input_tokens_known: completed.cached_tokens_known,
                    cache_write_tokens: completed.cache_write_tokens,
                    cache_write_tokens_known: completed.cache_write_tokens_known,
                    reasoning_tokens: completed.reasoning_tokens,
                });
            }
            CodexEvent::Failed { .. } => failed = true,
            _ => {}
        }
    }
    failed
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
    // One response in, one response out: the ledger exists only for this call,
    // so a signature that arrived on the body is re-emitted on it (the Gemini
    // shape needs it back on the functionCall) without outliving the aggregate.
    let replay = signature_ledger::SignatureLedger::in_memory().scope(&model, "");
    let mut parser = ProtocolParser::with_parts(protocol, None, Some(replay.clone()));
    let mut events = Vec::new();
    parser.push(raw, &mut events);
    parser.finish(&mut events);

    let mut aggregator = Aggregator::new(model, created).with_replay(replay);
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

    let tool_calls: Vec<(String, String, String)> = tool_calls_by_index.into_values().collect();

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

#[cfg(test)]
mod bounded_stream_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    /// A source that counts its polls and records its own drop, so a body that
    /// keeps consuming after a terminal failure is directly observable.
    struct CountingSource {
        chunks: VecDeque<Bytes>,
        polls: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }

    impl Stream for CountingSource {
        type Item = reqwest::Result<Bytes>;

        fn poll_next(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            match self.chunks.pop_front() {
                Some(chunk) => Poll::Ready(Some(Ok(chunk))),
                None => Poll::Ready(None),
            }
        }
    }

    impl Drop for CountingSource {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Drives a `streaming_body` that must terminate on its very first chunk,
    /// returning the rendered body text plus the source's poll and drop counts.
    async fn drain_after_terminal_first(first: Bytes, protocol: Protocol) -> (String, usize, usize) {
        let polls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let source = CountingSource {
            chunks: VecDeque::from(vec![Bytes::from_static(
                b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"late\"}\n\n",
            )]),
            polls: Arc::clone(&polls),
            drops: Arc::clone(&drops),
        };
        let body = streaming_body(StreamingBodyParams {
            first,
            upstream: Box::pin(source),
            model: "fixture".into(),
            created: 0,
            include_usage: false,
            shape: ReplyShape::Chat,
            session: ProtocolSession {
                protocol,
                cursor_reply: None,
                replay: None,
            },
            upstream_capture: None,
            devin_outcome: None,
        });
        let collected = http_body_util::BodyExt::collect(body).await.unwrap();
        (
            String::from_utf8_lossy(&collected.to_bytes()).to_string(),
            polls.load(Ordering::SeqCst),
            drops.load(Ordering::SeqCst),
        )
    }

    #[tokio::test]
    async fn an_sse_limit_failure_stops_polling_and_drops_the_upstream() {
        let (text, polls, drops) = drain_after_terminal_first(
            Bytes::from(vec![b'a'; events::MAX_SSE_LINE_BYTES + 1]),
            Protocol::Codex,
        )
        .await;
        assert!(
            text.contains("size limit"),
            "expected a terminal failure frame: {text}"
        );
        assert_eq!(
            polls, 0,
            "the upstream must not be polled after the limit failure"
        );
        assert_eq!(drops, 1, "the upstream must be dropped when the stream ends");
    }

    #[tokio::test]
    async fn a_kiro_frame_limit_failure_stops_polling_and_drops_the_upstream() {
        let mut prelude = (kiro::MAX_KIRO_FRAME_BYTES as u32 + 1).to_be_bytes().to_vec();
        prelude.extend_from_slice(&0u32.to_be_bytes());
        prelude.extend_from_slice(&0u32.to_be_bytes());
        let (text, polls, drops) =
            drain_after_terminal_first(Bytes::from(prelude), Protocol::Kiro).await;
        assert!(text.contains("error"), "expected a terminal error frame: {text}");
        assert_eq!(polls, 0, "the upstream must not be polled after the frame limit");
        assert_eq!(drops, 1, "the upstream must be dropped when the stream ends");
    }

    #[tokio::test]
    async fn a_cursor_frame_limit_failure_stops_polling_and_drops_the_upstream() {
        let mut prelude = vec![0u8];
        prelude.extend_from_slice(&(cursor::MAX_CURSOR_FRAME_BYTES as u32 + 1).to_be_bytes());
        let (text, polls, drops) =
            drain_after_terminal_first(Bytes::from(prelude), Protocol::Cursor).await;
        assert!(text.contains("error"), "expected a terminal error frame: {text}");
        assert_eq!(polls, 0, "the upstream must not be polled after the frame limit");
        assert_eq!(drops, 1, "the upstream must be dropped when the stream ends");
    }

    #[test]
    fn bounded_nonstream_append_allows_exactly_the_cap() {
        let mut raw = vec![0u8; MAX_NONSTREAM_RESPONSE_BYTES - 3];
        assert!(extend_nonstream_bounded(&mut raw, &[1, 2, 3]).is_ok());
        assert_eq!(raw.len(), MAX_NONSTREAM_RESPONSE_BYTES);
    }

    #[test]
    fn bounded_nonstream_append_enforces_the_cap() {
        let mut raw = vec![0u8; MAX_NONSTREAM_RESPONSE_BYTES];
        let error = extend_nonstream_bounded(&mut raw, &[1]).unwrap_err();
        assert!(error.contains("limit"), "{error}");
        assert_eq!(raw.len(), MAX_NONSTREAM_RESPONSE_BYTES);
    }
}
