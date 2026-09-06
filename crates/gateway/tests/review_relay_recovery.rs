mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::Router;
use common::unique_temp_dir;
use http_body_util::BodyExt;
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use mahoquot_types::{Health, PoolMember, Strategy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;
use tower::ServiceExt;

const CODEX_REQUEST: &str = r#"{"model":"gpt-5.6-sol","stream":false,"input":"hi"}"#;
const FORBIDDEN_BODY: &str = r#"{"error":{"message":"workspace policy denied this request"}}"#;
const UNAUTHORIZED_BODY: &str = r#"{"error":{"message":"credential is no longer valid"}}"#;
const REFUSED_GRANT: &str = r#"{"error":"invalid_grant"}"#;
const REFRESHED_TOKENS: &str = r#"{"access_token":"refreshed_at_123","refresh_token":"refreshed_rt_123","id_token":"refreshed_idt_123","token_type":"Bearer","expires_in":3600}"#;

struct Fixture {
    servers: Vec<JoinHandle<()>>,
    dir: PathBuf,
}

impl Fixture {
    fn new(prefix: &str) -> Self {
        Self {
            servers: Vec::new(),
            dir: unique_temp_dir(prefix),
        }
    }

    fn account(&self, name: &str, upstream: &str, expired: &str) {
        write_account(&self.dir, name, upstream, expired);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for server in &self.servers {
            server.abort();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn bind_fixture_listener() -> tokio::net::TcpListener {
    for port in 18840..18900 {
        if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            return listener;
        }
    }
    panic!("no free fixture port in 18840-18899");
}

fn write_account(dir: &Path, name: &str, upstream: &str, expired: &str) {
    let credential = serde_json::json!({
        "identity_slug": name,
        "access_token": format!("token-{name}"),
        "account_id": format!("acct-{name}"),
        "email": format!("{name}@example.com"),
        "expired": expired,
        "id_token": "fake_idt",
        "last_refresh": "2026-08-27T00:00:00Z",
        "refresh_token": format!("rt-{name}"),
        "type": "plus",
        "upstream_override": upstream,
        "usage_override": upstream,
    });
    std::fs::write(
        dir.join(format!("codex-{name}.json")),
        credential.to_string(),
    )
    .expect("write credential");
}

fn state_for(dir: &Path, refresh_url: String, auth_refresh_enabled: bool) -> Arc<AppState> {
    let config_path = dir.join("config.yaml");
    std::fs::write(&config_path, "logging-to-file: false\n").expect("write config");
    let config = GatewayConfig {
        auth_dir: dir.to_path_buf(),
        config_path,
        refresh_url,
        auth_refresh_enabled,
        max_failover: 3,
        strategy: Strategy::StrictRoundRobin,
        usage_poll_secs: 120,
        port: 0,
        ..GatewayConfig::default()
    };
    Arc::new(AppState::new(&config).expect("state"))
}

async fn spawn_upstream(
    fixture: &mut Fixture,
    status: StatusCode,
    body: &'static str,
) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let app = Router::new().route(
        common::CODEX_PATH,
        post(move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                (status, [("content-type", "application/json")], body)
            }
        }),
    );
    let listener = bind_fixture_listener().await;
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    fixture.servers.push(tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    }));
    (url, hits)
}

async fn spawn_oauth(
    fixture: &mut Fixture,
    status: StatusCode,
    body: &'static str,
) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let app = Router::new().route(
        "/oauth/token",
        post(move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                (status, [("content-type", "application/json")], body)
            }
        }),
    );
    let listener = bind_fixture_listener().await;
    let url = format!(
        "http://127.0.0.1:{}/oauth/token",
        listener.local_addr().unwrap().port()
    );
    fixture.servers.push(tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    }));
    (url, hits)
}

async fn relay(state: Arc<AppState>, path: &str, body: &'static str) -> (StatusCode, String) {
    let response = create_app(state)
        .oneshot(
            Request::post(path)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

fn health_of(state: &AppState) -> Vec<Health> {
    state
        .pool
        .load()
        .members
        .iter()
        .map(|member| member.health())
        .collect()
}

async fn proactive_refresh_failure(oauth_status: StatusCode) -> (Vec<Health>, StatusCode, usize) {
    let mut fixture = Fixture::new("qg-review-relay-recovery");
    let (upstream_url, upstream_hits) =
        spawn_upstream(&mut fixture, StatusCode::OK, "{\"ok\":true}").await;
    let (oauth_url, oauth_hits) = spawn_oauth(&mut fixture, oauth_status, REFUSED_GRANT).await;
    fixture.account("expiredacct", &upstream_url, "2020-01-01T00:00:00Z");
    let state = state_for(&fixture.dir, oauth_url, true);

    let (status, _) = relay(Arc::clone(&state), common::CODEX_PATH, CODEX_REQUEST).await;

    assert_eq!(
        oauth_hits.load(Ordering::SeqCst),
        1,
        "refresh was attempted"
    );
    (
        health_of(&state),
        status,
        upstream_hits.load(Ordering::SeqCst),
    )
}

/// A refresh the provider rejects means the stored grant is dead, so the account
/// has to be quarantined instead of being retried by the next request.
#[tokio::test]
async fn a_rejected_proactive_refresh_quarantines_the_account() {
    // given an expired credential whose refresh is refused outright
    // when the relay tries to use it
    let (health, status, upstream_hits) = proactive_refresh_failure(StatusCode::BAD_REQUEST).await;

    // then the account is benched and the upstream was never contacted with a
    // token known to be stale
    assert_eq!(health, vec![Health::AuthFailed]);
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(upstream_hits, 0);
}

/// A refresh endpoint that is merely down says nothing about the credential, so
/// the account must stay usable once the outage passes.
#[tokio::test]
async fn a_proactive_refresh_outage_leaves_the_account_usable() {
    // given the same expired credential and a refresh endpoint that is failing
    // when the relay tries to use it
    let (health, status, upstream_hits) =
        proactive_refresh_failure(StatusCode::INTERNAL_SERVER_ERROR).await;

    // then this request still fails, but the credential keeps its health
    assert_ne!(health, vec![Health::AuthFailed]);
    assert_eq!(health, vec![Health::Available]);
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(upstream_hits, 0);
}

/// A 403 is an entitlement or workspace denial carried by a perfectly valid
/// credential: it must fail over to another account without quarantining any of
/// them, and the caller must still see the upstream's own explanation.
#[tokio::test]
async fn a_forbidden_denial_fails_over_without_quarantining_the_account() {
    // given two accounts behind an upstream that denies every request
    let mut fixture = Fixture::new("qg-review-relay-forbidden");
    let (upstream_url, upstream_hits) =
        spawn_upstream(&mut fixture, StatusCode::FORBIDDEN, FORBIDDEN_BODY).await;
    fixture.account("firstacct", &upstream_url, "2099-01-01T00:00:00Z");
    fixture.account("secondacct", &upstream_url, "2099-01-01T00:00:00Z");
    let state = state_for(&fixture.dir, "http://127.0.0.1:1/unused".to_string(), false);
    assert_eq!(state.pool.load().members.len(), 2);

    // when the relay runs out of accounts to try
    let (status, body) = relay(Arc::clone(&state), common::CODEX_PATH, CODEX_REQUEST).await;

    // then both accounts were tried, neither was quarantined, and the denial is
    // reported verbatim
    assert_eq!(upstream_hits.load(Ordering::SeqCst), 2);
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        body.contains("workspace policy denied this request"),
        "{body}"
    );
    assert_eq!(health_of(&state), vec![Health::Available; 2]);
}

/// With refresh disabled there is no second chance: a 401 is terminal and the
/// account must be quarantined rather than left in rotation.
#[tokio::test]
async fn an_unauthorized_reply_quarantines_the_account_when_refresh_is_off() {
    // given one account behind an upstream that rejects its token
    let mut fixture = Fixture::new("qg-review-relay-unauthorized");
    let (upstream_url, upstream_hits) =
        spawn_upstream(&mut fixture, StatusCode::UNAUTHORIZED, UNAUTHORIZED_BODY).await;
    fixture.account("deadacct", &upstream_url, "2099-01-01T00:00:00Z");
    let state = state_for(&fixture.dir, "http://127.0.0.1:1/unused".to_string(), false);

    // when the relay tries it
    let (status, body) = relay(Arc::clone(&state), common::CODEX_PATH, CODEX_REQUEST).await;

    // then the credential is benched and the rejection reaches the caller
    assert_eq!(upstream_hits.load(Ordering::SeqCst), 1);
    assert_eq!(health_of(&state), vec![Health::AuthFailed]);
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("credential is no longer valid"), "{body}");
}

/// An upstream that dies mid-stream cannot be retried — the client already holds
/// committed bytes — so the translated stream has to be closed with a terminal
/// error frame instead of simply stopping.
#[tokio::test]
async fn a_broken_upstream_stream_is_closed_with_an_error_frame_and_never_retried() {
    // given an upstream that flushes part of a Codex stream and then drops the
    // connection without terminating the chunked body
    let mut fixture = Fixture::new("qg-review-relay-stream");
    let full = common::codex_sse("partial");
    let truncated = full
        .split("event: response.completed")
        .next()
        .expect("partial stream")
        .to_string();
    let listener = bind_fixture_listener().await;
    let upstream_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&connections);
    fixture.servers.push(tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            drain_request(&mut socket).await;
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket
                .write_all(format!("{:X}\r\n{truncated}\r\n", truncated.len()).as_bytes())
                .await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        }
    }));
    fixture.account("streamacct", &upstream_url, "2099-01-01T00:00:00Z");
    let state = state_for(&fixture.dir, "http://127.0.0.1:1/unused".to_string(), false);

    // when an OpenAI-compatible client streams through the translation path
    let (status, body) = relay(
        Arc::clone(&state),
        "/v1/chat/completions",
        common::OPENAI_REQUEST,
    )
    .await;

    // then the committed prefix survives, the stream ends as a terminated SSE
    // error, and the request was never replayed upstream
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("partial"), "{body}");
    assert!(body.contains("error"), "{body}");
    assert!(body.trim_end().ends_with("data: [DONE]"), "{body}");
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

async fn spawn_unauthorized_then_unreachable_upstream(
    fixture: &mut Fixture,
) -> (String, Arc<AtomicUsize>) {
    let listener = bind_fixture_listener().await;
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&connections);
    fixture.servers.push(tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let seen = counter.fetch_add(1, Ordering::SeqCst);
            drain_request(&mut socket).await;
            if seen == 0 {
                let head = format!(
                    "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    UNAUTHORIZED_BODY.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(UNAUTHORIZED_BODY.as_bytes()).await;
                let _ = socket.flush().await;
            }
            let _ = socket.shutdown().await;
        }
    }));
    (url, connections)
}

/// A replay that cannot reach the upstream says nothing about the credential the
/// refresh just renewed, so the account must keep its health while the request
/// itself fails.
#[tokio::test]
async fn a_replay_that_cannot_reach_the_upstream_keeps_the_refreshed_account() {
    // given an upstream that rejects the first call and then refuses to answer,
    // behind a refresh endpoint that hands out a fresh token
    let mut fixture = Fixture::new("qg-review-relay-replay");
    let (upstream_url, connections) =
        spawn_unauthorized_then_unreachable_upstream(&mut fixture).await;
    let (oauth_url, oauth_hits) = spawn_oauth(&mut fixture, StatusCode::OK, REFRESHED_TOKENS).await;
    fixture.account("replayacct", &upstream_url, "2099-01-01T00:00:00Z");
    let state = state_for(&fixture.dir, oauth_url, true);

    // when the relay refreshes and replays
    let (status, _) = relay(Arc::clone(&state), common::CODEX_PATH, CODEX_REQUEST).await;

    // then the token was renewed once, the replay was attempted, and the account
    // survives the transport failure
    assert_eq!(oauth_hits.load(Ordering::SeqCst), 1);
    assert_eq!(connections.load(Ordering::SeqCst), 2);
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(health_of(&state), vec![Health::Available]);
}

async fn drain_request(socket: &mut tokio::net::TcpStream) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buffer.extend_from_slice(&chunk[..read]);
        let Some(head_end) = buffer
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|at| at + 4)
        else {
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..head_end]).to_lowercase();
        let content_length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if buffer.len() - head_end >= content_length {
            return;
        }
    }
}
