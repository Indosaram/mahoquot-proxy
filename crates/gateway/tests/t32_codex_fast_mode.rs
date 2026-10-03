//! The proxy-wide ChatGPT fast-mode switch.
//!
//! Assertions read the body the upstream stub actually received: in observed test
//! samples the backend echoed `auto`/`default` rather than reflecting the requested tier,
//! so request body inspection tests the outbound client configuration. Note that the
//! relay fast flag records pre-response request configuration rather than served-tier proof.

mod common;

use std::future::IntoFuture;
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::routing::post;
use axum::Router;
use mahoquot_gateway::{config::GatewayConfig, routes::create_app, state::AppState};
use serde_json::{json, Value};
use tower::ServiceExt;

type SeenLog = Arc<Mutex<Vec<(String, Value)>>>;

struct Harness {
    app: Router,
    state: Arc<AppState>,
    seen: SeenLog,
    dir: std::path::PathBuf,
    upstream: tokio::task::JoinHandle<()>,
}

impl Harness {
    fn seen(&self) -> Vec<(String, Value)> {
        self.seen.lock().unwrap().clone()
    }

    async fn stop(self) {
        self.upstream.abort();
        let _ = self.upstream.await;
        std::fs::remove_dir_all(self.dir).unwrap();
    }
}

async fn harness(label: &str) -> Harness {
    let seen: SeenLog = Arc::new(Mutex::new(Vec::new()));
    async fn capture(
        State(seen): State<SeenLog>,
        uri: axum::http::Uri,
        body: Bytes,
    ) -> impl axum::response::IntoResponse {
        seen.lock()
            .unwrap()
            .push((uri.path().to_string(), serde_json::from_slice(&body).unwrap()));
        (
            [("content-type", "text/event-stream")],
            common::codex_sse("done"),
        )
    }

    let mut listener = None;
    for port in 18880..=18899 {
        if let Ok(bound) = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            listener = Some(bound);
            break;
        }
    }
    let listener = listener.expect("an isolated test port in 18880-18899");
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn({
        let seen = seen.clone();
        async move {
            let _ = axum::serve(
                listener,
                Router::new().fallback(post(capture)).with_state(seen),
            )
            .into_future()
            .await;
        }
    });

    let dir = common::unique_temp_dir(label);
    let mut credential: Value = serde_json::from_str(&common::create_auth_file_json(
        "fixture-codex",
        "fixture-account",
        "stale-token",
        Some(&upstream),
    ))
    .unwrap();
    credential["upstream_override"] = json!(upstream);
    credential["usage_override"] = json!(upstream);
    std::fs::write(dir.join("codex-fixture.json"), credential.to_string()).unwrap();
    let config_path = dir.join("config.yaml");
    std::fs::write(&config_path, "logging-to-file: false\n").unwrap();

    let state = Arc::new(
        AppState::new(&GatewayConfig {
            auth_dir: dir.clone(),
            config_path,
            refresh_url: format!("{upstream}/token"),
            auth_refresh_enabled: false,
            api_keys: mahoquot_gateway::inbound::ApiKeys::from_env_value("fixture-key"),
            ..GatewayConfig::default()
        })
        .unwrap(),
    );
    assert_eq!(state.pool.load().members.len(), 1, "one codex fixture account");
    let app = create_app(Arc::clone(&state));
    Harness {
        app,
        state,
        seen,
        dir,
        upstream: task,
    }
}

async fn post_json(app: &Router, path: &str, body: Value) -> u16 {
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        app.clone().oneshot(
            axum::http::Request::post(path)
                .header("authorization", "Bearer fixture-key")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let status = response.status();
    let _ = axum::body::to_bytes(response.into_body(), 1_000_000).await;
    status.as_u16()
}

fn chat_request(tier: Option<&str>) -> Value {
    let mut body = json!({
        "model": "codex",
        "stream": false,
        "messages": [{"role": "user", "content": "hi"}],
    });
    if let Some(tier) = tier {
        body["service_tier"] = json!(tier);
    }
    body
}

fn responses_request() -> Value {
    json!({
        "model": "codex",
        "stream": true,
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
    })
}

#[tokio::test]
async fn the_switch_puts_priority_on_every_codex_bound_request() {
    // Given: a gateway with the switch on
    let h = harness("t32-fast-mode-on").await;
    h.state
        .settings
        .mutate(|settings| {
            settings.codex_fast_mode = true;
        })
        .unwrap();

    // When: the caller arrives through both codex-bound entry points
    assert_eq!(post_json(&h.app, "/v1/chat/completions", chat_request(None)).await, 200);
    assert_eq!(post_json(&h.app, "/v1/responses", responses_request()).await, 200);

    // Then: each upstream request carries the priority tier on the codex path
    let seen = h.seen();
    assert_eq!(seen.len(), 2, "{seen:?}");
    for (path, body) in &seen {
        assert_eq!(path, common::CODEX_PATH, "{seen:?}");
        assert_eq!(
            body["service_tier"],
            json!("priority"),
            "the switch owns the tier on {path}: {body}"
        );
    }
    // And: the translated path still carries the instructions it always had, so
    // the injection rewrote the document rather than replacing it.
    let translated = seen
        .iter()
        .find(|(_, body)| body.get("instructions").is_some())
        .expect("the chat-completions request was translated");
    assert_eq!(translated.1["store"], json!(false));
    assert!(translated.1["input"].is_array());
    h.stop().await;
}

#[tokio::test]
async fn the_switch_is_off_by_default_and_never_invents_a_tier() {
    // Given: a gateway on defaults about to be discarded by a real install
    let h = harness("t32-fast-mode-off").await;
    assert!(
        !h.state.settings.current().codex_fast_mode,
        "the switch ships off"
    );

    // When: a client asks for a tier of its own through the translated path
    assert_eq!(
        post_json(&h.app, "/v1/chat/completions", chat_request(Some("auto"))).await,
        200
    );

    // Then: the switch adds nothing — the translator's allow list remains the
    // only thing deciding what reaches the upstream.
    let seen = h.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(
        seen[0].1.get("service_tier").is_none(),
        "fast mode is off, so the tier is the caller's to lose: {}",
        seen[0].1
    );
    h.stop().await;
}

#[tokio::test]
async fn the_switch_outranks_a_client_tier() {
    // Given: the switch on and a client insisting on a different tier
    let h = harness("t32-fast-mode-override").await;
    h.state
        .settings
        .mutate(|settings| {
            settings.codex_fast_mode = true;
        })
        .unwrap();

    // When: that client calls
    assert_eq!(
        post_json(&h.app, "/v1/chat/completions", chat_request(Some("auto"))).await,
        200
    );

    // Then: the operator's switch wins over the caller's preference
    let seen = h.seen();
    let translated = seen
        .iter()
        .find(|(path, _)| path == common::CODEX_PATH)
        .expect("the chat-completions request reached the codex path");
    assert_eq!(translated.1["service_tier"], json!("priority"), "{seen:?}");
    h.stop().await;
}