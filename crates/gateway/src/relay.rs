// allow: SIZE_OK — single-loop failover relay state machine with auth refresh, retry, and in-flight tracking


use std::hash::Hasher;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};


use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use mahoquot_registry::{ModelCapability, ProviderBinding, ProviderId};
use mahoquot_types::{Health, Outcome, PoolMember, SessionHint};

use crate::account::AccountMember;
use crate::compat;
use crate::state::AppState;
use crate::usage::{parse_claude_headers, parse_codex_headers};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RelayMode {
    Native,
    OpenAiCompat,
    Anthropic,
    GeminiNative,
    /// Same upstream request as `OpenAiCompat`; only the reply envelope differs.
    LegacyCompletions,
    /// Non-streaming Gemini token count; the upstream reply is passed through
    /// verbatim so its `promptTokensDetails` reach the client unmodified.
    GeminiCountTokens,
    /// Image generation is relayed without chat translation after a binding
    /// capability gate selects a provider-specific image binding.
    Image,
    /// OpenAI Responses API surface.
    Responses,
}

struct FinalFailure {
    status: StatusCode,
    content_type: Option<String>,
    body: Bytes,
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

pub fn subscribe_finalizer(
    state: &AppState,
    account_or_event: &str,
) -> tokio::sync::mpsc::Receiver<()> {
    state.subscribe_finalizer(account_or_event)
}

fn get_preflight_deadline(headers: &HeaderMap) -> std::time::Duration {
    let ms = headers
        .get("x-test-preflight-deadline-ms")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(10_000);
    std::time::Duration::from_millis(ms)
}

struct StreamedOutcome {
    state: Arc<AppState>,
    member: Arc<AccountMember>,
    credential_token: String,
    event_id: String,
    occurred_at_ms: i64,
    provider: String,
    model: Option<String>,
    key_identifier: Option<String>,
    scoped_entry: Option<Arc<crate::state::ScopedKeyEntry>>,
    upstream_capture: Option<Arc<std::sync::Mutex<Option<crate::usage::ResponseTokenUsage>>>>,
    devin_outcome: Option<Arc<std::sync::Mutex<Option<compat::devin::DevinOutcome>>>>,
    status: u16,
    started: std::time::Instant,
    bytes_in: usize,
}

/// Shared state of a counting body: byte counter, bounded head/tail windows
/// for usage parsing, and the pending record — all under one mutex, finalized
/// exactly once at end of stream or on early drop.
struct StreamCapture {
    bytes_out: u64,
    head_tail: crate::usage::HeadTailCapture,
    outcome: Option<StreamedOutcome>,
}

impl StreamCapture {
    fn observe(&mut self, data: &[u8]) {
        self.bytes_out += data.len() as u64;
        self.head_tail.push(data);
    }

    /// Spawn the deferred record exactly once; safe to call from `Drop`.
    fn finalize(&mut self) {
        let Some(outcome) = self.outcome.take() else {
            return;
        };
        let token_usage = outcome
            .upstream_capture
            .as_ref()
            .and_then(|capture| {
                *capture
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
            })
            .or_else(|| {
                let (head, tail) = self.head_tail.parts();
                crate::usage::extract_response_token_usage(head, tail)
            });
        let tokens = token_usage.map(crate::usage::ResponseTokenUsage::total_tokens);
        let bytes_out = self.bytes_out;
        let state = outcome.state;

        let mut success = true;
        let mut status = outcome.status;

        if outcome.provider == "devin" {
            // Defect 1: Default to failure unless verified EOF terminal success!
            success = false;
            let mut terminal_success = false;
            let mut connect_error = None;

            // Find matching member in current pool ONLY IF it has the exact same credential and runtime identity!
            // (Preserves same-credential mutable runtime identity across catalog generations,
            // while rotated credentials are never contaminated).
            let current_pool_member = state
                .pool
                .load()
                .members
                .iter()
                .find(|m| {
                    m.id() == outcome.member.id()
                        && m.access_token() == outcome.credential_token
                        && (Arc::ptr_eq(&outcome.member, m) || outcome.member.shares_runtime_identity(m))
                })
                .cloned();

            if let Some(cell) = outcome.devin_outcome.as_ref() {
                let guard = cell.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(devin) = guard.as_ref() {
                    if let Some(ref code) = devin.error_code {
                        connect_error = Some((code.clone(), devin.error_message.clone()));
                    } else if devin.terminated {
                        terminal_success = true;
                    }
                }
            }

            if terminal_success {
                success = true;
                outcome.member.record_ok();
                if let Some(ref current) = current_pool_member {
                    if !Arc::ptr_eq(&outcome.member.ok_count, &current.ok_count) {
                        current.record_ok();
                    }
                    state.monitor.clear_error(current.id());
                    state.scheduler.record_success(current.id(), &state.pool.load().members);
                    state.router.feedback(current.id(), Outcome::Success);
                }
                state.metrics.served.fetch_add(1, Ordering::Relaxed);
            } else if let Some((code, msg)) = connect_error {
                // Connect error in EndStream: fail exactly once, preserve status and health
                status = connect_code_to_http_status(&code).as_u16();
                outcome.member.record_fail();
                if let Some(ref current) = current_pool_member {
                    if !Arc::ptr_eq(&outcome.member.fail_count, &current.fail_count) {
                        current.record_fail();
                    }
                    state.monitor.record_error(
                        current.id(),
                        status,
                        msg.as_deref().unwrap_or("upstream error"),
                    );
                    if code == "resource_exhausted" {
                        let now_ms = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
                            .unwrap_or(0);
                        let until = cooldown_deadline_ms(now_ms, 300);
                        current.set_health(Health::Cooldown { until_unix_ms: until });
                    } else if code == "unauthenticated" {
                        current.set_health(Health::AuthFailed);
                    }
                }
                if code == "resource_exhausted" {
                    let now_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
                        .unwrap_or(0);
                    let until = cooldown_deadline_ms(now_ms, 300);
                    outcome.member.set_health(Health::Cooldown { until_unix_ms: until });
                } else if code == "unauthenticated" {
                    outcome.member.set_health(Health::AuthFailed);
                }
            } else {
                // Client cancellation / premature disconnect / incomplete stream
                outcome.member.record_fail();
                if let Some(ref current) = current_pool_member {
                    if !Arc::ptr_eq(&outcome.member.fail_count, &current.fail_count) {
                        current.record_fail();
                    }
                    state.monitor.record_error(
                        current.id(),
                        if status == 200 { 499 } else { status },
                        "stream terminated prematurely or canceled by client",
                    );
                }
                if status == 200 {
                    status = 499;
                }
            }
        }

        tokio::spawn(async move {
            let record = OutcomeRecord {
                event_id: &outcome.event_id,
                occurred_at_ms: outcome.occurred_at_ms,
                provider: &outcome.provider,
                account: Some(outcome.member.id()),
                model: outcome.model.as_deref(),
                key_identifier: outcome.key_identifier.as_deref(),
                scoped_entry: outcome.scoped_entry.as_deref(),
                status,
                success,
                elapsed_ms: outcome.started.elapsed().as_millis() as u64,
                bytes_in: outcome.bytes_in,
                bytes_out,
                tokens,
                token_usage,
            };
            record_request_outcome(&state, record).await;
            state.notify_finalizer(Some(outcome.member.id()), &outcome.event_id);
        });
    }
}

type SharedCapture = Arc<std::sync::Mutex<StreamCapture>>;

fn with_capture(shared: &SharedCapture, f: impl FnOnce(&mut StreamCapture)) {
    let mut guard = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard);
}

/// Stream wrapper over the success response body: counts delivered bytes,
/// captures head/tail windows for usage parsing, and spawns the request
/// record at end of stream. Chunks pass through untouched — nothing is
/// buffered beyond the bounded capture windows.
struct CountedStream {
    inner: http_body_util::BodyStream<Body>,
    shared: SharedCapture,
    in_flight: Option<crate::monitor::InFlightGuard>,
}

impl CountedStream {
    fn new(body: Body, outcome: StreamedOutcome, in_flight: crate::monitor::InFlightGuard) -> Self {
        Self {
            in_flight: Some(in_flight),
            inner: http_body_util::BodyStream::new(body),
            shared: SharedCapture::new(std::sync::Mutex::new(StreamCapture {
                bytes_out: 0,
                head_tail: crate::usage::HeadTailCapture::new(),
                outcome: Some(outcome),
            })),
        }
    }
}

impl Drop for CountedStream {
    fn drop(&mut self) {
        with_capture(&self.shared, StreamCapture::finalize);
    }
}

impl futures::Stream for CountedStream {
    type Item = Result<Bytes, axum::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        loop {
            match std::task::ready!(std::pin::Pin::new(&mut self.inner).poll_next(cx)) {
                Some(Ok(frame)) => {
                    if let Some(data) = frame.data_ref() {
                        with_capture(&self.shared, |capture| capture.observe(data));
                        return std::task::Poll::Ready(Some(Ok(data.clone())));
                    }
                }
                Some(Err(error)) => {
                    self.in_flight.take();
                    with_capture(&self.shared, StreamCapture::finalize);
                    return std::task::Poll::Ready(Some(Err(error)));
                }
                None => {
                    self.in_flight.take();
                    with_capture(&self.shared, StreamCapture::finalize);
                    return std::task::Poll::Ready(None);
                }
            }
        }
    }
}

/// Fields of one finalized request record, handed to `record_request_outcome`
/// by the synchronous error paths and the streamed-success finalizer alike.
struct OutcomeRecord<'a> {
    event_id: &'a str,
    occurred_at_ms: i64,
    provider: &'a str,
    account: Option<&'a str>,
    model: Option<&'a str>,
    key_identifier: Option<&'a str>,
    scoped_entry: Option<&'a crate::state::ScopedKeyEntry>,
    status: u16,
    success: bool,
    elapsed_ms: u64,
    bytes_in: usize,
    bytes_out: u64,
    tokens: Option<u64>,
    token_usage: Option<crate::usage::ResponseTokenUsage>,
}

async fn record_request_outcome(state: &AppState, record: OutcomeRecord<'_>) {
    let timestamp = now_unix_secs();
    let token_usage = record.token_usage.unwrap_or_default();
    let total_tokens = token_usage.total_tokens();
    if total_tokens > 0 {
        if let Some(entry) = record.scoped_entry {
            let charged = entry.consume(total_tokens);
            // The atomic is authoritative while the process lives; mirroring it
            // into the settings document is what makes a restart resume from
            // the spent balance instead of handing the allowance back.
            persist_scoped_usage(state, &entry.key.id, charged).await;
        }
    }
    state.history.enqueue(crate::request_history::UsageEvent {
        event_id: record.event_id.to_string(),
        occurred_at_ms: record.occurred_at_ms,
        account_identifier: record.account.unwrap_or("unknown").to_string(),
        provider: record.provider.to_string(),
        model: record.model.unwrap_or("unknown").to_string(),
        key_identifier: record.key_identifier.map(ToString::to_string),
        status_code: record.status,
        succeeded: record.success,
        input_tokens: token_usage.input_tokens,
        output_tokens: token_usage.output_tokens,
        cached_input_tokens: token_usage.cached_input_tokens,
        cache_write_tokens: token_usage.cache_write_tokens,
        reasoning_tokens: token_usage.reasoning_tokens,
        total_tokens: token_usage.total_tokens(),
        latency_ms: record.elapsed_ms,
    });
    state
        .telemetry
        .record_with_account(timestamp, record.provider, record.account, record.success);
    if let (Some(account), Some(token_usage)) = (record.account, record.token_usage) {
        state.telemetry.record_tokens(
            timestamp,
            record.provider,
            account,
            token_usage.input_tokens,
            token_usage.output_tokens,
        );
    }
    let line = serde_json::json!({
        "kind": "request",
        "timestamp": timestamp,
        "provider": record.provider,
        "account": record.account,
        "model": record.model.unwrap_or(""),
        "status": record.status,
        "success": record.success,
        "latency-ms": record.elapsed_ms,
        "bytes-in": record.bytes_in,
        "bytes-out": record.bytes_out,
        "tokens": record.tokens,
    })
    .to_string();
    // The live tail is always fed; file persistence is the only gated part.
    state.log_tail.push(line.clone());
    let settings = state.settings.current();
    if !settings.logging_to_file {
        return;
    }
    let settings = (*settings).clone();
    let _ = tokio::task::spawn_blocking(move || {
        crate::management::observability::append_log_line(&settings, &line);
    })
    .await;
}

/// Mirror a scoped key's live token counter into the settings document.
///
/// Skipped when the persisted value already covers the charge, so a burst of
/// zero-token or already-recorded outcomes does not turn into a disk write.
async fn persist_scoped_usage(state: &AppState, identifier: &str, charged: u64) {
    let already_persisted = state
        .settings
        .current()
        .scoped_api_keys
        .iter()
        .find(|key| key.id == identifier)
        .map(|key| key.token_used);
    match already_persisted {
        Some(persisted) if persisted >= charged => return,
        Some(_) => {}
        None => return,
    }

    let identifier = identifier.to_string();
    let settings = Arc::clone(&state.settings);
    let result = tokio::task::spawn_blocking(move || {
        settings.mutate(|document| {
            if let Some(key) = document
                .scoped_api_keys
                .iter_mut()
                .find(|key| key.id == identifier)
            {
                key.token_used = key.token_used.max(charged);
            }
        })
    })
    .await;
    match result {
        Ok(Err(err)) => tracing::warn!("failed to persist scoped key usage: {err}"),
        Err(err) => tracing::warn!("scoped key usage persistence task failed: {err}"),
        Ok(Ok(_)) => {}
    }
}

struct RelayPlan {
    upstream_path: String,
    body: Bytes,
    /// Client-requested model. This stays unchanged for external response
    /// rewriting; routing and upstream translation use `ResolvedRoute`.
    model: Option<String>,
    mode: RelayMode,
    client_stream: bool,
    include_usage: bool,
    openai_body: Option<serde_json::Value>,
    original_body: Bytes,
}

struct UpstreamTarget {
    url: String,
    body: Bytes,
    protocol: compat::Protocol,
    headers: Option<Vec<(String, String)>>,
}

struct UpstreamExchange {
    response: reqwest::Response,
    cursor_reply: Option<tokio::sync::mpsc::UnboundedSender<Bytes>>,
}

#[derive(Clone, Debug)]
struct ResolvedProviderClass {
    binding: ProviderBinding,
    upstream_model: String,
}

#[derive(Clone, Debug)]
struct ResolvedRoute {
    canonical_model: String,
    #[allow(dead_code)]
    capabilities: std::collections::BTreeSet<ModelCapability>,
    provider_classes: Vec<ResolvedProviderClass>,
}

fn body_with_model(body: &Bytes, model: &str) -> Bytes {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return body.clone();
    };
    let Some(object) = value.as_object_mut() else {
        return body.clone();
    };
    object.insert(
        "model".to_string(),
        serde_json::Value::String(model.to_string()),
    );
    Bytes::from(value.to_string())
}

fn openai_body_with_model(plan: &RelayPlan, model: &str) -> Option<serde_json::Value> {
    let mut body = plan.openai_body.clone()?;
    if let Some(object) = body.as_object_mut() {
        object.insert(
            "model".to_string(),
            serde_json::Value::String(model.to_string()),
        );
    }
    Some(body)
}

fn resolve_target(
    pool: &crate::state::PoolSnapshot,
    member: &AccountMember,
    selected_token: &str,
    plan: &RelayPlan,
    upstream_model: &str,
) -> Result<UpstreamTarget, String> {
    if plan.mode == RelayMode::Image {
        let base = member.upstream_override.as_deref().unwrap_or_default();
        return Ok(UpstreamTarget {
            url: crate::url::join_provider_path(base, &plan.upstream_path),
            body: body_with_model(&plan.original_body, upstream_model),
            protocol: compat::Protocol::Codex,
            headers: None,
        });
    }

    if plan.mode == RelayMode::GeminiCountTokens {
        if member.kind() != crate::account::ProviderKind::Antigravity {
            return Err("gemini-native requests need an antigravity account".to_string());
        }
        let mut gemini: serde_json::Value = serde_json::from_slice(&plan.body)
            .map_err(|e| format!("invalid gemini request: {e}"))?;
        // Probed against cloudcode-pa v1internal:countTokens: it accepts only
        // {"request": GenerateContentRequest}. A bare body, a
        // "generateContentRequest" key, or a sibling model/project field are
        // all rejected as unknown fields.
        if let Some(obj) = gemini.as_object_mut() {
            obj.remove("model");
            obj.remove("stream");
        }
        let wrapped = serde_json::json!({ "request": gemini });
        return Ok(UpstreamTarget {
            url: crate::url::build_antigravity_count_tokens_url(
                member.upstream_override.as_deref(),
            ),
            body: Bytes::from(wrapped.to_string()),
            protocol: compat::Protocol::Antigravity,
            headers: None,
        });
    }

    if plan.mode == RelayMode::GeminiNative {
        if member.kind() == crate::account::ProviderKind::Devin {
            let openai_body = openai_body_with_model(plan, upstream_model)
                .ok_or_else(|| "Devin requires an OpenAI-shaped request".to_string())?;
            let token = selected_token.to_string();
            let chat_model_uid = upstream_model
                .strip_prefix("devin/")
                .unwrap_or(upstream_model)
                .to_string();
            let supports_vision = pool.devin_model_supports_vision(member.id(), &chat_model_uid);
            let params = compat::devin::DevinRequestParams {
                token: token.clone(),
                chat_model_uid,
                supports_vision,
                trajectory_id: uuid::Uuid::new_v4().to_string(),
                cascade_id: uuid::Uuid::new_v4().to_string(),
                execution_id: uuid::Uuid::new_v4().to_string(),
                fingerprint: "0123456789abcdef0123456789abcdef".to_string(),
            };
            let mut id_counter = 0u64;
            let mut new_id = || {
                id_counter += 1;
                format!("{}-{}", uuid::Uuid::new_v4(), id_counter)
            };
            let wire = compat::devin::build_chat_request(&openai_body, &params, &mut new_id)
                .map_err(|e| format!("failed to build Devin request: {e}"))?;
            let framed = compat::devin::frame_data(&wire);
            let url = crate::url::build_provider_url(
                member.kind(),
                member.upstream_override.as_deref(),
                "/exa.api_server_pb.ApiServerService/GetChatMessage",
            );
            return Ok(UpstreamTarget {
                url,
                body: Bytes::from(framed),
                protocol: compat::Protocol::Devin,
                headers: Some(vec![
                    (
                        "authorization".to_string(),
                        compat::devin::authorization_header(&token),
                    ),
                    (
                        "content-type".to_string(),
                        compat::devin::STREAM_CONTENT_TYPE.to_string(),
                    ),
                    ("connect-protocol-version".to_string(), "1".to_string()),
                ]),
            });
        }

        // The client already speaks Gemini, so only the envelope is added.
        if member.kind() != crate::account::ProviderKind::Antigravity {
            return Err("gemini-native requests need an antigravity or devin account".to_string());
        }
        let project = member
            .project_id()
            .ok_or_else(|| "antigravity account missing project_id".to_string())?;
        let gemini: serde_json::Value = serde_json::from_slice(&plan.body)
            .map_err(|e| format!("invalid gemini request: {e}"))?;
        let model = upstream_model.to_string();
        let mut inner = gemini.clone();
        if let Some(obj) = inner.as_object_mut() {
            obj.remove("model");
            obj.remove("stream");
        }
        let wrapped = crate::v1beta::wrap_for_antigravity(&model, &project, &inner);
        return Ok(UpstreamTarget {
            url: crate::url::build_antigravity_url(member.upstream_override.as_deref()),
            body: Bytes::from(wrapped.to_string()),
            protocol: compat::Protocol::Antigravity,
            headers: None,
        });
    }

    if member.kind() != crate::account::ProviderKind::Antigravity {
        if member.kind() == crate::account::ProviderKind::Devin {
            if matches!(plan.mode, RelayMode::Native) {
                return Err("Devin does not support native/responses relay mode yet".to_string());
            }
            let openai_body = openai_body_with_model(plan, upstream_model)
                .ok_or_else(|| "Devin requires an OpenAI-shaped request".to_string())?;
            let token = selected_token.to_string();
            let chat_model_uid = upstream_model
                .strip_prefix("devin/")
                .unwrap_or(upstream_model)
                .to_string();
            let supports_vision = pool.devin_model_supports_vision(member.id(), &chat_model_uid);
            let params = compat::devin::DevinRequestParams {
                token: token.clone(),
                chat_model_uid,
                supports_vision,
                trajectory_id: uuid::Uuid::new_v4().to_string(),
                cascade_id: uuid::Uuid::new_v4().to_string(),
                execution_id: uuid::Uuid::new_v4().to_string(),
                fingerprint: "0123456789abcdef0123456789abcdef".to_string(),
            };
            let mut id_counter = 0u64;
            let mut new_id = || {
                id_counter += 1;
                format!("{}-{}", uuid::Uuid::new_v4(), id_counter)
            };
            let wire = compat::devin::build_chat_request(&openai_body, &params, &mut new_id)
                .map_err(|e| format!("failed to build Devin request: {e}"))?;
            let framed = compat::devin::frame_data(&wire);
            let url = crate::url::build_provider_url(
                member.kind(),
                member.upstream_override.as_deref(),
                "/exa.api_server_pb.ApiServerService/GetChatMessage",
            );
            return Ok(UpstreamTarget {
                url,
                body: Bytes::from(framed),
                protocol: compat::Protocol::Devin,
                headers: Some(vec![
                    (
                        "authorization".to_string(),
                        compat::devin::authorization_header(&token),
                    ),
                    (
                        "content-type".to_string(),
                        compat::devin::STREAM_CONTENT_TYPE.to_string(),
                    ),
                    ("connect-protocol-version".to_string(), "1".to_string()),
                ]),
            });
        }
        if member.kind() == crate::account::ProviderKind::Vertex {
            let openai = openai_body_with_model(plan, upstream_model)
                .ok_or_else(|| "Vertex requires an OpenAI-shaped request".to_string())?;
            let model = upstream_model;
            let project = member
                .project_id()
                .ok_or_else(|| "Vertex account missing project_id".to_string())?;
            let location = member
                .vertex_location()
                .unwrap_or_else(|| "us-central1".to_string());
            let action = if plan.client_stream {
                "streamGenerateContent?alt=sse"
            } else {
                "generateContent"
            };
            let path = format!(
                "/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:{action}"
            );
            return Ok(UpstreamTarget {
                url: crate::url::build_provider_url(
                    member.kind(),
                    member.upstream_override.as_deref(),
                    &path,
                ),
                body: Bytes::from(compat::gemini::openai_to_gemini(&openai)?.to_string()),
                protocol: compat::Protocol::Antigravity,
                headers: None,
            });
        }
        if member.kind() == crate::account::ProviderKind::Generic {
            let adapter = member
                .generic_adapter()
                .unwrap_or_else(|| "openai-chat".to_string());
            let openai_body = openai_body_with_model(plan, upstream_model)
                .ok_or_else(|| "generic adapter requires OpenAI-compatible input".to_string())?;
            if adapter == "google" {
                let model = openai_body
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| "Google request missing model".to_string())?;
                let action = if plan.client_stream {
                    "streamGenerateContent?alt=sse"
                } else {
                    "generateContent"
                };
                let path = format!("/v1beta/models/{model}:{action}");
                return Ok(UpstreamTarget {
                    url: crate::url::build_provider_url(
                        member.kind(),
                        member.upstream_override.as_deref(),
                        &path,
                    ),
                    body: Bytes::from(compat::gemini::openai_to_gemini(&openai_body)?.to_string()),
                    protocol: compat::Protocol::Antigravity,
                    headers: None,
                });
            }
            if adapter == "anthropic" {
                return Ok(UpstreamTarget {
                    url: crate::url::build_provider_url(
                        member.kind(),
                        member.upstream_override.as_deref(),
                        "/v1/messages",
                    ),
                    body: Bytes::from(
                        serde_json::to_vec(&compat::claude::openai_to_anthropic(&openai_body)?)
                            .map_err(|error| error.to_string())?,
                    ),
                    protocol: compat::Protocol::Anthropic,
                    headers: None,
                });
            }
            if adapter == "mimo-free" {
                let endpoint = member.upstream_override.clone().unwrap_or_default();
                if !mahoquot_providers::is_mimo_endpoint(&endpoint) {
                    return Err(
                        "the mimo-free adapter only serves the canonical MiMo Free endpoint; use openai-chat for a custom one"
                            .to_string(),
                    );
                }
                let mut body = openai_body.clone();
                compat::mimo::inject_system_marker(&mut body);
                return Ok(UpstreamTarget {
                    url: endpoint,
                    body: Bytes::from(body.to_string()),
                    protocol: compat::Protocol::Codex,
                    headers: None,
                });
            }
            if adapter == "openai-responses" || adapter == "azure-openai" {
                let url = crate::url::build_provider_url(
                    member.kind(),
                    member.upstream_override.as_deref(),
                    "/v1/responses",
                );
                // The catalog ships Azure's host as a {resource} template, so a
                // request against an unedited base URL must fail loudly here
                // rather than reach a nonexistent host.
                if url.contains('{') || url.contains('}') {
                    return Err(format!(
                        "{adapter} base URL still contains a placeholder: set your real resource URL"
                    ));
                }
                return Ok(UpstreamTarget {
                    url,
                    body: plan.body.clone(),
                    protocol: compat::Protocol::Codex,
                    headers: None,
                });
            }
            return Ok(UpstreamTarget {
                url: crate::url::build_provider_url(
                    member.kind(),
                    member.upstream_override.as_deref(),
                    "/v1/chat/completions",
                ),
                body: body_with_model(&plan.original_body, upstream_model),
                protocol: compat::Protocol::Codex,
                headers: None,
            });
        }
        if member.kind() == crate::account::ProviderKind::Cursor {
            let openai = openai_body_with_model(plan, upstream_model)
                .ok_or_else(|| "Cursor requires an OpenAI-shaped request".to_string())?;
            return Ok(UpstreamTarget {
                url: crate::url::build_provider_url(
                    member.kind(),
                    member.upstream_override.as_deref(),
                    "/agent.v1.AgentService/Run",
                ),
                body: Bytes::from(compat::cursor::openai_to_cursor_connect(&openai)?),
                protocol: compat::Protocol::Cursor,
                headers: None,
            });
        }
        if member.kind() == crate::account::ProviderKind::Kiro {
            let openai = openai_body_with_model(plan, upstream_model)
                .ok_or_else(|| "Kiro requires an OpenAI-shaped request".to_string())?;
            return Ok(UpstreamTarget {
                url: mahoquot_providers::kiro_generate_url(
                    member.upstream_override.as_deref(),
                    member
                        .kiro_region()
                        .as_deref()
                        .unwrap_or(mahoquot_providers::KIRO_DEFAULT_REGION),
                ),
                body: Bytes::from(
                    serde_json::to_vec(&compat::kiro::openai_to_kiro_with_profile(
                        &openai,
                        member.kiro_profile_arn().as_deref(),
                    )?)
                    .map_err(|e| e.to_string())?,
                ),
                protocol: compat::Protocol::Kiro,
                headers: None,
            });
        }
        if matches!(
            member.kind(),
            crate::account::ProviderKind::Claude | crate::account::ProviderKind::Zcode
        ) {
            let mut anthropic_val: serde_json::Value = if plan.mode == RelayMode::Anthropic {
                let bytes = body_with_model(&plan.original_body, upstream_model);
                serde_json::from_slice(&bytes).unwrap_or_else(|_| serde_json::json!({}))
            } else {
                let openai = openai_body_with_model(plan, upstream_model).ok_or_else(|| {
                    "Anthropic provider requires an OpenAI-shaped request".to_string()
                })?;
                compat::claude::openai_to_anthropic(&openai)?
            };

            if member.kind() == crate::account::ProviderKind::Claude
                && !member.is_nekos_relay()
                && member.relay_api_key().is_none()
            {
                compat::claude::ensure_claude_code_system_instruction(&mut anthropic_val);
            }

            let body = Bytes::from(
                serde_json::to_vec(&anthropic_val).map_err(|e| e.to_string())?,
            );
            return Ok(UpstreamTarget {
                url: crate::url::build_provider_url(
                    member.kind(),
                    member.upstream_override.as_deref(),
                    "/v1/messages",
                ),
                body,
                protocol: compat::Protocol::Anthropic,
                headers: None,
            });
        }
        return Ok(UpstreamTarget {
            url: crate::url::build_provider_url(
                member.kind(),
                member.upstream_override.as_deref(),
                &plan.upstream_path,
            ),
            body: body_with_model(&plan.body, upstream_model),
            protocol: compat::Protocol::Codex,
            headers: None,
        });
    }

    let openai_body = openai_body_with_model(plan, upstream_model)
        .ok_or_else(|| "antigravity requires an openai-shaped request".to_string())?;
    let project = member
        .project_id()
        .ok_or_else(|| "antigravity account missing project_id".to_string())?;
    let translated = compat::openai_to_antigravity(&openai_body, &project)?;

    Ok(UpstreamTarget {
        url: crate::url::build_antigravity_url(member.upstream_override.as_deref()),
        body: Bytes::from(translated.to_string()),
        protocol: compat::Protocol::Antigravity,
        headers: None,
    })
}

#[derive(Debug)]
enum UpstreamSendError {
    Reqwest(reqwest::Error),
    DevinClient(crate::proxy_policy::DevinClientBuildError),
}

impl std::fmt::Display for UpstreamSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reqwest(e) => write!(f, "{e}"),
            Self::DevinClient(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for UpstreamSendError {}

impl From<reqwest::Error> for UpstreamSendError {
    fn from(e: reqwest::Error) -> Self {
        Self::Reqwest(e)
    }
}

impl From<crate::proxy_policy::DevinClientBuildError> for UpstreamSendError {
    fn from(e: crate::proxy_policy::DevinClientBuildError) -> Self {
        Self::DevinClient(e)
    }
}

impl UpstreamSendError {
    fn is_ambiguous(&self) -> bool {
        match self {
            Self::Reqwest(e) => e.is_timeout() || e.is_connect() || e.is_request(),
            Self::DevinClient(_) => false,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_upstream(
    state: &AppState,
    target_url: &str,
    member: &AccountMember,
    headers: &HeaderMap,
    body_bytes: &Bytes,
    protocol: compat::Protocol,
    accept: Option<&str>,
    custom_headers: Option<&[(String, String)]>,
) -> Result<UpstreamExchange, UpstreamSendError> {
    let client = if member.kind() == crate::account::ProviderKind::Devin {
        state.try_devin_client_for_member(member)?
    } else {
        state.client_for_member(member)
    };
    let mut req_builder = client.post(target_url);
    if protocol == compat::Protocol::Devin {
        req_builder = req_builder.version(reqwest::Version::HTTP_11);
    }
    let mut member_headers = if let Some(custom) = custom_headers {
        custom.to_vec()
    } else {
        member.build_upstream_headers()
    };

    // If client provided anthropic-beta headers, merge them with member headers
    // so client-requested beta features (e.g. prompt-caching, output-128k) are preserved.
    if let Some(client_beta) = headers.get("anthropic-beta").and_then(|v| v.to_str().ok()) {
        if let Some((_, val)) = member_headers
            .iter_mut()
            .find(|(k, _)| k == "anthropic-beta")
        {
            let mut betas: Vec<String> = val.split(',').map(|s| s.trim().to_string()).collect();
            for part in client_beta.split(',') {
                let p = part.trim();
                if !p.is_empty() && !betas.iter().any(|b| b == p) {
                    betas.push(p.to_string());
                }
            }
            *val = betas.join(",");
        } else if member.kind() == crate::account::ProviderKind::Claude {
            member_headers.push(("anthropic-beta".to_string(), client_beta.to_string()));
        }
    }

    for (name, val) in member_headers {
        req_builder = req_builder.header(name, val);
    }
    if protocol == compat::Protocol::Devin {
        req_builder = req_builder.header(
            header::ACCEPT,
            accept.unwrap_or(compat::devin::STREAM_CONTENT_TYPE),
        );
    } else {
        if let Some(accept) = accept {
            req_builder = req_builder.header(header::ACCEPT, accept);
        }
        if let Some(ct) = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
        {
            req_builder = req_builder.header(header::CONTENT_TYPE.as_str(), ct);
        }
    }
    let req_start = std::time::Instant::now();
    let (resp, cursor_reply) = if protocol == compat::Protocol::Cursor {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
        let _ = tx.send(body_bytes.clone());
        let heartbeat_tx = tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
            interval.tick().await;
            loop {
                interval.tick().await;
                if heartbeat_tx
                    .send(Bytes::from(compat::cursor::client_heartbeat_frame()))
                    .is_err()
                {
                    break;
                }
            }
        });
        let stream = futures::stream::unfold(
            rx,
            |mut rx: tokio::sync::mpsc::UnboundedReceiver<Bytes>| async move {
                rx.recv()
                    .await
                    .map(|chunk| (Ok::<Bytes, std::io::Error>(chunk), rx))
            },
        );
        (
            req_builder
                .body(reqwest::Body::wrap_stream(stream))
                .send()
                .await?,
            Some(tx),
        )
    } else {
        (req_builder.body(body_bytes.clone()).send().await?, None)
    };
    let elapsed_ms = req_start.elapsed().as_secs_f64() * 1000.0;
    state.monitor.record_ttft(member.id(), elapsed_ms);
    Ok(UpstreamExchange {
        response: resp,
        cursor_reply,
    })
}

async fn extract_failure(resp: reqwest::Response, status_code: u16) -> FinalFailure {
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);
    let body = resp.bytes().await.unwrap_or_default();
    let status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    FinalFailure {
        status,
        content_type,
        body,
    }
}

/// Upper bound on an upstream-supplied cooldown, so a hostile or broken
/// `Retry-After` cannot bench an account effectively forever.
const MAX_COOLDOWN_SECS: i64 = 86_400;

/// Cooldown deadline for an upstream-supplied `Retry-After`, in unix ms.
///
/// The header is fully upstream-controlled, so the value is clamped in BOTH
/// directions: saturating arithmetic alone preserves sign, so a negative value
/// would put the deadline in the past and mean no cooldown at all.
fn cooldown_deadline_ms(now_ms: i64, retry_after_secs: i64) -> i64 {
    let bounded = retry_after_secs.clamp(0, MAX_COOLDOWN_SECS);
    now_ms.saturating_add(bounded.saturating_mul(1000))
}

pub fn cooldown_deadline_from_headers(headers: &reqwest::header::HeaderMap, now_ms: i64) -> i64 {
    let now_unix = now_ms / 1000;
    // Prefer an explicit Retry-After, then a reset timestamp the Codex quota
    // families report on the same responses (opencodex reset-derived cooldown
    // source), so the account returns exactly when its window frees instead of
    // sitting out a fixed 5-minute default.
    let retry_after_secs = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|secs| *secs > 0)
        .or_else(|| {
            headers
                .iter()
                .filter_map(|(name, value)| {
                    let lower = name.as_str().to_ascii_lowercase();
                    if !(lower.ends_with("-reset-at") || lower.ends_with("-reset-after-seconds")) {
                        return None;
                    }
                    let secs: i64 = value.to_str().ok()?.trim().parse().ok()?;
                    let delay = if lower.ends_with("-reset-after-seconds") {
                        secs
                    } else {
                        secs.saturating_sub(now_unix)
                    };
                    (delay > 0).then_some(delay)
                })
                .min()
        })
        .unwrap_or(300);
    cooldown_deadline_ms(now_ms, retry_after_secs)
}

/// Parses a Cline `INFERENCE_CAP_ERROR` 429 response:
/// `{"error":{"code":"INFERENCE_CAP_ERROR","message":"Error 429: Daily free limit reached on model z-ai/glm-5.3-flash. Try again in 8h 48m"}}`
/// Returns `Some((model_slug, reset_seconds))`.
fn parse_cline_cap_error(body: &[u8]) -> Option<(String, i64)> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let message = value.get("error")?.get("message")?.as_str()?;
    let prefix = "Daily free limit reached on model ";
    let pos = message.find(prefix)?;
    let remainder = &message[pos + prefix.len()..];
    let try_again_prefix = ". Try again in ";
    let try_pos = remainder.find(try_again_prefix)?;
    let model = remainder[..try_pos].trim().to_string();
    let time_str = remainder[try_pos + try_again_prefix.len()..].trim();

    let mut hours = 0i64;
    let mut mins = 0i64;
    for part in time_str.split_whitespace() {
        if let Some(h) = part.strip_suffix('h') {
            if let Ok(val) = h.parse::<i64>() {
                hours = val;
            }
        } else if let Some(m) = part.strip_suffix('m') {
            if let Ok(val) = m.parse::<i64>() {
                mins = val;
            }
        }
    }
    let total_secs = hours * 3600 + mins * 60;
    Some((model, if total_secs > 0 { total_secs } else { 300 }))
}

/// Updates the member's `AccountUsage` with a QuotaGroup bucket representing the Cline model limit.
fn record_cline_quota_bucket(member: &AccountMember, model: &str, reset_seconds: i64, now_unix: i64) {
    let reset_at_unix = now_unix + reset_seconds;
    let mut usage = member.usage_snapshot();
    let group_name = "Cline Free Limits".to_string();
    let bucket_label = format!("{model} (Daily limit)");

    let group = match usage.groups.iter_mut().find(|g| g.display_name.as_deref() == Some(&group_name)) {
        Some(g) => g,
        None => {
            usage.groups.push(crate::usage::QuotaGroup {
                display_name: Some(group_name.clone()),
                models: Some("Cline Free Models".to_string()),
                buckets: Vec::new(),
            });
            usage.groups.last_mut().unwrap()
        }
    };

    if let Some(bucket) = group.buckets.iter_mut().find(|b| b.bucket_id.as_deref() == Some(model)) {
        bucket.used_percent = Some(100.0);
        bucket.reset_at_unix = Some(reset_at_unix);
        bucket.display_name = Some(bucket_label);
    } else {
        group.buckets.push(crate::usage::QuotaBucket {
            bucket_id: Some(model.to_string()),
            display_name: Some(bucket_label),
            window: Some("Daily".to_string()),
            used_percent: Some(100.0),
            reset_at_unix: Some(reset_at_unix),
        });
    }

    usage.observed_at_unix = Some(now_unix);
    member.set_usage(usage);
}

async fn record_cooldown(
    resp: reqwest::Response,
    member: &AccountMember,
    status_code: u16,
    state: &AppState,
) -> FinalFailure {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0);
    member.set_health(Health::Cooldown {
        // Retry-After is upstream-controlled: clamp so a hostile or broken value
        // cannot overflow into a past (or panicking) cooldown deadline.
        until_unix_ms: cooldown_deadline_from_headers(resp.headers(), now_ms),
    });
    member.record_fail();
    state.metrics.failed_over.fetch_add(1, Ordering::Relaxed);
    state
        .monitor
        .record_error(member.id(), status_code, "upstream error");
    extract_failure(resp, status_code).await
}

fn content_type_of(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string)
}

fn stream_response(resp: reqwest::Response, status_code: u16) -> Response {
    let status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::OK);
    let mut res_builder = Response::builder().status(status);
    if let Some(ct) = resp.headers().get(reqwest::header::CONTENT_TYPE) {
        if let Ok(v) = HeaderValue::from_bytes(ct.as_bytes()) {
            res_builder = res_builder.header(header::CONTENT_TYPE, v);
        }
    }
    res_builder
        .body(Body::from_stream(resp.bytes_stream()))
        .unwrap_or_else(|_| {
            (StatusCode::INTERNAL_SERVER_ERROR, "failed to build body").into_response()
        })
}

fn body_response(status: StatusCode, content_type: Option<&str>, body: Bytes) -> Response {
    let mut builder = Response::builder().status(status);
    if let Some(ct) = content_type {
        if let Ok(v) = HeaderValue::from_str(ct) {
            builder = builder.header(header::CONTENT_TYPE, v);
        }
    }
    builder
        .body(Body::from(body))
        .unwrap_or_else(|_| (status, "error").into_response())
}

fn json_error(status: StatusCode, message: &str) -> Response {
    let payload = serde_json::json!({
        "type": "error",
        "error": {"message": message, "type": "invalid_request_error"}
    });
    body_response(
        status,
        Some("application/json"),
        Bytes::from(payload.to_string()),
    )
}

fn is_account_scoped_model_rejection(status_code: u16, body: &[u8]) -> bool {
    if status_code == 402 {
        return true;
    }
    // Model rejections also arrive as 403s on entitlement-gated shards.
    if status_code != 400 && status_code != 403 {
        return false;
    }
    let text = String::from_utf8_lossy(body);
    text.contains("is not supported when using Codex")
        || text.contains("model is not supported")
        || text.contains("INVALID_MODEL_ID")
        || text.contains("MONTHLY_REQUEST_COUNT")
}

fn build_plan(mode: RelayMode, req_path: &str, body_bytes: Bytes) -> Result<RelayPlan, String> {
    match mode {
        RelayMode::Native => Ok(RelayPlan {
            upstream_path: req_path.to_string(),
            model: compat::extract_model(&body_bytes),
            body: body_bytes.clone(),
            mode,
            client_stream: true,
            include_usage: false,
            openai_body: None,
            original_body: body_bytes.clone(),
        }),
        RelayMode::GeminiCountTokens | RelayMode::Image => Ok(RelayPlan {
            upstream_path: req_path.to_string(),
            model: compat::extract_model(&body_bytes),
            body: body_bytes.clone(),
            mode,
            client_stream: false,
            include_usage: false,
            openai_body: None,
            original_body: body_bytes.clone(),
        }),
        RelayMode::Anthropic => {
            let anthropic: serde_json::Value = serde_json::from_slice(&body_bytes)
                .map_err(|e| format!("invalid anthropic request: {e}"))?;
            let openai = compat::anthropic_to_openai(&anthropic)?;
            let openai_bytes = Bytes::from(openai.to_string());
            let translated = compat::openai_to_codex(&openai_bytes).map_err(|e| e.to_string())?;
            Ok(RelayPlan {
                upstream_path: compat::CODEX_PATH.to_string(),
                body: Bytes::from(translated.body),
                model: Some(translated.model),
                mode,
                client_stream: translated.stream,
                include_usage: translated.include_usage,
                openai_body: Some(openai),
                original_body: body_bytes,
            })
        }
        RelayMode::GeminiNative => {
            let gemini: serde_json::Value = serde_json::from_slice(&body_bytes)
                .map_err(|e| format!("invalid gemini request: {e}"))?;
            let stream = gemini
                .get("stream")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let model = compat::extract_model(&body_bytes);
            let openai_body = model
                .as_deref()
                .and_then(|m| compat::gemini::gemini_to_openai(&gemini, m).ok());
            Ok(RelayPlan {
                upstream_path: req_path.to_string(),
                model,
                body: body_bytes.clone(),
                mode,
                client_stream: stream,
                include_usage: false,
                openai_body,
                original_body: body_bytes.clone(),
            })
        }
        RelayMode::Responses => {
            let responses_req: serde_json::Value = serde_json::from_slice(&body_bytes)
                .map_err(|e| format!("invalid responses request: {e}"))?;
            let stream = responses_req
                .get("stream")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let openai = compat::responses::responses_to_openai(&responses_req)
                .map_err(|e| e.to_string())?;
            let model = compat::extract_model(&body_bytes);
            Ok(RelayPlan {
                upstream_path: req_path.to_string(),
                model,
                body: body_bytes.clone(),
                mode,
                client_stream: stream,
                include_usage: false,
                openai_body: Some(openai),
                original_body: body_bytes.clone(),
            })
        }
        RelayMode::OpenAiCompat | RelayMode::LegacyCompletions => {
            match compat::openai_to_codex(&body_bytes) {
                Ok(translated) => Ok(RelayPlan {
                    upstream_path: compat::CODEX_PATH.to_string(),
                    body: Bytes::from(translated.body),
                    model: Some(translated.model),
                    mode,
                    client_stream: translated.stream,
                    include_usage: translated.include_usage,
                    openai_body: serde_json::from_slice(&body_bytes).ok(),
                    original_body: body_bytes,
                }),
                Err(err) => Err(err.to_string()),
            }
        }
    }
}

fn reply_shape(mode: RelayMode) -> compat::ReplyShape {
    match mode {
        RelayMode::GeminiNative => compat::ReplyShape::Gemini,
        RelayMode::LegacyCompletions => compat::ReplyShape::TextCompletion,
        RelayMode::Responses => compat::ReplyShape::Responses,
        _ => compat::ReplyShape::Chat,
    }
}

fn member_matches_api_key_binding(
    member: &AccountMember,
    binding: Option<&crate::management::settings::ApiKeyBinding>,
) -> bool {
    let Some(binding) = binding else {
        return true;
    };
    binding
        .account
        .as_deref()
        .is_none_or(|account| member.id() == account)
        && binding
            .provider
            .as_deref()
            .is_none_or(|provider| member.provider_name() == provider)
}

fn member_provider_id(member: &AccountMember) -> Option<ProviderId> {
    let id = match member.kind() {
        crate::account::ProviderKind::Codex => ProviderId::codex(),
        crate::account::ProviderKind::Antigravity => ProviderId::antigravity(),
        crate::account::ProviderKind::Claude => ProviderId::claude(),
        crate::account::ProviderKind::Cursor => ProviderId::cursor(),
        crate::account::ProviderKind::Kiro => ProviderId::kiro(),
        crate::account::ProviderKind::Zcode => ProviderId::zcode(),
        crate::account::ProviderKind::Vertex => ProviderId::vertex(),
        crate::account::ProviderKind::Devin => ProviderId::devin(),
        crate::account::ProviderKind::Generic => {
            ProviderId::canonical(member.provider_name()).ok()?
        }
    };
    Some(id)
}

fn account_declares_binding_model(
    pool: &crate::state::PoolSnapshot,
    member: &AccountMember,
    requested_model: &str,
    canonical_model: &str,
    provider: &ResolvedProviderClass,
) -> bool {
    let unsupported = member
        .unsupported_models
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if unsupported.iter().any(|model| {
        model == requested_model || model == canonical_model || model == &provider.upstream_model
    }) {
        return false;
    }
    drop(unsupported);

    if member.kind() == crate::account::ProviderKind::Devin {
        return pool.is_devin_model_eligible(member.id(), requested_model)
            || pool.is_devin_model_eligible(member.id(), canonical_model)
            || pool.is_devin_model_eligible(member.id(), &provider.upstream_model);
    }

    let Some((_, models)) = member.generic_models() else {
        return true;
    };
    models.is_empty()
        || models.iter().any(|model| {
            model == requested_model
                || model == canonical_model
                || model == &provider.upstream_model
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelPrefix {
    Anthropic,
    Nekos,
}

pub fn parse_model_prefix(model: &str) -> (Option<ModelPrefix>, &str) {
    if let Some(stripped) = model
        .strip_prefix("anthropic-")
        .or_else(|| model.strip_prefix("anthropic/"))
    {
        (Some(ModelPrefix::Anthropic), stripped)
    } else if let Some(stripped) = model
        .strip_prefix("nekos-")
        .or_else(|| model.strip_prefix("nekos/"))
    {
        (Some(ModelPrefix::Nekos), stripped)
    } else {
        (None, model)
    }
}

pub(crate) fn resolve_model(
    pool: &crate::state::PoolSnapshot,
    requested_model: &str,
) -> Result<mahoquot_registry::ResolvedModel, mahoquot_registry::RegistryError> {
    let (_, base_model) = parse_model_prefix(requested_model);
    let base_id = mahoquot_registry::ModelId::new(base_model)?;
    if pool.registry.aliases().contains_key(&base_id) {
        return pool.registry.resolve(base_model);
    }
    let resolved_model_id = if pool.registry.models().contains_key(&base_id) {
        base_model
    } else if base_model.starts_with("claude-") {
        let mut dated_models = pool.registry.models().keys().filter(|id| {
            id.as_str()
                .strip_prefix(base_model)
                .and_then(|suffix| suffix.strip_prefix('-'))
                .is_some_and(|date| date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()))
        });
        match (dated_models.next(), dated_models.next()) {
            (Some(target), None) => target.as_str(),
            _ => base_model,
        }
    } else {
        base_model
    };
    let canonical_id = mahoquot_registry::ModelId::new(resolved_model_id)?;
    if pool.registry.exclusions().contains(&mahoquot_registry::ModelExclusionRule {
        model_id: canonical_id.clone(),
        provider_id: None,
    }) {
        return Err(mahoquot_registry::RegistryError::ModelExcluded { model_id: canonical_id });
    }
    if resolved_model_id.starts_with("claude-")
        && !pool.registry.models().contains_key(&canonical_id)
    {
        if pool.registry.exclusions().contains(&mahoquot_registry::ModelExclusionRule {
            model_id: canonical_id.clone(),
            provider_id: Some(ProviderId::claude()),
        }) {
            return Err(mahoquot_registry::RegistryError::UnknownModel(canonical_id));
        }
        Ok(mahoquot_registry::ResolvedModel {
            canonical_id,
            descriptor: None,
            eligible_bindings: vec![mahoquot_registry::ProviderBinding::new(
                mahoquot_registry::ProviderId::claude(),
                mahoquot_registry::ProviderPolicy::Closed,
                mahoquot_registry::CatalogSource::EmbeddedFallback,
            )],
            effective_capabilities: std::collections::BTreeSet::new(),
            source: mahoquot_registry::CatalogSource::EmbeddedFallback,
        })
    } else {
        pool.registry.resolve(resolved_model_id)
    }
}

fn resolve_route(
    pool: &crate::state::PoolSnapshot,
    requested_model: Option<&str>,
    required_capability: Option<ModelCapability>,
) -> Result<Option<ResolvedRoute>, mahoquot_registry::RegistryError> {
    let Some(requested_model) = requested_model else {
        return Ok(None);
    };
    let resolved = resolve_model(pool, requested_model)?;
    let provider_classes = resolved
        .eligible_bindings
        .into_iter()
        .filter(|binding| {
            required_capability
                .as_ref()
                .is_none_or(|capability| binding.capabilities.contains(capability))
        })
        .filter_map(|binding| {
            let loaded = pool
                .members
                .iter()
                .any(|member| member_provider_id(member).as_ref() == Some(&binding.provider_id));
            loaded.then(|| ResolvedProviderClass {
                upstream_model: binding
                    .effective_upstream_id(&resolved.canonical_id)
                    .to_string(),
                binding,
            })
        })
        .collect();

    Ok(Some(ResolvedRoute {
        canonical_model: resolved.canonical_id.to_string(),
        capabilities: resolved.effective_capabilities,
        provider_classes,
    }))
}

fn eligible_indices(
    pool: &crate::state::PoolSnapshot,
    route: Option<&ResolvedRoute>,
    requested_model: Option<&str>,
    now_ms: i64,
    api_key_binding: Option<&crate::management::settings::ApiKeyBinding>,
    scoped_key: Option<&crate::management::settings::ScopedApiKey>,
    state: &AppState,
) -> Vec<usize> {
    let Some(route) = route else {
        return pool
            .members
            .iter()
            .enumerate()
            .filter(|(_, member)| member.health().is_available(now_ms))
            .filter(|(_, member)| state.scheduler.permits(member.id()))
            .filter(|(_, member)| member_matches_api_key_binding(member, api_key_binding))
            .filter(|(_, member)| crate::models_route::member_matches_scope(member, scoped_key))
            .map(|(index, _)| index)
            .collect();
    };
    let requested_model = requested_model.unwrap_or(&route.canonical_model);
    let (prefix, _) = parse_model_prefix(requested_model);
    let mut eligible = Vec::new();
    for provider in &route.provider_classes {
        let indices: Vec<usize> = pool
            .members
            .iter()
            .enumerate()
            .filter(|(_, member)| member.health().is_available(now_ms))
            .filter(|(_, member)| state.scheduler.permits(member.id()))
            .filter(|(_, member)| member_matches_api_key_binding(member, api_key_binding))
            .filter(|(_, member)| crate::models_route::member_matches_scope(member, scoped_key))
            .filter(|(_, member)| {
                match prefix {
                    Some(ModelPrefix::Anthropic) => {
                        member.kind() == crate::account::ProviderKind::Claude
                            && !member.is_nekos_relay()
                    }
                    Some(ModelPrefix::Nekos) => {
                        member.kind() == crate::account::ProviderKind::Claude
                            && member.is_nekos_relay()
                    }
                    None => true,
                }
            })
            .filter(|(_, member)| {
                member_provider_id(member).as_ref() == Some(&provider.binding.provider_id)
            })
            .filter(|(_, member)| {
                account_declares_binding_model(
                    pool,
                    member,
                    requested_model,
                    &route.canonical_model,
                    provider,
                )
            })
            .map(|(index, _)| index)
            .collect();
        eligible.extend(indices);
    }
    eligible
}

fn select_index(
    state: &AppState,
    pool: &crate::state::PoolSnapshot,
    hint: &SessionHint,
    eligible: &[usize],
    exclude: &[usize],
) -> Option<usize> {
    // If there is an active session affinity key pointing to an eligible, non-excluded
    // member, select that member's provider group first so session affinity survives across turns.
    let target_provider = hint
        .affinity_key
        .as_deref()
        .and_then(|key| state.router.bound_affinity_member(key))
        .and_then(|bound_id| {
            eligible
                .iter()
                .copied()
                .find(|&idx| {
                    !exclude.contains(&idx)
                        && pool.members.get(idx).map(|m| m.id()) == Some(&bound_id)
                })
                .and_then(|idx| member_provider_id(pool.members.get(idx)?))
        })
        .or_else(|| {
            let first_index = eligible
                .iter()
                .copied()
                .find(|index| !exclude.contains(index))?;
            member_provider_id(pool.members.get(first_index)?)
        })?;

    let mut candidates: Vec<Arc<dyn PoolMember>> = Vec::with_capacity(eligible.len());
    let mut origin: Vec<usize> = Vec::with_capacity(eligible.len());
    for &index in eligible {
        if exclude.contains(&index) {
            continue;
        }
        let member = pool.members.get(index)?;
        if member_provider_id(member).as_ref() != Some(&target_provider) {
            continue;
        }
        candidates.push(member.clone());
        origin.push(index);
    }
    state
        .router
        .select(&candidates, hint)
        .and_then(|idx| origin.get(idx).copied())
}

fn provider_for_member<'a>(
    route: &'a ResolvedRoute,
    member: &AccountMember,
) -> Option<&'a ResolvedProviderClass> {
    let provider_id = member_provider_id(member)?;
    route
        .provider_classes
        .iter()
        .find(|provider| provider.binding.provider_id == provider_id)
}

/// Codex and Claude both report quota state on every response, under different
/// header families; antigravity sends none on the relay path and is polled
/// separately, so its accounts stay "unknown" rather than being reported as
/// having full quota.
pub(crate) fn usage_header_prefix(kind: crate::account::ProviderKind) -> Option<&'static str> {
    match kind {
        crate::account::ProviderKind::Codex => Some("x-codex-"),
        crate::account::ProviderKind::Claude => Some("anthropic-ratelimit-"),
        _ => None,
    }
}

fn capture_usage(member: &AccountMember, headers: &HeaderMap) {
    let Some(prefix) = usage_header_prefix(member.kind()) else {
        return;
    };
    let map: std::collections::HashMap<String, String> = headers
        .iter()
        .filter_map(|(k, v)| {
            let name = k.as_str();
            if !name.starts_with(prefix) {
                return None;
            }
            Some((name.to_string(), v.to_str().ok()?.to_string()))
        })
        .collect();
    if map.is_empty() {
        return;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let usage = match member.kind() {
        crate::account::ProviderKind::Claude => parse_claude_headers(&map, now),
        crate::account::ProviderKind::Devin => crate::usage::AccountUsage::default(),
        _ => parse_codex_headers(&map, now),
    };
    if usage.observed_at_unix.is_some() {
        member.set_usage(usage);
    }
}

#[cfg(test)]
mod usage_capture_tests {

    #[test]
    fn cooldown_deadline_clamps_hostile_retry_after() {
        use super::{cooldown_deadline_ms, MAX_COOLDOWN_SECS};
        let now = 1_000_000_i64;
        // A negative header must not put the deadline in the past: that would
        // mean no cooldown at all, since Health::Cooldown compares
        // until_unix_ms <= now (types/src/lib.rs:28).
        assert_eq!(cooldown_deadline_ms(now, -1), now);
        assert_eq!(cooldown_deadline_ms(now, i64::MIN), now);
        // An absurd header must not bench the account effectively forever.
        assert_eq!(
            cooldown_deadline_ms(now, i64::MAX),
            now + MAX_COOLDOWN_SECS * 1000
        );
        // Ordinary values pass through.
        assert_eq!(cooldown_deadline_ms(now, 300), now + 300_000);
        assert_eq!(cooldown_deadline_ms(now, 0), now);
    }

    use super::usage_header_prefix;
    use crate::account::ProviderKind;

    #[test]
    fn claude_responses_are_scanned_for_subscription_quota_headers() {
        assert_eq!(
            usage_header_prefix(ProviderKind::Claude),
            Some("anthropic-ratelimit-")
        );
        assert_eq!(usage_header_prefix(ProviderKind::Codex), Some("x-codex-"));
        assert_eq!(usage_header_prefix(ProviderKind::Antigravity), None);
    }
}

pub fn connect_code_to_http_status(code: &str) -> StatusCode {
    match code {
        "canceled" => StatusCode::REQUEST_TIMEOUT,
        "invalid_argument" | "failed_precondition" | "out_of_range" => StatusCode::BAD_REQUEST,
        "deadline_exceeded" => StatusCode::GATEWAY_TIMEOUT,
        "not_found" => StatusCode::NOT_FOUND,
        "already_exists" | "aborted" => StatusCode::CONFLICT,
        "permission_denied" => StatusCode::FORBIDDEN,
        "resource_exhausted" => StatusCode::TOO_MANY_REQUESTS,
        "unimplemented" => StatusCode::NOT_IMPLEMENTED,
        "unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        "unauthenticated" => StatusCode::UNAUTHORIZED,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

async fn acquire_devin_first_frame(
    resp: reqwest::Response,
) -> Result<(Bytes, compat::UpstreamStream), String> {
    use futures::StreamExt;
    let mut stream: compat::UpstreamStream = Box::pin(resp.bytes_stream());

    // 1. Read chunks until we have at least 5 bytes for the Connect header.
    let mut header_buf = [0u8; 5];
    let mut header_len = 0;
    let mut first_chunk_rem: Option<Bytes> = None;

    while header_len < 5 {
        let chunk = match stream.next().await {
            Some(Ok(c)) => c,
            Some(Err(_)) => return Err("upstream connection error during preflight".to_string()),
            None => {
                return Err("connect stream ended with truncated header during preflight".to_string())
            }
        };

        if chunk.is_empty() {
            continue;
        }

        let needed = 5 - header_len;
        if chunk.len() >= needed {
            header_buf[header_len..5].copy_from_slice(&chunk[..needed]);
            let rem = chunk.slice(needed..);
            if !rem.is_empty() {
                first_chunk_rem = Some(rem);
            }
            break;
        } else {
            header_buf[header_len..header_len + chunk.len()].copy_from_slice(&chunk);
            header_len += chunk.len();
        }
    }

    let flags = header_buf[0];
    let payload_len = u32::from_be_bytes(header_buf[1..5].try_into().unwrap()) as usize;
    if payload_len > compat::devin::MAX_FRAME_SIZE {
        return Err(format!(
            "connect frame too large: {payload_len} bytes > {}",
            compat::devin::MAX_FRAME_SIZE
        ));
    }

    let required_frame_len = 5 + payload_len;

    // Buffer ONLY the first declared frame; preserve remainder as Bytes slices zero-copy
    let (first_frame_bytes, stream) = match first_chunk_rem {
        Some(rem) if rem.len() >= payload_len => {
            let mut frame_buf = bytes::BytesMut::with_capacity(required_frame_len);
            frame_buf.extend_from_slice(&header_buf);
            frame_buf.extend_from_slice(&rem[..payload_len]);
            let trailing = rem.slice(payload_len..);
            let s: compat::UpstreamStream = if trailing.is_empty() {
                stream
            } else {
                Box::pin(futures::stream::once(async move { Ok(trailing) }).chain(stream))
            };
            (frame_buf.freeze(), s)
        }
        Some(rem) => {
            let mut frame_buf = bytes::BytesMut::with_capacity(required_frame_len);
            frame_buf.extend_from_slice(&header_buf);
            frame_buf.extend_from_slice(&rem);
            let mut s = stream;
            while frame_buf.len() < required_frame_len {
                let chunk = match s.next().await {
                    Some(Ok(c)) => c,
                    Some(Err(_)) => return Err("upstream connection error during preflight".to_string()),
                    None => {
                        return Err("connect stream ended with truncated frame during preflight".to_string())
                    }
                };
                let needed = required_frame_len - frame_buf.len();
                if chunk.len() <= needed {
                    frame_buf.extend_from_slice(&chunk);
                } else {
                    frame_buf.extend_from_slice(&chunk[..needed]);
                    let trailing = chunk.slice(needed..);
                    s = Box::pin(futures::stream::once(async move { Ok(trailing) }).chain(s));
                    break;
                }
            }
            (frame_buf.freeze(), s)
        }
        None => {
            let mut frame_buf = bytes::BytesMut::with_capacity(required_frame_len);
            frame_buf.extend_from_slice(&header_buf);
            let mut s = stream;
            while frame_buf.len() < required_frame_len {
                let chunk = match s.next().await {
                    Some(Ok(c)) => c,
                    Some(Err(_)) => return Err("upstream connection error during preflight".to_string()),
                    None => {
                        return Err("connect stream ended with truncated frame during preflight".to_string())
                    }
                };
                let needed = required_frame_len - frame_buf.len();
                if chunk.len() <= needed {
                    frame_buf.extend_from_slice(&chunk);
                } else {
                    frame_buf.extend_from_slice(&chunk[..needed]);
                    let trailing = chunk.slice(needed..);
                    s = Box::pin(futures::stream::once(async move { Ok(trailing) }).chain(s));
                    break;
                }
            }
            (frame_buf.freeze(), s)
        }
    };

    // Validate the first frame using the accepted DevinDecoder
    let mut decoder = compat::devin::DevinDecoder::new();
    let mut events = Vec::new();
    decoder.decode(&first_frame_bytes, &mut events);

    if flags == 2 {
        // First frame is terminal EndStream: decode finish to capture error or completion
        decoder.finish(&mut events);
        if let Some(ref code) = decoder.outcome().error_code {
            let desc = decoder
                .outcome()
                .error_message
                .as_deref()
                .unwrap_or("upstream error");
            return Err(format!("{code}: {desc}"));
        }
    }

    for event in &events {
        if let compat::events::CodexEvent::Failed { message } = event {
            return Err(message.clone());
        }
    }

    Ok((first_frame_bytes, stream))
}

#[allow(clippy::too_many_arguments)]
async fn finish_success(
    state: &AppState,
    member: &AccountMember,
    plan: &RelayPlan,
    headers: &HeaderMap,
    resp: reqwest::Response,
    status_code: u16,
    created: i64,
    session: compat::ProtocolSession,
) -> Result<Response, String> {
    let protocol = session.protocol;
    let content_type = content_type_of(&resp);
    if protocol == compat::Protocol::Devin {
        let is_valid_ct = content_type
            .as_deref()
            .map(|ct| ct.split(';').next().unwrap_or("").trim() == compat::devin::STREAM_CONTENT_TYPE)
            .unwrap_or(false);
        if !is_valid_ct {
            let actual = content_type.as_deref().unwrap_or("missing");
            return Err(format!(
                "invalid content-type for Devin Connect stream: expected {}, got {}",
                compat::devin::STREAM_CONTENT_TYPE,
                actual
            ));
        }
    } else {
        capture_usage(member, resp.headers());
        state.monitor.clear_error(member.id());
        state
            .scheduler
            .record_success(member.id(), &state.pool.load().members);
    }

    // Generic accounts relay verbatim only when the upstream speaks the same
    // wire as the client. The google and anthropic adapters do not, so they
    // fall through to the conversion branches below.
    if member.kind() == crate::account::ProviderKind::Generic
        && !matches!(
            member.generic_adapter().as_deref(),
            Some("google") | Some("anthropic")
        )
    {
        if content_type
            .as_deref()
            .is_some_and(|ct| ct.trim_start().starts_with("text/html"))
        {
            return Err("upstream returned html instead of an api response".to_string());
        }
        member.record_ok();
        state.metrics.served.fetch_add(1, Ordering::Relaxed);
        state.router.feedback(member.id(), Outcome::Success);
        return Ok(stream_response(resp, status_code));
    }

    if matches!(
        plan.mode,
        RelayMode::Native | RelayMode::Image | RelayMode::GeminiCountTokens
    ) {
        if member.kind() == crate::account::ProviderKind::Devin {
            return Err("Devin does not support native/responses relay mode yet".to_string());
        }
        if content_type
            .as_deref()
            .is_some_and(|ct| ct.trim_start().starts_with("text/html"))
        {
            return Err("upstream returned html instead of an api response".to_string());
        }
        member.record_ok();
        state.metrics.served.fetch_add(1, Ordering::Relaxed);
        state.router.feedback(member.id(), Outcome::Success);
        return Ok(stream_response(resp, status_code));
    }

    if content_type
        .as_deref()
        .is_some_and(|ct| ct.trim_start().starts_with("text/html"))
    {
        return Err("upstream body is not an event stream: html response".to_string());
    }

    if !plan.client_stream
        && protocol == compat::Protocol::Anthropic
        && content_type
            .as_deref()
            .is_some_and(|ct| ct.starts_with("application/json"))
    {
        let raw = resp.bytes().await.map_err(|e| e.to_string())?;
        let value: serde_json::Value = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
        member.record_ok();
        state.metrics.served.fetch_add(1, Ordering::Relaxed);
        state.router.feedback(member.id(), Outcome::Success);
        let output = if plan.mode == RelayMode::Anthropic {
            value
        } else {
            compat::claude::anthropic_json_to_openai(
                &value,
                &plan.model.clone().unwrap_or_default(),
                created,
            )
        };
        return Ok(body_response(
            StatusCode::OK,
            Some("application/json"),
            Bytes::from(output.to_string()),
        ));
    }

    if !plan.client_stream
        && (member.kind() == crate::account::ProviderKind::Vertex
            || (member.kind() == crate::account::ProviderKind::Generic
                && member.generic_adapter().as_deref() == Some("google")))
        && content_type
            .as_deref()
            .is_some_and(|ct| ct.starts_with("application/json"))
    {
        let raw = resp.bytes().await.map_err(|error| error.to_string())?;
        let value: serde_json::Value =
            serde_json::from_slice(&raw).map_err(|error| error.to_string())?;
        let output = compat::gemini::gemini_json_to_openai(
            &value,
            &plan.model.clone().unwrap_or_default(),
            created,
        );
        member.record_ok();
        state.metrics.served.fetch_add(1, Ordering::Relaxed);
        state.router.feedback(member.id(), Outcome::Success);
        return Ok(body_response(
            StatusCode::OK,
            Some("application/json"),
            Bytes::from(output.to_string()),
        ));
    }

    let (first, stream) = if protocol == compat::Protocol::Devin {
        tokio::time::timeout(
            get_preflight_deadline(headers),
            acquire_devin_first_frame(resp),
        )
        .await
        .map_err(|_| "upstream request timed out during preflight".to_string())??
    } else {
        compat::open_stream(resp, protocol).await?
    };
    let model = plan.model.clone().unwrap_or_default();

    if plan.mode == RelayMode::Anthropic && plan.client_stream {
        let devin_outcome = (protocol == compat::Protocol::Devin)
            .then(|| Arc::new(std::sync::Mutex::new(None)));
        if protocol != compat::Protocol::Devin {
            member.record_ok();
            state.metrics.served.fetch_add(1, Ordering::Relaxed);
            state.router.feedback(member.id(), Outcome::Success);
        }
        let upstream_capture = Arc::new(std::sync::Mutex::new(None));
        let body = compat::streaming_body(compat::StreamingBodyParams {
            first,
            upstream: stream,
            model,
            created,
            include_usage: false,
            shape: compat::ReplyShape::Anthropic,
            session,
            upstream_capture: Some(Arc::clone(&upstream_capture)),
            devin_outcome: devin_outcome.as_ref().map(Arc::clone),
        });
        let mut response = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(body)
            .unwrap_or_else(|_| {
                (StatusCode::INTERNAL_SERVER_ERROR, "failed to build body").into_response()
            });
        response.extensions_mut().insert(upstream_capture);
        if let Some(cell) = devin_outcome {
            response.extensions_mut().insert(cell);
        }
        return Ok(response);
    }

    if plan.mode == RelayMode::Anthropic {
        let (raw, upstream_usage, devin_outcome) =
            compat::collect_stream_with_replies(first, stream, session).await?;
        if protocol == compat::Protocol::Devin {
            if let Some(devin) = devin_outcome {
                if let Some(code) = devin.error_code {
                    return Err(format!(
                        "{code}: {}",
                        devin.error_message.as_deref().unwrap_or("unknown upstream error")
                    ));
                }
                if !devin.terminated {
                    return Err("connect stream ended without EndStream frame".to_string());
                }
            }
        }
        member.record_ok();
        state.metrics.served.fetch_add(1, Ordering::Relaxed);
        state.router.feedback(member.id(), Outcome::Success);
        let mut response = compat::anthropic_response(
            &raw,
            &model,
            created,
            protocol,
            plan.client_stream,
        );
        if let Some(usage) = upstream_usage {
            response
                .extensions_mut()
                .insert(Arc::new(std::sync::Mutex::new(Some(usage))));
        }
        return Ok(response);
    }

    if plan.client_stream {
        let devin_outcome = (protocol == compat::Protocol::Devin)
            .then(|| Arc::new(std::sync::Mutex::new(None)));
        if protocol != compat::Protocol::Devin {
            member.record_ok();
            state.metrics.served.fetch_add(1, Ordering::Relaxed);
            state.router.feedback(member.id(), Outcome::Success);
        }
        let upstream_capture = Arc::new(std::sync::Mutex::new(None));
        let body = compat::streaming_body(compat::StreamingBodyParams {
            first,
            upstream: stream,
            model,
            created,
            include_usage: plan.include_usage,
            shape: reply_shape(plan.mode),
            session,
            upstream_capture: Some(Arc::clone(&upstream_capture)),
            devin_outcome: devin_outcome.as_ref().map(Arc::clone),
        });
        let mut response = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(body)
            .unwrap_or_else(|_| {
                (StatusCode::INTERNAL_SERVER_ERROR, "failed to build body").into_response()
            });
        response.extensions_mut().insert(upstream_capture);
        if let Some(cell) = devin_outcome {
            response.extensions_mut().insert(cell);
        }
        return Ok(response);
    }

    let (raw, upstream_usage, devin_outcome) =
        compat::collect_stream_with_replies(first, stream, session).await?;
    if protocol == compat::Protocol::Devin {
        if let Some(devin) = devin_outcome {
            if let Some(code) = devin.error_code {
                return Err(format!(
                    "{code}: {}",
                    devin.error_message.as_deref().unwrap_or("unknown upstream error")
                ));
            }
            if !devin.terminated {
                return Err("connect stream ended without EndStream frame".to_string());
            }
        }
    }
    let completion = compat::aggregate(&raw, model, created, protocol, reply_shape(plan.mode))?;
    if protocol == compat::Protocol::Devin {
        let current_pool_member = state
            .pool
            .load()
            .members
            .iter()
            .find(|m| {
                m.id() == member.id()
                    && m.access_token() == member.access_token()
                    && member.shares_runtime_identity(m)
            })
            .cloned();

        member.record_ok();
        if let Some(ref current) = current_pool_member {
            if !Arc::ptr_eq(&member.ok_count, &current.ok_count) {
                current.record_ok();
            }
            state.monitor.clear_error(current.id());
            state.scheduler.record_success(current.id(), &state.pool.load().members);
            state.router.feedback(current.id(), Outcome::Success);
        }
        state.metrics.served.fetch_add(1, Ordering::Relaxed);
    } else {
        member.record_ok();
        state.metrics.served.fetch_add(1, Ordering::Relaxed);
        state.router.feedback(member.id(), Outcome::Success);
        state.monitor.clear_error(member.id());
        state
            .scheduler
            .record_success(member.id(), &state.pool.load().members);
    }
    let mut response = body_response(
        StatusCode::OK,
        Some("application/json"),
        Bytes::from(completion.to_string()),
    );
    if let Some(usage) = upstream_usage {
        response
            .extensions_mut()
            .insert(Arc::new(std::sync::Mutex::new(Some(usage))));
    }
    Ok(response)
}

/// Identify the conversation a request belongs to, so successive turns keep
/// landing on the same upstream account. Codex and Anthropic clients both send a
/// stable per-session id; without one the request routes by plain round-robin.
fn affinity_key(headers: &HeaderMap) -> Option<String> {
    for name in [
        "x-claude-code-session-id",
        "session_id",
        // Codex CLI sends the hyphenated form; opencodex binds pool affinity on
        // session-id + thread-id (and pins subagent fan-out to the parent thread).
        "session-id",
        "thread-id",
        "x-codex-parent-thread-id",
        "x-session-id",
        "conversation_id",
        "x-conversation-id",
        "anthropic-client-session",
    ] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn body_affinity_key(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let obj = value.as_object()?;

    let extract_val = |v: &serde_json::Value| -> Option<String> {
        match v {
            serde_json::Value::String(s) => {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    Some(trimmed.to_string())
                } else {
                    None
                }
            }
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    };

    for field in [
        "conversation_id",
        "conversation-id",
        "thread_id",
        "thread-id",
        "session_id",
        "session-id",
    ] {
        if let Some(v) = obj.get(field).and_then(extract_val) {
            return Some(format!("body-id-{v}"));
        }
    }

    if let Some(metadata) = obj.get("metadata").and_then(|m| m.as_object()) {
        for field in [
            "thread_id",
            "thread-id",
            "conversation_id",
            "conversation-id",
            "session_id",
            "session-id",
        ] {
            if let Some(v) = metadata.get(field).and_then(extract_val) {
                return Some(format!("body-id-{v}"));
            }
        }
    }

    if let Some(user) = obj.get("user").and_then(|v| v.as_str()) {
        let user = user.trim();
        if user.starts_with("session_") || user.starts_with("conv_") || user.starts_with("thread_") {
            return Some(format!("body-id-{user}"));
        }
    }

    None
}

/// Bodies smaller than this get no body-derived affinity: prompts below the
/// roughly 1Ki-token prompt-cache floor gain nothing from prefix stickiness,
/// and leaving tiny probes on strict round-robin preserves load fairness.
const BODY_AFFINITY_MIN_BYTES: usize = 4096;

/// How much of the request body head feeds the affinity hash.
const BODY_AFFINITY_HEAD_BYTES: usize = 2048;

/// Fallback affinity for clients that send no session header: hash the stable
/// head of the request body, so successive turns of one conversation keep
/// landing on the account that already holds their provider-side prompt cache.
fn body_prefix_affinity_key(body: &[u8]) -> Option<String> {
    if body.len() < BODY_AFFINITY_MIN_BYTES {
        return None;
    }
    let head = &body[..body.len().min(BODY_AFFINITY_HEAD_BYTES)];
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write(head);
    Some(format!("body-prefix-{:016x}", hasher.finish()))
}

pub async fn handle_relay(
    state: Arc<AppState>,
    auth: &crate::inbound::ResolvedAuth,
    mode: RelayMode,
    req_path: &str,
    headers: &HeaderMap,
    body_bytes: Bytes,
) -> Response {
    let request_started = std::time::Instant::now();
    let event_id = uuid::Uuid::new_v4().to_string();
    let occurred_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0);
    let key_identifier = auth.key_identifier.clone();
    let scoped_key = auth.identity.scoped().cloned();
    let scoped_entry = scoped_key.as_ref().and_then(|key| state.scoped_keys.get(&key.key_identifier));

    // 1. Quota check: if token budget is exhausted, reject with 429.
    if let Some(ref entry) = scoped_entry {
        if entry.is_exhausted() {
            let used = entry.token_used();
            let limit = entry.token_limit();
            return json_error(
                StatusCode::TOO_MANY_REQUESTS,
                &format!("Token quota exceeded for this API key (used: {used}, limit: {limit})"),
            );
        }
    }

    let binding = state.settings.current().api_key_bindings.iter()
        .find(|binding| Some(&binding.key_identifier) == key_identifier.as_ref()).cloned();
    let _in_flight = state.monitor.track_in_flight();
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0);
    let created = now_ms / 1000;

    let plan = match build_plan(mode, req_path, body_bytes) {
        Ok(plan) => plan,
        Err(message) => return json_error(StatusCode::BAD_REQUEST, &message),
    };

    // 2. Model check: if scoped key restricts allowed_models, enforce whitelist.
    if let Some(ref scoped) = scoped_key {
        let requested_model = plan.model.as_deref().unwrap_or_default();
        if !crate::models_route::allow_list_admits(&scoped.allowed_models, requested_model) {
            return json_error(
                StatusCode::FORBIDDEN,
                &format!("Model '{requested_model}' is not allowed for this API key"),
            );
        }
    }

    // One immutable generation governs resolution, eligibility, account
    // selection, and upstream model rewriting for the whole request.
    let pool = state.pool.load_full();
    let required_capability = match plan.mode {
        RelayMode::Image => Some(ModelCapability::Image),
        RelayMode::GeminiCountTokens => Some(ModelCapability::CountTokens),
        _ => None,
    };
    let route = match resolve_route(&pool, plan.model.as_deref(), required_capability) {
        Ok(route) => route,
        Err(_) => {
            let model = plan.model.as_deref().unwrap_or_default();
            return body_response(
                StatusCode::BAD_REQUEST,
                Some("application/json"),
                Bytes::from(crate::capability::unknown_provider(model).to_string()),
            );
        }
    };
    let eligible = eligible_indices(
        &pool,
        route.as_ref(),
        plan.model.as_deref(),
        now_ms,
        binding.as_ref(),
        scoped_key.as_deref(),
        &state,
    );
    if eligible.is_empty() && scoped_key.is_some() {
        return json_error(
            StatusCode::FORBIDDEN,
            "no permitted accounts/providers available for this API key",
        );
    }
    let max_attempts = std::cmp::min(eligible.len(), state.max_failover);
    if max_attempts == 0 {
        let model = plan.model.as_deref().unwrap_or_default();
        if !pool.members.is_empty() && route.is_some_and(|route| route.provider_classes.is_empty())
        {
            return body_response(
                StatusCode::BAD_REQUEST,
                Some("application/json"),
                Bytes::from(crate::capability::unknown_provider(model).to_string()),
            );
        }
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no available accounts for this model",
        );
    }

    let mut last_failure: Option<FinalFailure> = None;
    // Attribution follows the last account we ATTEMPTED, not the last one that
    // happened to answer with a buffered HTTP failure: transport and
    // proactive-refresh failures end an attempt without an HTTP response.
    let mut last_attempted: Option<(&'static str, String)> = None;
    let hint = SessionHint {
        affinity_key: affinity_key(headers)
            .or_else(|| body_affinity_key(plan.original_body.as_ref()))
            .or_else(|| body_prefix_affinity_key(plan.original_body.as_ref())),
    };
    // Accounts already tried in this request. 5xx and transport failures leave
    // health untouched (per contract), so exclusion is what forces the next
    // attempt onto a distinct account even when session affinity binds the
    // router to the failing one.
    let mut attempted: Vec<usize> = Vec::new();

    for _ in 0..max_attempts {
        let chosen_idx = match select_index(&state, &pool, &hint, &eligible, &attempted) {
            Some(idx) => idx,
            None => break,
        };
        let member = match pool.members.get(chosen_idx) {
            Some(m) => m.clone(),
            None => break,
        };
        attempted.push(chosen_idx);
        last_attempted = Some((member.kind().as_str(), member.id().to_string()));

        let mut refreshed_this_account = false;
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let member_at = member.access_token();

        if state.auth_refresh_enabled && member.is_expired(now_unix) {
            match state.refresh_member(&member, None).await {
                Ok(_) => refreshed_this_account = true,
                Err(e) => {
                    if e.is_auth_failure() {
                        member.set_health(Health::AuthFailed);
                        state.monitor.record_error(
                            member.id(),
                            401,
                            &format!("refresh failed: {e}"),
                        );
                    } else {
                        state.monitor.record_error(
                            member.id(),
                            502,
                            &format!("refresh network error: {e}"),
                        );
                    }
                    member.record_fail();
                    state.metrics.failed_over.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }
        }

        let upstream_model = route
            .as_ref()
            .and_then(|route| provider_for_member(route, &member))
            .map(|provider| provider.upstream_model.as_str())
            .or(plan.model.as_deref())
            .unwrap_or_default();
        let target = match resolve_target(&pool, &member, &member_at, &plan, upstream_model) {
            Ok(t) => t,
            Err(message) => return json_error(StatusCode::BAD_REQUEST, &message),
        };
        let target_url = target.url;
        // The MiMo anti-abuse gate inspects Accept the way it inspects the
        // User-Agent, so mirror what its own CLI client sends.
        let accept = (member.kind() == crate::account::ProviderKind::Generic
            && member.generic_adapter().as_deref() == Some("mimo-free"))
        .then_some(if plan.client_stream {
            "text/event-stream"
        } else {
            "application/json"
        });
        let exchange = match send_upstream(
            &state,
            &target_url,
            &member,
            headers,
            &target.body,
            target.protocol,
            accept,
            target.headers.as_deref(),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                member.record_fail();
                state
                    .scheduler
                    .record_non_auth_failure(member.id(), &state.pool.load().members);
                state.metrics.failed_over.fetch_add(1, Ordering::Relaxed);
                state
                    .monitor
                    .record_error(member.id(), 502, &format!("request error: {e}"));
                if member.kind() == crate::account::ProviderKind::Devin && e.is_ambiguous() {
                    return json_error(
                        StatusCode::GATEWAY_TIMEOUT,
                        &format!("ambiguous upstream request error: {e}"),
                    );
                }
                continue;
            }
        };
        let mut resp = exchange.response;
        let mut cursor_reply = exchange.cursor_reply;

        let mut status_code = resp.status().as_u16();

        // Only a 401 earns the refresh-and-replay: a 403 carries a valid
        // credential that was merely denied (workspace/entitlement/model), so
        // refreshing it changes nothing (opencodex #1789, 1fa0f6aad).
        if status_code == 401 && state.auth_refresh_enabled && !refreshed_this_account {
            match state.refresh_member(&member, Some(&member_at)).await {
                Ok(_) => {
                    match send_upstream(
                        &state,
                        &target_url,
                        &member,
                        headers,
                        &target.body,
                        target.protocol,
                        accept,
                        target.headers.as_deref(),
                    )
                    .await
                    {
                        Ok(retry_exchange) => {
                            resp = retry_exchange.response;
                            cursor_reply = retry_exchange.cursor_reply;
                            status_code = resp.status().as_u16();
                        }
                        Err(e) => {
                            member.record_fail();
                            state.metrics.failed_over.fetch_add(1, Ordering::Relaxed);
                            state.monitor.record_error(
                                member.id(),
                                502,
                                &format!("retry error: {e}"),
                            );
                            continue;
                        }
                    }
                }
                Err(e) => {
                    if e.is_auth_failure() {
                        member.set_health(Health::AuthFailed);
                        state.monitor.record_error(
                            member.id(),
                            status_code,
                            &format!("refresh failed: {e}"),
                        );
                    } else {
                        state.monitor.record_error(
                            member.id(),
                            502,
                            &format!("refresh network error: {e}"),
                        );
                    }
                    member.record_fail();
                    state.metrics.failed_over.fetch_add(1, Ordering::Relaxed);
                    last_failure = Some(extract_failure(resp, status_code).await);
                    continue;
                }
            }
        }

        if (200..=399).contains(&status_code) {
            match finish_success(
                &state,
                &member,
                &plan,
                headers,
                resp,
                status_code,
                created,
                compat::ProtocolSession {
                    protocol: target.protocol,
                    cursor_reply,
                },
            )
            .await
            {
                Ok(mut response) => {
                    let upstream_capture = response
                        .extensions_mut()
                        .remove::<Arc<std::sync::Mutex<Option<crate::usage::ResponseTokenUsage>>>>(
                        );
                    let devin_outcome = response
                        .extensions_mut()
                        .remove::<Arc<std::sync::Mutex<Option<compat::devin::DevinOutcome>>>>();
                    let (parts, body) = response.into_parts();
                    let bytes_in = plan.original_body.len();
                    // A buffered body is already fully in memory: finalize the
                    // record synchronously so stats and logs reflect the
                    // request the moment the client sees the response. Only
                    // true streams defer to the end-of-stream finalizer.
                    if http_body::Body::size_hint(&body).exact().is_some() {
                        let collected = match http_body_util::BodyExt::collect(body).await {
                            Ok(collected) => collected,
                            Err(error) => {
                                record_request_outcome(
                                    &state,
                                    OutcomeRecord {
                                        event_id: &event_id,
                                        occurred_at_ms,
                                        provider: member.kind().as_str(),
                                        account: Some(member.id()),
                                        model: plan.model.as_deref(),
                                        key_identifier: key_identifier.as_deref(),
                                        scoped_entry: scoped_entry.as_deref(),
                                        status: StatusCode::BAD_GATEWAY.as_u16(),
                                        success: false,
                                        elapsed_ms: request_started.elapsed().as_millis() as u64,
                                        bytes_in,
                                        bytes_out: 0,
                                        tokens: None,
                                        token_usage: None,
                                    },
                                )
                                .await;
                                return json_error(
                                    StatusCode::BAD_GATEWAY,
                                    &format!("buffered upstream body failed: {error}"),
                                );
                            }
                        };
                        let bytes = collected.to_bytes();
                        let token_usage = upstream_capture
                            .as_ref()
                            .and_then(|capture| {
                                *capture
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                            })
                            .or_else(|| crate::usage::extract_response_token_usage(&bytes, &bytes));
                        let tokens =
                            token_usage.map(crate::usage::ResponseTokenUsage::total_tokens);
                        record_request_outcome(
                            &state,
                            OutcomeRecord {
                                event_id: &event_id,
                                occurred_at_ms,
                                provider: member.kind().as_str(),
                                account: Some(member.id()),
                                model: plan.model.as_deref(),
                                key_identifier: key_identifier.as_deref(),
                                scoped_entry: scoped_entry.as_deref(),
                                status: status_code,
                                success: true,
                                elapsed_ms: request_started.elapsed().as_millis() as u64,
                                bytes_in,
                                bytes_out: bytes.len() as u64,
                                tokens,
                                token_usage,
                            },
                        )
                        .await;
                        return Response::from_parts(parts, Body::from(bytes));
                    }
                    let credential_token = member.access_token();
                    let outcome = StreamedOutcome {
                        state: Arc::clone(&state),
                        member: Arc::clone(&member),
                        credential_token,
                        event_id: event_id.clone(),
                        occurred_at_ms,
                        provider: member.kind().as_str().to_string(),
                        model: plan.model.clone(),
                        key_identifier: key_identifier.clone(),
                        scoped_entry: scoped_entry.clone(),
                        upstream_capture,
                        devin_outcome,
                        status: status_code,
                        started: request_started,
                        bytes_in,
                    };
                    let counted = CountedStream::new(body, outcome, _in_flight);
                    return Response::from_parts(parts, Body::from_stream(counted));
                }
                Err(reason) => {
                    let (mapped_status, code, description) =
                        if let Some((c, d)) = reason.split_once(": ") {
                            if let Some(norm) = compat::devin::normalize_connect_code(c) {
                                (connect_code_to_http_status(norm), norm, d)
                            } else {
                                (StatusCode::BAD_GATEWAY, "unknown", reason.as_str())
                            }
                        } else {
                            (StatusCode::BAD_GATEWAY, "unknown", reason.as_str())
                        };

                    member.record_fail();
                    state.metrics.failed_over.fetch_add(1, Ordering::Relaxed);
                    state
                        .monitor
                        .record_error(member.id(), mapped_status.as_u16(), description);

                    if code == "resource_exhausted" {
                        let now_ms = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
                            .unwrap_or(0);
                        member.set_health(Health::Cooldown {
                            until_unix_ms: cooldown_deadline_ms(now_ms, 300),
                        });
                    } else if code == "unauthenticated" {
                        member.set_health(Health::AuthFailed);
                    } else {
                        state
                            .scheduler
                            .record_non_auth_failure(member.id(), &state.pool.load().members);
                    }

                    last_failure = Some(FinalFailure {
                        status: mapped_status,
                        content_type: Some("application/json".to_string()),
                        body: Bytes::from(
                            serde_json::json!({"error": {"message": description, "type": "upstream_error", "code": code}})
                                .to_string(),
                        ),
                    });
                    if member.kind() == crate::account::ProviderKind::Devin
                        && (reason.contains("ended without EndStream")
                            || reason.contains("truncated")
                            || reason.contains("ambiguous"))
                    {
                        break;
                    }
                    continue;
                }
            }
        }

        if status_code == 429 {
            let failure = record_cooldown(resp, &member, status_code, &state).await;
            if member.provider_name() == "cline" {
                if let Some((model, reset_secs)) = parse_cline_cap_error(&failure.body) {
                    record_cline_quota_bucket(&member, &model, reset_secs, now_unix);
                }
            }
            last_failure = Some(failure);
            continue;
        }

        if (500..=504).contains(&status_code) {
            // Contract: ServerError leaves health unchanged. The account is
            // excluded from this request's remaining attempts, but a transient
            // upstream 5xx never benches it for other requests.
            member.record_fail();
            state
                .scheduler
                .record_non_auth_failure(member.id(), &state.pool.load().members);
            state.metrics.failed_over.fetch_add(1, Ordering::Relaxed);
            state
                .monitor
                .record_error(member.id(), status_code, "upstream server error");
            last_failure = Some(extract_failure(resp, status_code).await);
            continue;
        }

        let failure = extract_failure(resp, status_code).await;

        // A 403 is usually a workspace/entitlement or model denial, not a dead
        // credential (opencodex #1789 / 1fa0f6aad): model-rejection signatures
        // mark the model unsupported, and any other 403 fails over WITHOUT
        // quarantining the account — the scheduler's non-auth-failure streak
        // still benches a persistently denied account.
        if let Some(model) = plan.model.as_deref() {
            if is_account_scoped_model_rejection(status_code, &failure.body) {
                member.mark_model_unsupported(model);
                state.model_restrictions.store(true, Ordering::Relaxed);
                member.record_fail();
                state.metrics.failed_over.fetch_add(1, Ordering::Relaxed);
                state.monitor.record_error(
                    member.id(),
                    status_code,
                    "model not supported by account",
                );
                last_failure = Some(failure);
                continue;
            }
        }

        if status_code == 403 {
            member.record_fail();
            state
                .scheduler
                .record_non_auth_failure(member.id(), &state.pool.load().members);
            state.metrics.failed_over.fetch_add(1, Ordering::Relaxed);
            state
                .monitor
                .record_error(member.id(), status_code, "forbidden denial");
            last_failure = Some(failure);
            continue;
        }

        if status_code == 401 {
            // The refresh-and-replay above already ran for this account; a
            // second 401 means the credential is terminally broken.
            member.set_health(Health::AuthFailed);
            member.record_fail();
            state.metrics.failed_over.fetch_add(1, Ordering::Relaxed);
            state
                .monitor
                .record_error(member.id(), status_code, "auth failed");
            last_failure = Some(failure);
            continue;
        }

        state
            .metrics
            .exposed_client_errors
            .fetch_add(1, Ordering::Relaxed);
        state
            .monitor
            .record_error(member.id(), status_code, "client error");
        record_request_outcome(
            &state,
            OutcomeRecord {
                event_id: &event_id,
                occurred_at_ms,
                provider: member.kind().as_str(),
                account: Some(member.id()),
                model: plan.model.as_deref(),
                key_identifier: key_identifier.as_deref(),
                scoped_entry: scoped_entry.as_deref(),
                status: failure.status.as_u16(),
                success: false,
                elapsed_ms: request_started.elapsed().as_millis() as u64,
                bytes_in: plan.original_body.len(),
                bytes_out: failure.body.len() as u64,
                tokens: None,
                token_usage: None,
            },
        )
        .await;
        return body_response(
            failure.status,
            failure.content_type.as_deref(),
            failure.body,
        );
    }

    state.metrics.exposed_errors.fetch_add(1, Ordering::Relaxed);

    let (failure_provider, failure_account) = last_attempted
        .map(|(provider, account)| (Some(provider), Some(account)))
        .unwrap_or((None, None));

    let response = match last_failure {
        Some(final_fail) => body_response(
            final_fail.status,
            final_fail.content_type.as_deref(),
            final_fail.body,
        ),
        None if plan.mode == RelayMode::OpenAiCompat && plan.client_stream => Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(compat::error_stream_body("all failover attempts failed"))
            .unwrap_or_else(|_| (StatusCode::BAD_GATEWAY, "upstream failure").into_response()),
        None => json_error(StatusCode::BAD_GATEWAY, "all failover attempts failed"),
    };
    let (parts, body) = response.into_parts();
    let bytes_out = http_body::Body::size_hint(&body).exact().unwrap_or(0);
    let response = Response::from_parts(parts, body);
    record_request_outcome(
        &state,
        OutcomeRecord {
            event_id: &event_id,
            occurred_at_ms,
            provider: failure_provider.unwrap_or("unknown"),
            account: failure_account.as_deref(),
            model: plan.model.as_deref(),
            key_identifier: key_identifier.as_deref(),
            scoped_entry: scoped_entry.as_deref(),
            status: response.status().as_u16(),
            success: false,
            elapsed_ms: request_started.elapsed().as_millis() as u64,
            bytes_in: plan.original_body.len(),
            bytes_out,
            tokens: None,
            token_usage: None,
        },
    )
    .await;
    response
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    use crate::config::GatewayConfig;

    #[test]
    fn body_affinity_key_extracts_expected_fields() {
        // Top-level fields
        assert_eq!(
            body_affinity_key(br#"{"conversation_id":"c1"}"#).as_deref(),
            Some("body-id-c1")
        );
        assert_eq!(
            body_affinity_key(br#"{"conversation-id":"c2"}"#).as_deref(),
            Some("body-id-c2")
        );
        assert_eq!(
            body_affinity_key(br#"{"thread_id":"t1"}"#).as_deref(),
            Some("body-id-t1")
        );
        assert_eq!(
            body_affinity_key(br#"{"thread-id":"t2"}"#).as_deref(),
            Some("body-id-t2")
        );
        assert_eq!(
            body_affinity_key(br#"{"session_id":"s1"}"#).as_deref(),
            Some("body-id-s1")
        );
        assert_eq!(
            body_affinity_key(br#"{"session-id":"s2"}"#).as_deref(),
            Some("body-id-s2")
        );

        // Metadata fields
        assert_eq!(
            body_affinity_key(br#"{"metadata":{"thread_id":"m-t1"}}"#).as_deref(),
            Some("body-id-m-t1")
        );
        assert_eq!(
            body_affinity_key(br#"{"metadata":{"thread-id":"m-t2"}}"#).as_deref(),
            Some("body-id-m-t2")
        );
        assert_eq!(
            body_affinity_key(br#"{"metadata":{"conversation_id":"m-c1"}}"#).as_deref(),
            Some("body-id-m-c1")
        );
        assert_eq!(
            body_affinity_key(br#"{"metadata":{"conversation-id":"m-c2"}}"#).as_deref(),
            Some("body-id-m-c2")
        );
        assert_eq!(
            body_affinity_key(br#"{"metadata":{"session_id":"m-s1"}}"#).as_deref(),
            Some("body-id-m-s1")
        );
        assert_eq!(
            body_affinity_key(br#"{"metadata":{"session-id":"m-s2"}}"#).as_deref(),
            Some("body-id-m-s2")
        );

        // User prefix matches
        assert_eq!(
            body_affinity_key(br#"{"user":"session_user1"}"#).as_deref(),
            Some("body-id-session_user1")
        );
        assert_eq!(
            body_affinity_key(br#"{"user":"conv_user2"}"#).as_deref(),
            Some("body-id-conv_user2")
        );
        assert_eq!(
            body_affinity_key(br#"{"user":"thread_user3"}"#).as_deref(),
            Some("body-id-thread_user3")
        );

        // User string that does not match prefixes
        assert_eq!(body_affinity_key(br#"{"user":"regular_user"}"#), None);

        // Blank, empty, or missing
        assert_eq!(body_affinity_key(br#"{"conversation_id":""}"#), None);
        assert_eq!(body_affinity_key(br#"{"conversation_id":"  "}"#), None);
        assert_eq!(body_affinity_key(br#"{"other_field":"val"}"#), None);
        assert_eq!(body_affinity_key(b"not json"), None);
        assert_eq!(body_affinity_key(b"[]"), None);
    }

    #[test]
    fn body_prefix_affinity_skips_bodies_below_the_cacheable_threshold() {
        let small = br#"{"model":"codex","messages":[{"role":"user","content":"hi"}]}"#;
        assert_eq!(body_prefix_affinity_key(small), None);
    }

    #[test]
    fn body_prefix_affinity_is_decided_by_the_stable_head_alone() {
        let mut base = vec![b'x'; 6144];
        base[..8].copy_from_slice(b"{\"model\"");
        let head_key = body_prefix_affinity_key(&base).expect("large body gets a key");

        let mut tail_changed = base.clone();
        tail_changed[3000] = b'!';
        assert_eq!(
            body_prefix_affinity_key(&tail_changed).as_deref(),
            Some(head_key.as_str()),
            "changes beyond the hashed head must not rebind the account"
        );

        let mut head_changed = base;
        head_changed[0] = b'y';
        assert_ne!(
            body_prefix_affinity_key(&head_changed).as_deref(),
            Some(head_key.as_str()),
            "different heads may bind different accounts"
        );
    }

    fn credential(kind: &str) -> String {
        let extra = match kind {
            "codex" => {
                r#""account_id":"acc","id_token":"id","last_refresh":"2026-01-01T00:00:00Z","#
            }
            "antigravity" => r#""project_id":"project","#,
            "vertex" => r#""project_id":"project","location":"us-central1","#,
            "kiro" => r#""region":"us-east-1","#,
            _ => "",
        };
        format!(
            r#"{{{extra}"identity_slug":"{kind}","access_token":"token","refresh_token":"refresh","email":"{kind}@test.invalid","expired":"2099-01-01T00:00:00Z","type":"{kind}"}}"#
        )
    }

    fn six_provider_state() -> (AppState, std::path::PathBuf) {
        let auth_dir = std::env::temp_dir().join(format!(
            "mahoquot-routing-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&auth_dir).expect("create auth dir");
        for kind in [
            "codex",
            "antigravity",
            "claude",
            "cursor",
            "kiro",
            "zcode",
            "vertex",
        ] {
            std::fs::write(auth_dir.join(format!("{kind}-test.json")), credential(kind))
                .expect("write credential");
        }
        let config = GatewayConfig {
            auth_dir: auth_dir.clone(),
            config_path: auth_dir.join("config.yaml"),
            auth_refresh_enabled: false,
            ..GatewayConfig::default()
        };
        (AppState::new(&config).expect("state"), auth_dir)
    }

    fn state_with_credentials(
        tag: &str,
        credentials: &[(&str, String)],
    ) -> (AppState, std::path::PathBuf) {
        let auth_dir = std::env::temp_dir().join(format!(
            "mahoquot-routing-{tag}-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&auth_dir).expect("create auth dir");
        for (file, contents) in credentials {
            std::fs::write(auth_dir.join(file), contents).expect("write credential");
        }
        let config = GatewayConfig {
            auth_dir: auth_dir.clone(),
            config_path: auth_dir.join("config.yaml"),
            auth_refresh_enabled: false,
            ..GatewayConfig::default()
        };
        (AppState::new(&config).expect("state"), auth_dir)
    }

    #[test]
    fn explicit_alias_wins_over_a_longer_catalog_model_name() {
        // Given: an operator alias that happens to prefix another model name.
        let (state, auth_dir) = six_provider_state();
        let mut pool = (*state.pool.load_full()).clone();
        let mut registry = (*pool.registry).clone();
        registry.aliases.insert(
            mahoquot_registry::ModelId::new("claude-sonnet").unwrap(),
            mahoquot_registry::ModelAliasRule {
                alias: mahoquot_registry::ModelId::new("claude-sonnet").unwrap(),
                target: mahoquot_registry::ModelId::new("claude-3-7-sonnet-20250219").unwrap(),
                provider_id: None,
            },
        );
        pool.registry = Arc::new(registry);

        // When: resolving the explicit alias.
        let route = resolve_route(&pool, Some("claude-sonnet"), None).unwrap().unwrap();

        // Then: shorthand matching must not override the configured target.
        assert_eq!(route.canonical_model, "claude-3-7-sonnet-20250219");
        std::fs::remove_dir_all(auth_dir).unwrap();
    }

    #[test]
    fn prefixed_requests_cannot_bypass_model_exclusions() {
        // Given: both a known and a future Claude model explicitly excluded.
        let (state, auth_dir) = six_provider_state();
        let mut pool = (*state.pool.load_full()).clone();
        let mut registry = (*pool.registry).clone();
        for model in ["claude-3-7-sonnet-20250219", "claude-opus-5"] {
            registry.exclusions.insert(mahoquot_registry::ModelExclusionRule {
                model_id: mahoquot_registry::ModelId::new(model).unwrap(),
                provider_id: None,
            });
        }
        pool.registry = Arc::new(registry);

        // When: clients address either model through an official prefix.
        for model in ["anthropic-claude-3-7-sonnet-20250219", "anthropic-claude-opus-5"] {
            let result = resolve_route(&pool, Some(model), None);

            // Then: a fallback cannot erase the explicit exclusion.
            assert!(matches!(result, Err(mahoquot_registry::RegistryError::ModelExcluded { .. })));
        }
        std::fs::remove_dir_all(auth_dir).unwrap();
    }

    #[test]
    fn unrelated_model_prefix_does_not_select_a_catalog_variant() {
        // Given: Codex accepts unknown model IDs verbatim.
        let (state, auth_dir) = six_provider_state();
        let pool = state.pool.load_full();

        // When: an ID is a prefix of several catalog models but is not an alias.
        let route = resolve_route(&pool, Some("gpt"), None).unwrap().unwrap();

        // Then: do not silently substitute a different model.
        assert_eq!(route.canonical_model, "gpt");
        std::fs::remove_dir_all(auth_dir).unwrap();
    }

    #[test]
    fn resolved_routes_preserve_virtual_ids_and_provider_upstream_ids() {
        let (state, auth_dir) = six_provider_state();
        let pool = state.pool.load_full();

        let kiro = resolve_route(&pool, Some("auto-kiro"), None)
            .unwrap()
            .unwrap();
        assert_eq!(kiro.canonical_model, "kiro/auto");
        assert_eq!(
            kiro.provider_classes[0].binding.provider_id.as_str(),
            "kiro"
        );
        assert_eq!(kiro.provider_classes[0].upstream_model, "auto");

        let cursor = resolve_route(&pool, Some("cursor/auto-cost"), None)
            .unwrap()
            .unwrap();
        assert_eq!(cursor.canonical_model, "cursor/auto-cost");
        assert_eq!(
            cursor.provider_classes[0].binding.provider_id.as_str(),
            "cursor"
        );
        assert_eq!(cursor.provider_classes[0].upstream_model, "auto-cost");

        let vertex = resolve_route(&pool, Some("gemini-2.5-flash"), None)
            .unwrap()
            .unwrap();
        assert_eq!(
            vertex.provider_classes[0].binding.provider_id.as_str(),
            "vertex"
        );

        std::fs::remove_dir_all(auth_dir).ok();
    }

    #[test]
    fn generic_account_models_become_exact_discovered_bindings() {
        let generic = serde_json::json!({
            "type": "generic",
            "identity_slug": "deepseek-a",
            "provider": "deepseek",
            "label": "DeepSeek",
            "adapter": "openai-chat",
            "base_url": "http://127.0.0.1:9",
            "api_key": "fixture",
            "models": ["deepseek-chat"]
        })
        .to_string();
        let (state, auth_dir) =
            state_with_credentials("generic-discovered", &[("generic.json", generic)]);
        let pool = state.pool.load_full();
        let route = resolve_route(&pool, Some("deepseek-chat"), None)
            .unwrap()
            .unwrap();
        assert_eq!(
            route.provider_classes[0].binding.provider_id.as_str(),
            "deepseek"
        );
        assert_eq!(
            route.provider_classes[0].binding.policy,
            mahoquot_registry::ProviderPolicy::Discovered
        );
        let other = resolve_route(&pool, Some("other-deepseek-model"), None)
            .unwrap()
            .unwrap();
        assert!(other.provider_classes.is_empty());
        std::fs::remove_dir_all(auth_dir).ok();
    }

    #[test]
    fn unknown_model_uses_codex_only_when_a_codex_account_is_loaded() {
        let (with_codex, with_dir) =
            state_with_credentials("unknown-codex", &[("codex.json", credential("codex"))]);
        let route = resolve_route(&with_codex.pool.load_full(), Some("new-open-model"), None)
            .unwrap()
            .unwrap();
        assert_eq!(route.provider_classes.len(), 1);
        assert_eq!(
            route.provider_classes[0].binding.provider_id.as_str(),
            "codex"
        );

        let (without_codex, without_dir) =
            state_with_credentials("unknown-claude", &[("claude.json", credential("claude"))]);
        let route = resolve_route(
            &without_codex.pool.load_full(),
            Some("new-open-model"),
            None,
        )
        .unwrap()
        .unwrap();
        assert!(route.provider_classes.is_empty());

        std::fs::remove_dir_all(with_dir).ok();
        std::fs::remove_dir_all(without_dir).ok();
    }

    #[test]
    fn eligibility_applies_unsupported_health_scheduler_and_api_key_binding() {
        let credential_a = credential("codex").replace(
            "\"identity_slug\":\"codex\"",
            "\"identity_slug\":\"codex-a\"",
        );
        let credential_b = credential("codex").replace(
            "\"identity_slug\":\"codex\"",
            "\"identity_slug\":\"codex-b\"",
        );
        let (state, auth_dir) = state_with_credentials(
            "filters",
            &[("a.json", credential_a), ("b.json", credential_b)],
        );
        let pool = state.pool.load_full();
        let route = resolve_route(&pool, Some("gpt-5.6-sol"), None)
            .unwrap()
            .unwrap();

        pool.members[0].mark_model_unsupported("gpt-5.6-sol");
        let eligible = eligible_indices(
            &pool,
            Some(&route),
            Some("gpt-5.6-sol"),
            0,
            None,
            None,
            &state,
        );
        assert_eq!(eligible, vec![1]);

        pool.members[0].unsupported_models.write().unwrap().clear();
        pool.members[0].set_health(Health::AuthFailed);
        let eligible = eligible_indices(
            &pool,
            Some(&route),
            Some("gpt-5.6-sol"),
            0,
            None,
            None,
            &state,
        );
        assert_eq!(eligible, vec![1]);

        pool.members[0].set_health(Health::Available);
        state.scheduler.reserve("fixture", "codex-a").unwrap();
        let eligible = eligible_indices(
            &pool,
            Some(&route),
            Some("gpt-5.6-sol"),
            0,
            None,
            None,
            &state,
        );
        assert_eq!(eligible, vec![1]);

        let account_binding = crate::management::settings::ApiKeyBinding {
            key_identifier: "fixture".to_string(),
            account: Some("codex-a".to_string()),
            provider: None,
        };
        assert!(eligible_indices(
            &pool,
            Some(&route),
            Some("gpt-5.6-sol"),
            0,
            Some(&account_binding),
            None,
            &state,
        )
        .is_empty());
        state.scheduler.release("fixture");
        assert_eq!(
            eligible_indices(
                &pool,
                Some(&route),
                Some("gpt-5.6-sol"),
                0,
                Some(&account_binding),
                None,
                &state,
            ),
            vec![0]
        );

        std::fs::remove_dir_all(auth_dir).ok();
    }

    #[test]
    fn default_routing_never_selects_an_account_that_rejects_the_requested_model() {
        let (state, auth_dir) = six_provider_state();
        let hint = SessionHint { affinity_key: None };

        let pool = state.pool.load_full();
        for model in [
            "gpt-5.6-sol",
            "gemini-3.7-flash-high",
            "claude-sonnet-4-5-20250929",
            "glm-5.3",
            "kiro/claude-haiku-4-5-20251001",
            "cursor/auto",
        ] {
            let route = resolve_route(&pool, Some(model), None).unwrap().unwrap();
            let eligible =
                eligible_indices(&pool, Some(&route), Some(model), 0, None, None, &state);
            let selected = select_index(&state, &pool, &hint, &eligible, &[]).expect("selection");
            let provider = provider_for_member(&route, &pool.members[selected]).unwrap();
            assert_eq!(
                member_provider_id(&pool.members[selected]).as_ref(),
                Some(&provider.binding.provider_id),
                "model {model} was routed outside its resolved binding"
            );
        }

        std::fs::remove_dir_all(auth_dir).ok();
    }

    #[test]
    fn anthropic_and_nekos_prefix_routes_exclusively_to_official_vs_relay_accounts() {
        let official_cred = r#"{
            "type": "claude",
            "email": "sookyoung91@gmail.com",
            "identity_slug": "claude-official",
            "access_token": "token1",
            "refresh_token": "refresh1",
            "expired": "2099-01-01T00:00:00Z"
        }"#;
        let nekos_cred = r#"{
            "type": "claude",
            "email": "claude-ccapi",
            "identity_slug": "claude-ccapi",
            "api_key": "sk-clb-secret",
            "upstream_override": "https://ccapi.labs.mengmota.com/anthropic",
            "usage_override": "https://claude.nekos.me",
            "plan": "opus-standard"
        }"#;
        let (state, auth_dir) = state_with_credentials(
            "prefix-routing",
            &[
                ("claude-official.json", official_cred.to_string()),
                ("claude-ccapi.json", nekos_cred.to_string()),
            ],
        );
        let pool = state.pool.load_full();
        assert_eq!(pool.members.len(), 2);

        let official_idx = pool.members.iter().position(|m| !m.is_nekos_relay()).unwrap();
        let nekos_idx = pool.members.iter().position(|m| m.is_nekos_relay()).unwrap();
        assert_ne!(official_idx, nekos_idx);

        // 1. anthropic- prefixed model resolves and routes ONLY to official
        let route = resolve_route(&pool, Some("anthropic-claude-3-7-sonnet-20250219"), None)
            .unwrap()
            .unwrap();
        let eligible = eligible_indices(&pool, Some(&route), Some("anthropic-claude-3-7-sonnet-20250219"), 0, None, None, &state);
        assert_eq!(eligible, vec![official_idx]);

        // 2. anthropic/ slash-prefixed model routes ONLY to official
        let route = resolve_route(&pool, Some("anthropic/claude-3-7-sonnet-20250219"), None)
            .unwrap()
            .unwrap();
        let eligible = eligible_indices(&pool, Some(&route), Some("anthropic/claude-3-7-sonnet-20250219"), 0, None, None, &state);
        assert_eq!(eligible, vec![official_idx]);

        // 3. nekos- prefixed model resolves and routes ONLY to nekos
        let route = resolve_route(&pool, Some("nekos-claude-3-7-sonnet-20250219"), None)
            .unwrap()
            .unwrap();
        let eligible = eligible_indices(&pool, Some(&route), Some("nekos-claude-3-7-sonnet-20250219"), 0, None, None, &state);
        assert_eq!(eligible, vec![nekos_idx]);

        // 4. nekos/ slash-prefixed model routes ONLY to nekos
        let route = resolve_route(&pool, Some("nekos/claude-3-7-sonnet-20250219"), None)
            .unwrap()
            .unwrap();
        let eligible = eligible_indices(&pool, Some(&route), Some("nekos/claude-3-7-sonnet-20250219"), 0, None, None, &state);
        assert_eq!(eligible, vec![nekos_idx]);

        // 5. Bare unprefixed model routes to BOTH accounts
        let route = resolve_route(&pool, Some("claude-3-7-sonnet-20250219"), None)
            .unwrap()
            .unwrap();
        let mut eligible = eligible_indices(&pool, Some(&route), Some("claude-3-7-sonnet-20250219"), 0, None, None, &state);
        eligible.sort();
        let mut expected = vec![official_idx, nekos_idx];
        expected.sort();
        assert_eq!(eligible, expected);

        // 6. Date-less shorthand models also resolve properly
        let route = resolve_route(&pool, Some("anthropic-claude-3-7-sonnet"), None)
            .unwrap()
            .unwrap();
        let eligible = eligible_indices(&pool, Some(&route), Some("anthropic-claude-3-7-sonnet"), 0, None, None, &state);
        assert_eq!(eligible, vec![official_idx]);

        let route = resolve_route(&pool, Some("nekos-claude-3-7-sonnet"), None)
            .unwrap()
            .unwrap();
        let eligible = eligible_indices(&pool, Some(&route), Some("nekos-claude-3-7-sonnet"), 0, None, None, &state);
        assert_eq!(eligible, vec![nekos_idx]);

        // 7. Uncataloged future Claude models (e.g. claude-opus-5) route cleanly
        let route = resolve_route(&pool, Some("anthropic-claude-opus-5"), None)
            .unwrap()
            .unwrap();
        let eligible = eligible_indices(&pool, Some(&route), Some("anthropic-claude-opus-5"), 0, None, None, &state);
        assert_eq!(eligible, vec![official_idx]);

        let route = resolve_route(&pool, Some("nekos-claude-opus-5"), None)
            .unwrap()
            .unwrap();
        let eligible = eligible_indices(&pool, Some(&route), Some("nekos-claude-opus-5"), 0, None, None, &state);
        assert_eq!(eligible, vec![nekos_idx]);

        std::fs::remove_dir_all(auth_dir).ok();
    }
}
