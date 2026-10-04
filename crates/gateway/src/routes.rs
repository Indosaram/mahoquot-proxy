use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{from_fn, from_fn_with_state, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use mahoquot_types::{Health, PoolMember};

use crate::cp_routes;
use crate::inbound::require_api_key;
use crate::models_route::models_payload;
use crate::monitor::PromAccount;
use crate::relay::{handle_relay, RelayMode};
use crate::state::AppState;
use axum::Extension;
use crate::inbound::ResolvedAuth;

// Agent clients replay the whole conversation on every turn, so an inbound
// request legitimately carries megabytes of history; axum's 2 MiB default
// answered those with 413 once a session grew. A 2M-token context window with
// inline multimodal assets (base64 images, PDFs) requires up to several hundred
// megabytes. 512 MiB provides headroom for 2M+ tokens without unbounded memory risk.
const MAX_REQUEST_BODY_BYTES: usize = 512 * 1024 * 1024;

pub fn create_app(state: Arc<AppState>) -> Router {
    // /admin/stats stays behind the key: it exposes account emails and reset times.
    let authed_routes = Router::new()
        .route("/v1/models", get(models_handler))
        .route("/models", get(models_handler))
        .route("/v1", get(cp_routes::root))
        .route("/v1/", get(cp_routes::root))
        .route("/v1beta/models", get(cp_routes::v1beta_models))
        .route(
            "/v1beta/models/{*action}",
            get(cp_routes::v1beta_action).post(cp_routes::v1beta_action),
        )
        .route(
            "/models/{*action}",
            get(cp_routes::v1beta_action).post(cp_routes::v1beta_action),
        )
        .route("/admin/stats", get(admin_stats_handler))
        .route("/admin/usage", get(admin_usage_handler))
        .route("/admin/warmup", post(admin_warmup_handler))
        .route(
            "/admin/accounts/{id}/warmup",
            post(admin_warmup_one_handler),
        )
        .route("/admin/usage/refresh", post(admin_usage_refresh_handler))
        .route("/admin/accounts/{id}/reset", post(admin_reset_handler))
        .route(
            "/api/codex-auth/accounts/credits",
            get(crate::management::accounts::get_codex_credits_opt_in)
                .put(crate::management::accounts::put_codex_credits_opt_in),
        )
        .route("/v1/chat/completions", post(chat_completions_handler))
        .route("/chat/completions", post(chat_completions_handler))
        .route("/v1/completions", post(completions_handler))
        .route("/completions", post(completions_handler))
        .route("/v1/messages", post(messages_handler))
        .route("/messages", post(messages_handler))
        .route("/v1/messages/count_tokens", post(count_tokens_handler))
        .route("/messages/count_tokens", post(count_tokens_handler))
        .route(
            "/backend-api/codex/responses",
            get(cp_routes::ws_upgrade).post(codex_responses_handler),
        )
        .route(
            "/backend-api/codex/responses/compact",
            post(cp_routes::responses_compact),
        )
        .route(
            "/backend-api/codex/alpha/search",
            post(cp_routes::alpha_search),
        )
        .route(
            "/v1/responses",
            get(cp_routes::ws_upgrade).post(cp_routes::responses),
        )
        .route(
            "/responses",
            get(cp_routes::ws_upgrade).post(cp_routes::responses),
        )
        .route("/v1/responses/compact", post(cp_routes::responses_compact))
        .route("/responses/compact", post(cp_routes::responses_compact))
        .route("/v1/alpha/search", post(cp_routes::alpha_search))
        .route("/alpha/search", post(cp_routes::alpha_search))
        .route(
            "/v1/images/generations",
            post(cp_routes::images_generations),
        )
        .route("/images/generations", post(cp_routes::images_generations))
        .route("/v1/images/edits", post(cp_routes::images_edits))
        .route("/images/edits", post(cp_routes::images_edits))
        .route("/v1/videos", post(cp_routes::videos))
        .route("/videos", post(cp_routes::videos))
        .route("/v1/videos/generations", post(cp_routes::videos))
        .route("/videos/generations", post(cp_routes::videos))
        .route("/v1/videos/edits", post(cp_routes::videos))
        .route("/videos/edits", post(cp_routes::videos))
        .route("/v1/videos/extensions", post(cp_routes::videos))
        .route("/videos/extensions", post(cp_routes::videos))
        .route("/v1/videos/{request_id}", get(cp_routes::videos_by_id))
        .route("/videos/{request_id}", get(cp_routes::videos_by_id))
        .route("/openai/v1/videos", post(cp_routes::openai_videos))
        .route(
            "/openai/v1/videos/{video_id}",
            get(cp_routes::openai_videos),
        )
        .route(
            "/openai/v1/videos/{video_id}/content",
            get(cp_routes::openai_videos),
        )
        .route("/videos/{video_id}/content", get(cp_routes::openai_videos))
        .route("/v1/live", post(cp_routes::realtime_offer))
        .route("/live", post(cp_routes::realtime_offer))
        .route("/v1/live/{call_id}", get(cp_routes::live_sideband))
        .route("/live/{call_id}", get(cp_routes::live_sideband))
        .route(
            "/v1/realtime",
            get(cp_routes::ws_upgrade).post(cp_routes::realtime_offer),
        )
        .route("/v1/realtime/calls", post(cp_routes::realtime_offer))
        .route(
            "/v1/realtime/calls/{call_id}",
            get(cp_routes::realtime_call_get),
        )
        .route(
            "/v1/realtime/calls/{call_id}/hangup",
            post(cp_routes::realtime_hangup),
        )
        .route(
            "/v1/realtime/calls/{call_id}/accept",
            post(cp_routes::realtime_sip_accept),
        )
        .route(
            "/v1/realtime/calls/{call_id}/reject",
            post(cp_routes::realtime_sip_reject),
        )
        .route(
            "/v1/realtime/calls/{call_id}/refer",
            post(cp_routes::realtime_sip_refer),
        )
        .route(
            "/v1/realtime/calls/{call_id}/{action}",
            post(cp_routes::realtime_sip),
        )
        .route(
            "/v1/realtime/client_secrets",
            post(cp_routes::realtime_client_secrets),
        )
        .route("/v1/realtime/sessions", post(cp_routes::realtime_sessions))
        .route(
            "/v1/realtime/transcription_sessions",
            post(cp_routes::realtime_transcription),
        )
        .route(
            "/v1/realtime/translations",
            get(cp_routes::realtime_translations).post(cp_routes::realtime_translations),
        )
        .route(
            "/v1/realtime/translations/client_secrets",
            post(cp_routes::realtime_translations),
        )
        .route("/v1beta/interactions", post(cp_routes::v1beta_interactions))
        .route("/interactions", post(cp_routes::v1beta_interactions))
        .layer(from_fn_with_state(
            Arc::clone(&state),
            inference_admission,
        ))
        .layer(from_fn_with_state(
            Arc::new(crate::inbound::ApiKeys::with_live_settings(
                Arc::clone(&state.settings),
                crate::inbound::ApiKeys::new(state.api_keys.values().to_vec()),
            )),
            require_api_key,
        ));

    // Public surface: Prometheus scrapers and liveness probes never send credentials,
    // and CLIProxyAPI exposes its metrics endpoint unauthenticated too.
    Router::new()
        .route("/healthz", get(healthz_handler))
        .route("/keep-alive", get(keep_alive_handler))
        .route("/metrics", get(metrics_handler))
        .route("/", get(cp_routes::root))
        .route("/management.html", get(cp_routes::management_html))
        .route("/anthropic/callback", get(cp_routes::oauth_callback))
        .route("/codex/callback", get(cp_routes::oauth_callback))
        .route("/antigravity/callback", get(cp_routes::oauth_callback))
        .route(
            "/oauth-callback",
            get(crate::management::oauth::oauth_callback)
                .post(crate::management::oauth::oauth_callback),
        )
        .route(
            "/v0/management/oauth-callback",
            get(crate::management::oauth::oauth_callback)
                .post(crate::management::oauth::oauth_callback),
        )
        .nest(
            "/v0/management",
            crate::management::management_router(Arc::clone(&state)),
        )
        .merge(authed_routes)
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .layer(from_fn(cors))
        .with_state(state)
}

/// CP answers every route with wildcard CORS and short-circuits preflight with
/// 204, so browser clients pointed at either proxy behave identically.
async fn cors(method: Method, req: axum::extract::Request, next: Next) -> Response {
    let mut response = if method == Method::OPTIONS {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(req).await
    };
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        // WebKit does not honor a wildcard for Authorization, so the desktop
        // webview console needs it named explicitly.
        HeaderValue::from_static("Authorization, Content-Type"),
    );
    response
}

/// Wraps a response body so it owns one inference permit until the body is
/// fully consumed or dropped by the client.
struct PermitBody<B> {
    inner: B,
    permit: std::cell::RefCell<Option<tokio::sync::OwnedSemaphorePermit>>,
}

impl<B> http_body::Body for PermitBody<B>
where
    B: http_body::Body + Unpin,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let polled = std::pin::Pin::new(&mut this.inner).poll_frame(cx);
        if matches!(
            &polled,
            std::task::Poll::Ready(None) | std::task::Poll::Ready(Some(Err(_)))
        ) {
            this.permit.get_mut().take();
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// Requests behind the authenticated surface that reach an upstream model:
/// inference, media and realtime relays. Model listing, informational roots,
/// the admin control plane and the local token counter are excluded so the
/// gateway stays administrable while the inference gate is saturated.
fn is_inference_surface(method: &Method, path: &str) -> bool {
    if method == Method::OPTIONS || path.starts_with("/admin/") {
        return false;
    }
    !matches!(
        path,
        "/api/codex-auth/accounts/credits"
            | "/v1/models"
            | "/models"
            | "/v1"
            | "/v1/"
            | "/v1beta/models"
            | "/v1/messages/count_tokens"
            | "/messages/count_tokens"
    )
}

/// Admission control for the inference surface.
///
/// The permit is taken before the handler extracts the request body, so a
/// chunked or length-less upload is rejected identically to a sized one, and it
/// is moved into the response body wrapper, so it lives for as long as the
/// downstream read does. Exhaustion answers immediately with a retryable 503:
/// there is no queue and no timeout. The 512 MiB body ceiling is unchanged —
/// this bounds concurrency, never request size.
async fn inference_admission(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if !is_inference_surface(request.method(), request.uri().path()) {
        return next.run(request).await;
    }

    if request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_REQUEST_BODY_BYTES as u64)
    {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }

    let permit = match state.inference_gate().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": {
                        "code": "gateway_busy",
                        "message": "too many concurrent inference requests",
                        "retryable": true
                    }
                })),
            )
                .into_response();
        }
    };

    let response = next.run(request).await;
    response.map(|body| {
        axum::body::Body::new(PermitBody {
            inner: body,
            permit: std::cell::RefCell::new(Some(permit)),
        })
    })
}

#[cfg(test)]
mod permit_body_tests {
    use super::*;
    use http_body::Body as _;
    use http_body::{Frame, SizeHint};
    use std::convert::Infallible;
    use std::task::{Context, Poll};

    struct OneFrameThenEnd(bool);

    impl http_body::Body for OneFrameThenEnd {
        type Data = bytes::Bytes;
        type Error = Infallible;

        fn poll_frame(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            if self.0 {
                Poll::Ready(None)
            } else {
                self.0 = true;
                Poll::Ready(Some(Ok(Frame::data(bytes::Bytes::from_static(b"body")))))
            }
        }

        fn is_end_stream(&self) -> bool {
            self.0
        }

        fn size_hint(&self) -> SizeHint {
            SizeHint::default()
        }
    }

    #[test]
    fn permit_is_released_at_eof_while_body_remains_alive() {
        let gate = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = gate.clone().try_acquire_owned().unwrap();
        let mut body = PermitBody {
            inner: OneFrameThenEnd(false),
            permit: std::cell::RefCell::new(Some(permit)),
        };
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(matches!(std::pin::Pin::new(&mut body).poll_frame(&mut cx), Poll::Ready(Some(Ok(_)))));
        assert_eq!(gate.available_permits(), 0);
        assert!(matches!(std::pin::Pin::new(&mut body).poll_frame(&mut cx), Poll::Ready(None)));
        assert_eq!(gate.available_permits(), 1);
        drop(body);
        assert_eq!(gate.available_permits(), 1);
    }

    #[test]
    fn permit_is_released_when_body_reports_end_stream() {
        let gate = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = gate.clone().try_acquire_owned().unwrap();
        let mut body = PermitBody {
            inner: OneFrameThenEnd(true),
            permit: std::cell::RefCell::new(Some(permit)),
        };
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(body.is_end_stream());
        assert!(matches!(std::pin::Pin::new(&mut body).poll_frame(&mut cx), Poll::Ready(None)));
        assert_eq!(gate.available_permits(), 1);
        drop(body);
        assert_eq!(gate.available_permits(), 1);
    }

}

async fn healthz_handler() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "api_schema": 1,
    }))
}

async fn keep_alive_handler() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
}

async fn models_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
) -> impl IntoResponse {
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let scoped_key = auth.identity.scoped();

    let pool = state.pool.load();
    // A scoped key must not learn about models it could never route to, so the
    // catalog is narrowed to its own providers, accounts and model allow list.
    let models = match scoped_key {
        Some(scoped) => crate::models_route::scoped_model_entries(&pool, scoped),
        None => pool.models.clone(),
    };

    Json(models_payload(&models, now_unix))
}

async fn metrics_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let now_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let accounts: Vec<PromAccount> = state
        .pool
        .load()
        .members
        .iter()
        .map(|m| {
            let cooldown_until_unix_ms = match m.health() {
                Health::Cooldown { until_unix_ms } => Some(until_unix_ms),
                _ => None,
            };
            PromAccount {
                id: m.id.clone(),
                ok: m.ok_count.load(Ordering::Relaxed),
                fails: m.fail_count.load(Ordering::Relaxed),
                cooldown_until_unix_ms,
            }
        })
        .collect();

    let mut body = state.monitor.render_prometheus(now_unix_ms, &accounts);
    body.push_str(&state.metrics.registry_refresh.render_prometheus());
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body)
}

async fn admin_stats_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.get_stats())
}

async fn admin_usage_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.get_stats())
}

async fn admin_warmup_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
) -> Response {
    if auth.identity.is_scoped() {
        return StatusCode::FORBIDDEN.into_response();
    }
    Json(serde_json::json!({ "results": crate::warmup::warm_all(&state).await })).into_response()
}

async fn admin_warmup_one_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if auth.identity.is_scoped() {
        return StatusCode::FORBIDDEN.into_response();
    }
    match state.find_member(&id) {
        Some(member) => Json(crate::warmup::warm_account(&state, &member).await).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "unknown account" })),
        )
            .into_response(),
    }
}

async fn admin_usage_refresh_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    crate::quota::refresh_all_usage(&state).await;
    Json(state.get_stats())
}

async fn admin_reset_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let member = {
        let pool = state.pool.load();
        pool.members
            .iter()
            .find(|member| {
                member.id == id && member.kind() == crate::account::ProviderKind::Codex
            })
            .or_else(|| pool.members.iter().find(|member| member.id == id))
            .cloned()
    };
    let Some(member) = member else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "unknown account" })),
        )
            .into_response();
    };
    match crate::quota::consume_reset_credit(&state, &member).await {
        Ok(()) => Json(serde_json::json!({
            "ok": true,
            "id": id,
            "usage": member.usage_snapshot(),
        }))
        .into_response(),
        Err(e) => (
            crate::quota::reset_error_status(&e),
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn chat_completions_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_relay(
        state,
        &auth,
        RelayMode::OpenAiCompat,
        "/v1/chat/completions",
        &headers,
        body,
    )
    .await
}

async fn messages_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_relay(state, &auth, RelayMode::Anthropic, "/v1/messages", &headers, body).await
}

async fn count_tokens_handler(State(state): State<Arc<AppState>>, Extension(auth): Extension<ResolvedAuth>, body: Bytes) -> Response {
    let parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "type": "error",
                    "error": { "type": "invalid_request_error", "message": e.to_string() }
                })),
            )
                .into_response()
        }
    };
    let model = parsed
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let snapshot = state.pool.load();
    if let Some(key) = auth.identity.scoped() {
        if !crate::models_route::scoped_model_entries(&snapshot, key).iter().any(|entry| entry.id == model) {
            return (StatusCode::FORBIDDEN, Json(serde_json::json!({"error":{"type":"permission_error"}}))).into_response();
        }
    }
    let is_supported = crate::capability::resolve_for_capability(
            &snapshot,
            model,
            mahoquot_registry::ModelCapability::CountTokens,
        )
        .is_some();

    if !is_supported {
        return (
            StatusCode::BAD_REQUEST,
            Json(crate::capability::count_tokens_error(model)),
        )
            .into_response();
    }
    Json(serde_json::json!({
        "input_tokens": crate::compat::estimate_input_tokens(&parsed)
    }))
    .into_response()
}

/// Legacy text-completions clients send `prompt`; lift it into the chat shape
/// so one relay path serves both surfaces.
async fn completions_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": { "message": e.to_string() } })),
            )
                .into_response()
        }
    };

    if parsed.get("tools").is_some() || parsed.get("functions").is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": {
                    "message": "tools and functions are not supported on legacy completions",
                    "type": "invalid_request_error"
                }
            })),
        )
            .into_response();
    }

    let prompt = parsed
        .get("prompt")
        .and_then(|p| match p {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Array(items) => Some(
                items
                    .iter()
                    .filter_map(|i| i.as_str())
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        })
        .unwrap_or_default();

    let mut chat = parsed.clone();
    if let Some(obj) = chat.as_object_mut() {
        obj.remove("prompt");
        obj.insert(
            "messages".to_string(),
            serde_json::json!([{ "role": "user", "content": prompt }]),
        );
    }

    handle_relay(
        state,
        &auth,
        RelayMode::LegacyCompletions,
        "/v1/chat/completions",
        &headers,
        Bytes::from(chat.to_string()),
    )
    .await
}

async fn codex_responses_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ResolvedAuth>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mode = if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&body) {
        let model = crate::capability::model_of(&parsed);
        if crate::cp_routes::owner_of(&state, model).as_deref() == Some("devin") {
            RelayMode::Responses
        } else {
            RelayMode::Native
        }
    } else {
        RelayMode::Native
    };
    handle_relay(
        state,
        &auth,
        mode,
        "/backend-api/codex/responses",
        &headers,
        body,
    )
    .await
}
