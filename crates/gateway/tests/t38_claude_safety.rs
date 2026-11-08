//! Claude account safety (plan W0-W6) through the real relay, against fake
//! Anthropic upstreams.
mod common;

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use common::unique_temp_dir;
use http_body_util::BodyExt;
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use mahoquot_types::{Health, PoolMember, Strategy};
use tower::ServiceExt;

const MODEL: &str = "claude-sonnet-4-5-20250929";
const OK_MESSAGE: &str = r#"{"id":"msg_ok","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
const DISABLED_403: &str = r#"{"type":"error","error":{"type":"permission_error","message":"This organization has been disabled."}}"#;
const PLAIN_403: &str = r#"{"type":"error","error":{"type":"permission_error","message":"Request not allowed"}}"#;
const CLAUDE_CODE_ONLY_401: &str = r#"{"type":"error","error":{"type":"authentication_error","message":"This credential is only authorized for use with Claude Code and cannot be used for other API requests."}}"#;
const OVERLOADED_529: &str = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
const UNAVAILABLE_503: &str = r#"{"type":"error","error":{"type":"api_error","message":"unavailable"}}"#;
const SSE_DISABLED: &str = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"permission_error\",\"message\":\"This organization has been disabled.\"}}\n\n";

type Scripted = (u16, &'static str, &'static str);

#[derive(Clone, Default)]
struct Upstream {
    hits: Arc<AtomicUsize>,
    script: Arc<Mutex<VecDeque<Scripted>>>,
    seen: Arc<Mutex<Vec<(HeaderMap, serde_json::Value)>>>,
}

impl Upstream {
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn then(&self, status: u16, content_type: &'static str, body: &'static str) {
        self.script
            .lock()
            .unwrap()
            .push_back((status, content_type, body));
    }

    fn last(&self) -> (HeaderMap, serde_json::Value) {
        self.seen.lock().unwrap().last().cloned().expect("an upstream call")
    }
}

struct Fixture {
    dir: std::path::PathBuf,
    servers: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for server in &self.servers {
            server.abort();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Fixture {
    fn new(prefix: &str) -> Self {
        Self {
            dir: unique_temp_dir(prefix),
            servers: Vec::new(),
        }
    }

    async fn upstream(&mut self) -> (String, Upstream) {
        let upstream = Upstream::default();
        let handle = upstream.clone();
        let app = Router::new().route(
            "/v1/messages",
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let upstream = handle.clone();
                async move {
                    upstream.hits.fetch_add(1, Ordering::SeqCst);
                    let value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                    upstream.seen.lock().unwrap().push((headers, value));
                    let (status, content_type, body) = upstream
                        .script
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or((200, "application/json", OK_MESSAGE));
                    (
                        StatusCode::from_u16(status).unwrap(),
                        [("content-type", content_type)],
                        body,
                    )
                        .into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        self.servers.push(tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        }));
        (url, upstream)
    }

    fn oauth(&self, id: &str, upstream: &str) {
        write_claude(&self.dir, id, upstream, None);
    }

    fn api_key(&self, id: &str, upstream: &str) {
        write_claude(&self.dir, id, upstream, Some("sk-ant-api-test"));
    }

    fn state(&self) -> Arc<AppState> {
        self.state_with_config(None)
    }

    fn state_with_config(&self, yaml: Option<&str>) -> Arc<AppState> {
        let config_path = self.dir.join("config.yaml");
        if let Some(yaml) = yaml {
            std::fs::write(&config_path, yaml).unwrap();
        } else if !config_path.exists() {
            std::fs::write(&config_path, "logging-to-file: false\n").unwrap();
        }
        Arc::new(
            AppState::new(&GatewayConfig {
                auth_dir: self.dir.clone(),
                config_path,
                auth_refresh_enabled: false,
                max_failover: 3,
                strategy: Strategy::StrictRoundRobin,
                usage_poll_secs: 120,
                port: 0,
                ..GatewayConfig::default()
            })
            .expect("state"),
        )
    }
}

fn write_claude(dir: &Path, id: &str, upstream: &str, api_key: Option<&str>) {
    let mut credential = serde_json::json!({
        "type": "claude",
        "identity_slug": id,
        "email": format!("{id}@example.test"),
        "access_token": format!("token-{id}"),
        "refresh_token": format!("refresh-{id}"),
        "expired": "2099-01-01T00:00:00Z",
        "upstream_override": upstream,
    });
    if let Some(key) = api_key {
        credential["api_key"] = serde_json::Value::String(key.to_string());
    }
    std::fs::write(dir.join(format!("claude-{id}.json")), credential.to_string()).unwrap();
}

async fn send(
    state: &Arc<AppState>,
    headers: &[(&str, &str)],
    stream: bool,
) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::post("/v1/messages").header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body = serde_json::json!({
        "model": MODEL,
        "max_tokens": 16,
        "stream": stream,
        "messages": [{"role": "user", "content": "ping"}],
    });
    let response = create_app(Arc::clone(state))
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&bytes).to_string())
}

fn suspected(state: &AppState) -> Vec<String> {
    state
        .settings
        .current()
        .claude_suspected_accounts
        .keys()
        .cloned()
        .collect()
}

fn member_health(state: &AppState, id: &str, health: Health) {
    let pool = state.pool.load();
    let member = pool.members.iter().find(|m| m.id() == id).unwrap();
    member.set_health(health);
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

const SESSION: (&str, &str) = ("x-claude-code-session-id", "session-one");

#[tokio::test]
async fn an_enforcement_403_benches_the_account_without_trying_another() {
    let mut fx = Fixture::new("claude-safety-enforce");
    let (url, upstream) = fx.upstream().await;
    upstream.then(403, "application/json", DISABLED_403);
    fx.oauth("alpha", &url);
    fx.oauth("beta", &url);
    let state = fx.state();

    let (status, _, body) = send(&state, &[], false).await;

    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("has been disabled"), "{body}");
    assert_eq!(upstream.hits(), 1, "no second account sees the request");
    assert_eq!(suspected(&state).len(), 1);
    let benched = suspected(&state)[0].clone();
    let reason = state.settings.current().claude_suspected_accounts[&benched]
        .reason
        .clone();
    assert!(!reason.contains("This organization"), "reason names the signature only: {reason}");

    // A restart reads the list back from settings: the benched account stays out.
    drop(state);
    let restarted = fx.state();
    assert_eq!(suspected(&restarted), vec![benched.clone()]);
    for _ in 0..3 {
        let (status, _, body) = send(&restarted, &[], false).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    assert_eq!(upstream.hits(), 4);
    let served_by_benched = upstream
        .seen
        .lock()
        .unwrap()
        .iter()
        .skip(1)
        .filter(|(headers, _)| {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                == Some(&format!("Bearer token-{benched}"))
        })
        .count();
    assert_eq!(served_by_benched, 0);

    // The operator clears it through the management route. Origin's
    // management surface is fail-closed: it answers 404 until a secret is
    // configured, and only trusts the transport peer, so the fixture sets
    // both the way the other management tests do.
    restarted
        .settings
        .mutate(|document| {
            document.remote_management.secret_key = "management".into();
        })
        .expect("management secret fixture");
    let response = create_app(Arc::clone(&restarted))
        .oneshot(
            Request::delete(format!("/v0/management/accounts/{benched}/suspected"))
                .header("x-management-key", "management")
                .extension(axum::extract::ConnectInfo(
                    "127.0.0.1:18840".parse::<std::net::SocketAddr>().unwrap(),
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(suspected(&restarted).is_empty());
}

#[tokio::test]
async fn an_enforcement_401_skips_refresh_and_failover() {
    let mut fx = Fixture::new("claude-safety-401");
    let (url, upstream) = fx.upstream().await;
    upstream.then(401, "application/json", CLAUDE_CODE_ONLY_401);
    fx.oauth("alpha", &url);
    fx.oauth("beta", &url);
    let state = fx.state();

    let (status, _, body) = send(&state, &[], false).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert!(body.contains("only authorized for use with Claude Code"), "{body}");
    assert_eq!(upstream.hits(), 1);
    assert_eq!(state.refreshed.load(Ordering::Relaxed), 0);
    assert_eq!(suspected(&state).len(), 1);
}

#[tokio::test]
async fn an_early_sse_enforcement_error_is_not_retried() {
    let mut fx = Fixture::new("claude-safety-sse");
    let (url, upstream) = fx.upstream().await;
    upstream.then(200, "text/event-stream", SSE_DISABLED);
    fx.oauth("alpha", &url);
    fx.oauth("beta", &url);
    let state = fx.state();

    let (status, _, body) = send(&state, &[], true).await;

    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["error"]["code"], "permission_denied");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("has been disabled"));
    assert_eq!(upstream.hits(), 1);
    assert_eq!(suspected(&state).len(), 1);
}

#[tokio::test]
async fn a_plain_403_is_returned_and_a_second_one_benches_the_account() {
    let mut fx = Fixture::new("claude-safety-denied");
    let (url, upstream) = fx.upstream().await;
    upstream.then(403, "application/json", PLAIN_403);
    upstream.then(403, "application/json", PLAIN_403);
    fx.oauth("alpha", &url);
    let state = fx.state();

    let (status, _, _) = send(&state, &[], false).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(suspected(&state).is_empty(), "one denial is not enforcement");

    let (status, _, _) = send(&state, &[], false).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(suspected(&state), vec!["alpha".to_string()]);
    assert_eq!(upstream.hits(), 2);
}

#[tokio::test]
async fn a_529_fails_over_like_a_5xx() {
    let mut fx = Fixture::new("claude-safety-529");
    let (url, upstream) = fx.upstream().await;
    upstream.then(529, "application/json", OVERLOADED_529);
    fx.oauth("alpha", &url);
    fx.oauth("beta", &url);
    let state = fx.state();

    let (status, _, body) = send(&state, &[], false).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(upstream.hits(), 2);
    assert!(suspected(&state).is_empty());
}

async fn two_accounts(prefix: &str) -> (Fixture, Upstream, Upstream) {
    let mut fx = Fixture::new(prefix);
    let (a_url, a) = fx.upstream().await;
    let (b_url, b) = fx.upstream().await;
    fx.oauth("alpha", &a_url);
    fx.oauth("beta", &b_url);
    (fx, a, b)
}

/// Which upstream served the turn: returns (bound, other).
fn split<'a>(a: &'a Upstream, b: &'a Upstream) -> (&'a Upstream, &'a Upstream, &'static str) {
    if a.hits() == 1 {
        (a, b, "alpha")
    } else {
        (b, a, "beta")
    }
}

#[tokio::test]
async fn a_session_returns_to_its_account_after_a_restart() {
    let (fx, a, b) = two_accounts("claude-safety-pin-restart").await;
    let state = fx.state();
    assert_eq!(send(&state, &[SESSION], false).await.0, StatusCode::OK);
    let (bound, other, _) = split(&a, &b);
    state.claude_affinity.flush_blocking(now_ms() / 1000).unwrap();
    drop(state);

    let restarted = fx.state();
    for _ in 0..3 {
        assert_eq!(send(&restarted, &[SESSION], false).await.0, StatusCode::OK);
    }
    assert_eq!(bound.hits(), 4);
    assert_eq!(other.hits(), 0);
}

#[tokio::test]
async fn a_short_cooldown_on_the_bound_account_answers_429() {
    let (fx, a, b) = two_accounts("claude-safety-pin-short").await;
    let state = fx.state();
    send(&state, &[SESSION], false).await;
    let (bound, other, bound_id) = split(&a, &b);

    member_health(
        &state,
        bound_id,
        Health::Cooldown {
            until_unix_ms: now_ms() + 60_000,
        },
    );
    let (status, headers, _) = send(&state, &[SESSION], false).await;

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let retry: i64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((1..=60).contains(&retry), "retry-after {retry}");
    assert_eq!(bound.hits() + other.hits(), 1, "no upstream call");
}

#[tokio::test]
async fn a_long_cooldown_on_the_bound_account_rebinds() {
    let (fx, a, b) = two_accounts("claude-safety-pin-long").await;
    let state = fx.state();
    send(&state, &[SESSION], false).await;
    let (bound, other, bound_id) = split(&a, &b);

    member_health(
        &state,
        bound_id,
        Health::Cooldown {
            until_unix_ms: now_ms() + 30 * 60_000,
        },
    );
    assert_eq!(send(&state, &[SESSION], false).await.0, StatusCode::OK);
    assert_eq!(other.hits(), 1);

    member_health(&state, bound_id, Health::Available);
    assert_eq!(send(&state, &[SESSION], false).await.0, StatusCode::OK);
    assert_eq!(other.hits(), 2, "the new binding holds");
    assert_eq!(bound.hits(), 1);
}

#[tokio::test]
async fn a_one_off_failover_does_not_move_the_binding() {
    let (fx, a, b) = two_accounts("claude-safety-pin-503").await;
    let state = fx.state();
    send(&state, &[SESSION], false).await;
    let (bound, other, _) = split(&a, &b);

    bound.then(503, "application/json", UNAVAILABLE_503);
    assert_eq!(send(&state, &[SESSION], false).await.0, StatusCode::OK);
    assert_eq!(other.hits(), 1, "turn 2 is served elsewhere");

    // Turn 3: the router's in-memory map saw turn 2 on `other`, but the
    // durable binding wins.
    assert_eq!(send(&state, &[SESSION], false).await.0, StatusCode::OK);
    assert_eq!(bound.hits(), 3);
    assert_eq!(other.hits(), 1);
}

#[tokio::test]
async fn a_client_request_id_alone_does_not_pin_a_claude_session() {
    let (fx, _a, _b) = two_accounts("claude-safety-pin-reqid").await;
    let state = fx.state();
    for request_id in ["req-1", "req-2", "req-3"] {
        assert_eq!(
            send(&state, &[("x-client-request-id", request_id)], false).await.0,
            StatusCode::OK
        );
    }
    assert_eq!(
        state.claude_affinity.len(),
        1,
        "per-request ids are not session keys; the shared body key is"
    );

    send(&state, &[SESSION], false).await;
    assert_eq!(state.claude_affinity.len(), 2);
}

#[tokio::test]
async fn a_cooling_bound_account_is_not_replaced_by_an_api_key_account() {
    let mut fx = Fixture::new("claude-safety-pin-apikey");
    let (oauth_url, oauth) = fx.upstream().await;
    let (key_url, key) = fx.upstream().await;
    key.then(500, "application/json", UNAVAILABLE_503);
    fx.oauth("alpha", &oauth_url);
    fx.api_key("keyed", &key_url);
    let state = fx.state();

    // Turn 1 ends on the OAuth account either directly or after the API-key
    // account's 500.
    assert_eq!(send(&state, &[SESSION], false).await.0, StatusCode::OK);
    assert_eq!(oauth.hits(), 1);
    let key_hits = key.hits();

    member_health(
        &state,
        "alpha",
        Health::Cooldown {
            until_unix_ms: now_ms() + 60_000,
        },
    );
    let (status, _, _) = send(&state, &[SESSION], false).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(key.hits(), key_hits);
}

#[tokio::test]
async fn api_key_only_never_sends_to_an_oauth_account() {
    let mut fx = Fixture::new("claude-safety-api-only");
    let (oauth_url, oauth) = fx.upstream().await;
    let (key_url, key) = fx.upstream().await;
    fx.oauth("alpha", &oauth_url);
    fx.api_key("keyed", &key_url);
    let state = fx.state_with_config(Some("logging-to-file: false\nclaude-route-mode: api_key_only\n"));

    for session in ["s1", "s2", "s3", "s4"] {
        let (status, _, body) =
            send(&state, &[("x-claude-code-session-id", session)], false).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    assert_eq!(oauth.hits(), 0);
    assert_eq!(key.hits(), 4);

    let (headers, body) = key.last();
    assert!(headers.contains_key("x-api-key"));
    assert!(!headers.contains_key("authorization"));
    assert!(!headers.contains_key("x-app"));
    let system = body.get("system").map(|s| s.to_string()).unwrap_or_default();
    assert!(!system.contains("Claude Agent SDK") && !system.contains("Claude Code"), "{system}");
}

#[tokio::test]
async fn the_oauth_profile_matches_the_captured_agent_sdk_wire() {
    let mut fx = Fixture::new("claude-safety-profile");
    let (url, upstream) = fx.upstream().await;
    fx.oauth("alpha", &url);
    let state = fx.state();
    assert_eq!(send(&state, &[], false).await.0, StatusCode::OK);

    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/agent_sdk_wire.json")).unwrap();
    let (headers, body) = upstream.last();
    for (name, value) in fixture["headers"].as_object().unwrap() {
        let sent = headers.get(name).and_then(|v| v.to_str().ok());
        if name == "authorization" {
            assert_eq!(sent, Some("Bearer token-alpha"));
        } else {
            assert_eq!(sent, value.as_str(), "header {name}");
        }
    }
    assert!(body.get("metadata").and_then(|m| m.get("user_id")).is_none());
}

#[tokio::test]
async fn cloak_off_sends_only_the_clients_identity() {
    let mut fx = Fixture::new("claude-safety-cloak-off");
    let (url, upstream) = fx.upstream().await;
    fx.oauth("alpha", &url);
    let state = fx.state_with_config(Some("logging-to-file: false\ndisable-claude-cloak-mode: true\n"));

    let (status, _, body) = send(
        &state,
        &[
            ("user-agent", "my-client/1.0"),
            ("x-stainless-lang", "python"),
            ("anthropic-beta", "output-128k-2025-02-19"),
        ],
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (headers, body) = upstream.last();
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    assert_eq!(header("user-agent"), Some("my-client/1.0"));
    assert_eq!(header("x-stainless-lang"), Some("python"));
    assert_eq!(header("anthropic-beta"), Some("oauth-2025-04-20,output-128k-2025-02-19"));
    assert_eq!(header("x-app"), None);
    assert_eq!(header("x-stainless-os"), None);
    assert_eq!(header("anthropic-dangerous-direct-browser-access"), None);
    assert!(body.get("system").is_none(), "no injected system: {body}");
    assert!(!body.to_string().contains("cache_control"), "{body}");
}

#[tokio::test]
async fn an_egress_entry_that_does_not_build_keeps_only_that_account_out() {
    let mut fx = Fixture::new("claude-safety-egress");
    let (url, upstream) = fx.upstream().await;
    fx.oauth("alpha", &url);
    fx.oauth("beta", &url);
    let state = fx.state_with_config(Some(
        "logging-to-file: false\nclaude-account-egress:\n  alpha: \"http://[bad\"\n",
    ));

    for session in ["s1", "s2", "s3"] {
        let (status, _, _) =
            send(&state, &[("x-claude-code-session-id", session)], false).await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(upstream.hits(), 3);
    let alpha_calls = upstream
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|(h, _)| h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer token-alpha"))
        .count();
    assert_eq!(alpha_calls, 0, "never sent from the host's own address");

    // Saving the same entry is refused.
    let refused = state.settings.mutate(|document| {
        document
            .claude_account_egress
            .insert("beta".into(), "ftp://proxy.example".into());
    });
    assert!(refused.is_err());
    let accepted = state.settings.mutate(|document| {
        document.claude_account_egress.clear();
        document
            .claude_account_egress
            .insert("beta".into(), "socks5h://user:secret@proxy.example:1080".into());
    });
    assert!(accepted.is_ok());

    let stats = serde_json::to_value(state.get_stats()).unwrap();
    let beta = stats["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "beta")
        .unwrap();
    assert_eq!(beta["claude"]["egress_host"], "proxy.example");
    assert_eq!(beta["claude"]["route_mode"], "oauth_first");
    assert!(!stats.to_string().contains("secret"));
}

#[tokio::test]
async fn stats_show_the_suspected_reason_and_pinned_sessions() {
    let mut fx = Fixture::new("claude-safety-stats");
    let (url, upstream) = fx.upstream().await;
    fx.oauth("alpha", &url);
    let state = fx.state();
    send(&state, &[SESSION], false).await;
    upstream.then(403, "application/json", DISABLED_403);
    send(&state, &[("x-claude-code-session-id", "other")], false).await;

    let stats = serde_json::to_value(state.get_stats()).unwrap();
    let alpha = &stats["accounts"][0]["claude"];
    assert_eq!(alpha["affinity_bindings"], 1);
    assert!(alpha["suspected_reason"].as_str().unwrap().contains("has been disabled"));
    assert!(alpha["suspected_at_unix"].as_i64().unwrap() > 0);
    assert_eq!(alpha["probes_24h"], 0);
}
