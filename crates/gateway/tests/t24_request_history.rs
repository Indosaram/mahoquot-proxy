mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use http_body_util::BodyExt;
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::inbound::ApiKeys;
use mahoquot_gateway::request_history::UsageEvent;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use rusqlite::Connection;
use serde_json::Value;
use tower::ServiceExt;

use common::{create_auth_file_json, unique_temp_dir, OPENAI_REQUEST};

const UPSTREAM_PORT: u16 = 18840;
const API_KEY: &str = "t24-management-key";
const EXPORT_SECRET: &str = "task-15-export-secret";

struct Fixture {
    state: Arc<AppState>,
    app: Router,
    dir: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture(label: &str, upstream: Option<&str>, queue_capacity: usize) -> Fixture {
    let dir = unique_temp_dir(&format!("t24-request-history-{label}"));
    let credential =
        create_auth_file_json("history-account", "account-24", "upstream-token", upstream);
    std::fs::write(dir.join("codex-history.json"), credential).expect("credential fixture");
    std::fs::write(
        dir.join("config.yaml"),
        "logging-to-file: false\nremote-management:\n  secret-key: task-15-export-secret\n",
    )
    .expect("config fixture");
    let config = GatewayConfig {
        auth_dir: dir.clone(),
        config_path: dir.join("config.yaml"),
        api_keys: ApiKeys::new(vec![API_KEY.to_string()]),
        auth_refresh_enabled: false,
        max_failover: 3,
        history_queue_capacity: queue_capacity,
        history_batch_size: 16,
        ..GatewayConfig::default()
    };
    let state = Arc::new(AppState::new(&config).expect("gateway state"));
    let app = create_app(Arc::clone(&state));
    Fixture { state, app, dir }
}

fn authed_request(method: &str, uri: &str, body: Body) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {API_KEY}"))
        .header("content-type", "application/json")
        .body(body)
        .expect("request")
}

async fn json(response: Response) -> Value {
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("response body")
        .to_bytes();
    serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "status {status}, invalid json {error}: {}",
            String::from_utf8_lossy(&bytes)
        )
    })
}

async fn spawn_history_upstream() -> tokio::task::JoinHandle<()> {
    let sse = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_t24\"}}\n\n",
        "event: response.output_item.added\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_t24\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"content_index\":0,\"delta\":\"ok\",\"item_id\":\"msg_t24\",\"output_index\":0}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_t24\",\"status\":\"completed\",\"usage\":{\"input_tokens\":11,\"output_tokens\":7,\"input_tokens_details\":{\"cached_tokens\":3},\"output_tokens_details\":{\"reasoning_tokens\":2},\"total_tokens\":18}}}\n\n",
    );
    let app = Router::new().route(
        "/backend-api/codex/responses",
        post(move || async move {
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from(sse))
                .expect("upstream response")
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", UPSTREAM_PORT))
        .await
        .expect("bind t24 upstream");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve t24 upstream");
    })
}

#[tokio::test]
async fn streaming_and_nonstreaming_events_match_analytics() {
    let upstream = spawn_history_upstream().await;
    let upstream_url = format!("http://127.0.0.1:{UPSTREAM_PORT}");
    let fixture = fixture("analytics", Some(&upstream_url), 64);

    for stream in [true, false] {
        let mut payload: Value =
            serde_json::from_str(OPENAI_REQUEST).expect("canonical OpenAI fixture");
        payload["stream"] = Value::Bool(stream);
        let request = authed_request(
            "POST",
            "/v1/chat/completions",
            Body::from(payload.to_string()),
        );
        let response = fixture
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("relay response");
        let status = response.status();
        let _delivered = response
            .into_body()
            .collect()
            .await
            .expect("consume relay response")
            .to_bytes();
        assert_eq!(status, StatusCode::OK);
    }

    let stats_response = fixture
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            "/v0/management/history/stats",
            Body::empty(),
        ))
        .await
        .expect("history stats");
    assert_eq!(stats_response.status(), StatusCode::OK);
    let stats = json(stats_response).await;
    assert_eq!(stats["totals"]["requests"], 2);
    assert_eq!(stats["totals"]["successful-requests"], 2);
    assert_eq!(stats["totals"]["failed-requests"], 0);
    let events_response = fixture
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            "/v0/management/history/events?limit=10",
            Body::empty(),
        ))
        .await
        .expect("history events");
    assert_eq!(events_response.status(), StatusCode::OK);
    let events = json(events_response).await;
    let rows = events["events"].as_array().expect("event rows");
    assert_eq!(
        rows.len(),
        2,
        "one finalized row per relay request: {events}"
    );
    assert_eq!(
        stats["totals"]["input-tokens"], 22,
        "stats: {stats}; events: {events}"
    );
    assert_eq!(stats["totals"]["output-tokens"], 14);
    assert_eq!(stats["totals"]["cached-input-tokens"], 6);
    assert_eq!(stats["totals"]["reasoning-tokens"], 4);
    assert_eq!(stats["totals"]["total-tokens"], 36);
    assert_ne!(rows[0]["event-id"], rows[1]["event-id"]);
    assert!(rows.iter().all(|row| row["account"] == "history-account"));
    assert!(rows.iter().all(|row| row["provider"] == "codex"));
    assert!(rows.iter().all(|row| row["status"] == 200));
    let expected_key_identifier = mahoquot_gateway::request_history::stable_key_identifier(API_KEY);
    assert!(rows
        .iter()
        .all(|row| row["key-label"] == expected_key_identifier));
    assert!(rows.iter().all(|row| row["key-label"] != API_KEY));

    let detail_id = rows[0]["event-id"].as_str().expect("detail event id");
    let detail_response = fixture
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            &format!("/v0/management/history/events/{detail_id}"),
            Body::empty(),
        ))
        .await
        .expect("relay event detail");
    let detail = json(detail_response).await;
    assert_eq!(detail["event"]["key-label"], expected_key_identifier);
    assert!(!detail.to_string().contains(API_KEY));

    upstream.abort();
}

#[tokio::test]
async fn full_channel_is_nonblocking() {
    let fixture = fixture("full-channel", None, 1);
    let database_path = fixture.dir.join("request-history.sqlite");
    let locker = Connection::open(&database_path).expect("open history database");
    locker
        .execute_batch("BEGIN IMMEDIATE")
        .expect("lock history database");

    let started = Instant::now();
    for index in 0..1_000_u64 {
        fixture.state.history.enqueue(UsageEvent {
            event_id: format!("overflow-{index}"),
            occurred_at_ms: index as i64,
            account_identifier: "history-account".to_string(),
            provider: "codex".to_string(),
            model: "gpt-5.1-codex".to_string(),
            key_identifier: Some("key-label".to_string()),
            status_code: 200,
            succeeded: true,
            input_tokens: 1,
            output_tokens: 1,
            cached_input_tokens: 0,
            reasoning_tokens: 0,
            total_tokens: 2,
            latency_ms: 1,
        });
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(250),
        "bounded enqueue blocked for {elapsed:?}"
    );

    let response = fixture
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            "/v0/management/history/health",
            Body::empty(),
        ))
        .await
        .expect("history health");
    assert_eq!(response.status(), StatusCode::OK);
    let health = json(response).await;
    assert_eq!(health["degraded"], true);
    assert!(
        health["dropped-events"].as_u64().unwrap_or(0) > 0,
        "{health}"
    );
    assert!(
        fixture
            .state
            .metrics
            .history_dropped
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0
    );

    locker
        .execute_batch("ROLLBACK")
        .expect("unlock history database");
}

#[tokio::test]
async fn invalid_range_is_400() {
    let fixture = fixture("invalid-range", None, 64);
    let response = fixture
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            "/v0/management/history/stats?start-ms=200&end-ms=100",
            Body::empty(),
        ))
        .await
        .expect("invalid history query");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json(response).await;
    assert_eq!(body["error"]["code"], "history_range_invalid");
    assert_eq!(body["error"]["retryable"], false);
}

#[tokio::test]
async fn invalid_cursor_is_400() {
    let fixture = fixture("invalid-cursor", None, 64);
    let response = fixture
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            "/v0/management/history/events?cursor=not-a-number",
            Body::empty(),
        ))
        .await
        .expect("invalid cursor response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json(response).await;
    assert_eq!(body["error"]["code"], "history_cursor_invalid");
}

#[tokio::test]
async fn cancelled_clear_preserves_rows() {
    let fixture = fixture("cancelled-clear", None, 64);
    fixture.state.history.enqueue(UsageEvent {
        event_id: "clear-sentinel".to_string(),
        occurred_at_ms: 1,
        account_identifier: "history-account".to_string(),
        provider: "codex".to_string(),
        model: "gpt-5.1-codex".to_string(),
        key_identifier: Some("key-label".to_string()),
        status_code: 200,
        succeeded: true,
        input_tokens: 1,
        output_tokens: 1,
        cached_input_tokens: 0,
        reasoning_tokens: 0,
        total_tokens: 2,
        latency_ms: 1,
    });
    fixture.state.history.flush().expect("flush clear sentinel");

    let response = fixture
        .app
        .clone()
        .oneshot(authed_request(
            "DELETE",
            "/v0/management/history/events",
            Body::empty(),
        ))
        .await
        .expect("cancelled clear response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json(response).await;
    assert_eq!(body["error"]["code"], "history_clear_confirmation_required");
    assert_eq!(
        fixture
            .state
            .history
            .store()
            .unwrap()
            .totals()
            .unwrap()
            .requests,
        1
    );
}

#[tokio::test]
async fn confirmed_filtered_clear_deletes_only_the_selected_scope() {
    let fixture = fixture("confirmed-clear", None, 64);
    for (event_id, account) in [("clear-a", "account-a"), ("clear-b", "account-b")] {
        fixture.state.history.enqueue(UsageEvent {
            event_id: event_id.to_string(),
            occurred_at_ms: 1,
            account_identifier: account.to_string(),
            provider: "codex".to_string(),
            model: "gpt-5.1-codex".to_string(),
            key_identifier: Some("key-label".to_string()),
            status_code: 200,
            succeeded: true,
            input_tokens: 1,
            output_tokens: 1,
            cached_input_tokens: 0,
            reasoning_tokens: 0,
            total_tokens: 2,
            latency_ms: 1,
        });
    }
    fixture.state.history.flush().expect("flush clear fixtures");
    for account in ["account-a", "account-b"] {
        fixture
            .state
            .telemetry
            .record_with_account(0, "codex", Some(account), true);
        fixture
            .state
            .telemetry
            .record_tokens(0, "codex", account, 1, 1);
    }
    fixture
        .state
        .telemetry
        .flush()
        .expect("flush dashboard history");

    let response = fixture
        .app
        .clone()
        .oneshot(authed_request(
            "DELETE",
            "/v0/management/history/events?account=account-a&confirm=true",
            Body::empty(),
        ))
        .await
        .expect("confirmed clear response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = json(response).await;
    assert_eq!(body["deleted"], 1);
    assert_eq!(body["dashboard-history-removed"], true);
    assert_eq!(body["proxy-file-logs-removed"], false);
    let dashboard = fixture.state.telemetry.snapshot();
    assert_eq!(dashboard.len(), 1);
    assert_eq!(dashboard[0].requests, 1);
    assert_eq!(dashboard[0].successes, 1);
    assert_eq!(dashboard[0].input_tokens, 1);
    assert_eq!(dashboard[0].output_tokens, 1);
    assert_eq!(dashboard[0].providers[0].provider, "codex");
    assert_eq!(dashboard[0].providers[0].requests, 1);
    assert_eq!(dashboard[0].accounts.len(), 1);
    assert_eq!(dashboard[0].accounts[0].account, "account-b");
    assert_eq!(dashboard[0].accounts[0].requests, 1);
    let page = fixture
        .state
        .history
        .store()
        .unwrap()
        .page(&Default::default(), None, 10)
        .unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].account_identifier, "account-b");
}

#[tokio::test]
async fn unauthorized_export_is_401() {
    let fixture = fixture("unauthorized-export", None, 64);
    let response = fixture
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v0/management/history/export?format=csv")
                .body(Body::empty())
                .expect("unauthorized export request"),
        )
        .await
        .expect("unauthorized export response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

fn enqueue_event(
    fixture: &Fixture,
    index: u64,
    account: &str,
    provider: &str,
    model: &str,
    key: &str,
    status: u16,
) {
    fixture.state.history.enqueue(UsageEvent {
        event_id: format!("task-15-event-{index}"),
        occurred_at_ms: 1_800_000_000_000 + index as i64,
        account_identifier: account.to_string(),
        provider: provider.to_string(),
        model: model.to_string(),
        key_identifier: Some(key.to_string()),
        status_code: status,
        succeeded: status < 400,
        input_tokens: 100 + index,
        output_tokens: 20 + index,
        cached_input_tokens: index % 11,
        reasoning_tokens: index % 7,
        total_tokens: 120 + index * 2,
        latency_ms: 50 + index,
    });
}

#[tokio::test]
async fn invalid_cursor_is_400_and_preserves_rows() {
    let fixture = fixture("invalid-cursor-preserves", None, 64);
    enqueue_event(&fixture, 1, "account-a", "codex", "gpt-5.6", "key-a", 200);

    let before = fixture
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            "/v0/management/history/count",
            Body::empty(),
        ))
        .await
        .expect("count before invalid cursor");
    assert_eq!(before.status(), StatusCode::OK);
    let before = json(before).await;

    let response = fixture
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            "/v0/management/history/events?cursor=not-an-integer&limit=20",
            Body::empty(),
        ))
        .await
        .expect("invalid cursor response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json(response).await;
    assert_eq!(body["error"]["code"], "history_cursor_invalid");

    let after = fixture
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            "/v0/management/history/count",
            Body::empty(),
        ))
        .await
        .expect("count after invalid cursor");
    assert_eq!(after.status(), StatusCode::OK);
    assert_eq!(json(after).await["count"], before["count"]);
}

#[tokio::test]
async fn cancelled_clear_is_409_and_preserves_rows() {
    let fixture = fixture("cancelled-clear-conflict", None, 64);
    enqueue_event(&fixture, 1, "account-a", "codex", "gpt-5.6", "key-a", 200);
    enqueue_event(&fixture, 2, "account-b", "claude", "claude-4", "key-b", 500);

    let response = fixture
        .app
        .clone()
        .oneshot(authed_request(
            "DELETE",
            "/v0/management/history?account=account-a",
            Body::empty(),
        ))
        .await
        .expect("cancelled clear response");
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = json(response).await;
    assert_eq!(body["error"]["code"], "history_clear_confirmation_required");

    let count = fixture
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            "/v0/management/history/count",
            Body::empty(),
        ))
        .await
        .expect("count after cancelled clear");
    assert_eq!(count.status(), StatusCode::OK);
    assert_eq!(json(count).await["count"], 2);
}

#[tokio::test]
async fn unauthorized_export_is_403_and_preserves_rows() {
    let fixture = fixture("unauthorized-export-explicit", None, 64);
    enqueue_event(&fixture, 1, "account-a", "codex", "gpt-5.6", "key-a", 200);

    let response = fixture
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            "/v0/management/history/export?format=json",
            Body::empty(),
        ))
        .await
        .expect("unauthorized explicit export response");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = json(response).await;
    assert_eq!(body["error"]["code"], "export_unauthorized");

    let count = fixture
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            "/v0/management/history/count",
            Body::empty(),
        ))
        .await
        .expect("count after unauthorized export");
    assert_eq!(json(count).await["count"], 1);
}

#[tokio::test]
async fn pages_filters_detail_clear_and_exports_large_history() {
    let fixture = fixture("task-15-large-history", None, 20_000);
    let csv_account = "account,\"alpha\"";
    for index in 0..10_000_u64 {
        let selected = index % 10 == 0;
        enqueue_event(
            &fixture,
            index,
            if index == 1 {
                csv_account
            } else if selected {
                "account-alpha"
            } else {
                "account-b"
            },
            if selected { "codex" } else { "claude" },
            if selected {
                "gpt-5.6-searchable"
            } else {
                "claude-4"
            },
            if selected { "key-safe" } else { "key-other" },
            if selected { 429 } else { 200 },
        );
    }
    fixture.state.history.flush().expect("flush large history");

    let query = "account=account-alpha&provider=codex&model=gpt-5.6-searchable&key-label=key-safe&status=429&text=searchable";
    let first = fixture
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            &format!("/v0/management/history/events?limit=100&{query}"),
            Body::empty(),
        ))
        .await
        .expect("first filtered page");
    assert_eq!(first.status(), StatusCode::OK);
    let first = json(first).await;
    assert_eq!(first["events"].as_array().expect("events").len(), 100);
    let cursor = first["next-cursor"].as_i64().expect("next cursor");
    assert!(!first.to_string().contains(API_KEY));
    assert!(!first.to_string().contains(EXPORT_SECRET));

    let second = fixture
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            &format!("/v0/management/history/events?limit=100&cursor={cursor}&{query}"),
            Body::empty(),
        ))
        .await
        .expect("second filtered page");
    assert_eq!(second.status(), StatusCode::OK);
    let second = json(second).await;
    assert_eq!(second["events"].as_array().expect("events").len(), 100);
    assert_ne!(
        first["events"][0]["event-id"],
        second["events"][0]["event-id"]
    );

    let detail_id = first["events"][0]["event-id"].as_str().expect("event id");
    let detail = fixture
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            &format!("/v0/management/history/events/{detail_id}"),
            Body::empty(),
        ))
        .await
        .expect("event detail");
    assert_eq!(detail.status(), StatusCode::OK);
    let detail = json(detail).await;
    assert_eq!(detail["event"]["event-id"], detail_id);

    let count = fixture
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            &format!("/v0/management/history/count?{query}"),
            Body::empty(),
        ))
        .await
        .expect("filtered count");
    assert_eq!(count.status(), StatusCode::OK);
    assert_eq!(json(count).await["count"], 1_000);

    let export_request = |format: &str| {
        Request::builder()
            .method("GET")
            .uri(format!(
                "/v0/management/history/export?format={format}&{query}"
            ))
            .header("authorization", format!("Bearer {API_KEY}"))
            .header("x-mahoquot-export-authorization", EXPORT_SECRET)
            .body(Body::empty())
            .expect("authorized export request")
    };
    let csv = fixture
        .app
        .clone()
        .oneshot(export_request("csv"))
        .await
        .expect("csv export");
    assert_eq!(csv.status(), StatusCode::OK);
    let csv_body = String::from_utf8(
        csv.into_body()
            .collect()
            .await
            .expect("csv body")
            .to_bytes()
            .to_vec(),
    )
    .expect("csv utf8");
    assert_eq!(csv_body.lines().count(), 1_001);
    assert!(csv_body.contains("\"task-15-event-9990\""));
    assert!(!csv_body.contains(API_KEY));
    assert!(!csv_body.contains(EXPORT_SECRET));

    let csv_all = fixture
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v0/management/history/export?format=csv")
                .header("authorization", format!("Bearer {API_KEY}"))
                .header("x-mahoquot-export-authorization", EXPORT_SECRET)
                .body(Body::empty())
                .expect("authorized full csv export request"),
        )
        .await
        .expect("full csv export");
    assert_eq!(csv_all.status(), StatusCode::OK);
    let csv_all = String::from_utf8(
        csv_all
            .into_body()
            .collect()
            .await
            .expect("full csv body")
            .to_bytes()
            .to_vec(),
    )
    .expect("full csv utf8");
    assert_eq!(csv_all.lines().count(), 10_001);
    assert!(csv_all.contains("\"account,\"\"alpha\"\"\""));
    assert!(!csv_all.contains(API_KEY));
    assert!(!csv_all.contains(EXPORT_SECRET));

    let json_export = fixture
        .app
        .clone()
        .oneshot(export_request("json"))
        .await
        .expect("json export");
    assert_eq!(json_export.status(), StatusCode::OK);
    let json_export = json(json_export).await;
    assert_eq!(json_export["count"], 1_000);
    assert_eq!(
        json_export["events"]
            .as_array()
            .expect("export events")
            .len(),
        1_000
    );
    assert!(!json_export.to_string().contains(API_KEY));
    assert!(!json_export.to_string().contains(EXPORT_SECRET));

    let clear = fixture
        .app
        .clone()
        .oneshot(authed_request(
            "DELETE",
            "/v0/management/history/events?provider=claude&confirm=delete-history",
            Body::empty(),
        ))
        .await
        .expect("scoped clear");
    assert_eq!(clear.status(), StatusCode::OK);
    let clear = json(clear).await;
    assert_eq!(clear["deleted"], 9_000);
    assert_eq!(clear["dashboard-history-removed"], true);

    let remaining = fixture
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            "/v0/management/history/count",
            Body::empty(),
        ))
        .await
        .expect("remaining count");
    assert_eq!(json(remaining).await["count"], 1_000);
}

#[tokio::test]
async fn invalid_cursor_unauthorized_export_cancelled_clear() {
    let invalid = fixture("qa-invalid-cursor", None, 64);
    enqueue_event(&invalid, 1, "account-a", "codex", "gpt-5.6", "key-a", 200);
    let invalid_response = invalid
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            "/v0/management/history/events?cursor=not-an-integer",
            Body::empty(),
        ))
        .await
        .expect("invalid cursor QA response");
    assert_eq!(invalid_response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json(invalid_response).await["error"]["code"],
        "history_cursor_invalid"
    );
    let invalid_count = invalid
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            "/v0/management/history/count",
            Body::empty(),
        ))
        .await
        .expect("invalid cursor unchanged count");
    assert_eq!(json(invalid_count).await["count"], 1);

    let unauthorized = fixture("qa-unauthorized-export", None, 64);
    enqueue_event(
        &unauthorized,
        2,
        "account-a",
        "codex",
        "gpt-5.6",
        "key-a",
        200,
    );
    let unauthorized_response = unauthorized
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            "/v0/management/history/export?format=json",
            Body::empty(),
        ))
        .await
        .expect("unauthorized export QA response");
    assert_eq!(unauthorized_response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json(unauthorized_response).await["error"]["code"],
        "export_unauthorized"
    );
    let unauthorized_count = unauthorized
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            "/v0/management/history/count",
            Body::empty(),
        ))
        .await
        .expect("unauthorized export unchanged count");
    assert_eq!(json(unauthorized_count).await["count"], 1);

    let cancelled = fixture("qa-cancelled-clear", None, 64);
    enqueue_event(&cancelled, 3, "account-a", "codex", "gpt-5.6", "key-a", 200);
    let cancelled_response = cancelled
        .app
        .clone()
        .oneshot(authed_request(
            "DELETE",
            "/v0/management/history?account=account-a",
            Body::empty(),
        ))
        .await
        .expect("cancelled clear QA response");
    assert_eq!(cancelled_response.status(), StatusCode::CONFLICT);
    assert_eq!(
        json(cancelled_response).await["error"]["code"],
        "history_clear_confirmation_required"
    );
    let cancelled_count = cancelled
        .app
        .clone()
        .oneshot(authed_request(
            "GET",
            "/v0/management/history/count",
            Body::empty(),
        ))
        .await
        .expect("cancelled clear unchanged count");
    assert_eq!(json(cancelled_count).await["count"], 1);
}
