mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::inbound::ApiKeys;
use mahoquot_gateway::request_history::UsageEvent;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use serde_json::Value;
use tower::ServiceExt;

const MANAGEMENT_KEY: &str = "todo15-manual-management-key";
const EXPORT_SECRET: &str = "todo15-manual-export-secret";

fn request(method: &str, uri: &str, export: bool) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {MANAGEMENT_KEY}"));
    if export {
        builder = builder.header("x-mahoquot-export-authorization", EXPORT_SECRET);
    }
    builder.body(Body::empty()).expect("manual QA request")
}

async fn json(response: axum::response::Response) -> Value {
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("manual QA body")
        .to_bytes();
    assert!(
        status.is_success(),
        "manual QA HTTP {status}: {}",
        String::from_utf8_lossy(&body)
    );
    serde_json::from_slice(&body).expect("manual QA JSON")
}

#[tokio::test]
async fn manual_api_driver() {
    let dir = common::unique_temp_dir("mahoquot-todo15-manual");
    std::fs::write(
        dir.join("config.yaml"),
        format!("logging-to-file: false\nremote-management:\n  secret-key: {EXPORT_SECRET}\n"),
    )
    .expect("manual QA config");
    let config = GatewayConfig {
        auth_dir: dir.clone(),
        config_path: dir.join("config.yaml"),
        api_keys: ApiKeys::new(vec![MANAGEMENT_KEY.to_string()]),
        auth_refresh_enabled: false,
        history_queue_capacity: 64,
        history_batch_size: 16,
        ..GatewayConfig::default()
    };
    let state = Arc::new(AppState::new(&config).expect("manual QA state"));
    for index in 0..6_u64 {
        let selected = index % 2 == 0;
        assert!(state.history.enqueue(UsageEvent {
            event_id: format!("manual-{index}"),
            occurred_at_ms: 1_800_000_000_000 + index as i64,
            account_identifier: if selected { "account-a" } else { "account-b" }.to_string(),
            provider: if selected { "codex" } else { "claude" }.to_string(),
            model: if selected { "gpt-5.6" } else { "claude-4" }.to_string(),
            key_identifier: Some(if selected { "key-a" } else { "key-b" }.to_string()),
            status_code: if selected { 429 } else { 200 },
            succeeded: !selected,
            input_tokens: 10 + index,
            output_tokens: 5 + index,
            cached_input_tokens: index,
            reasoning_tokens: index % 2,
            total_tokens: 15 + index * 2,
            latency_ms: 100 + index,
        }));
    }
    state.history.flush().expect("manual QA flush");
    let app = create_app(Arc::clone(&state));
    let query = "account=account-a&provider=codex&model=gpt-5.6&key-label=key-a&status=429&outcome=failed&text=manual";

    let page = json(
        app.clone()
            .oneshot(request(
                "GET",
                &format!("/v0/management/history/events?limit=2&{query}"),
                false,
            ))
            .await
            .expect("manual QA page"),
    )
    .await;
    assert_eq!(page["events"].as_array().expect("events").len(), 2);
    let cursor = page["next-cursor"].as_i64().expect("next cursor");
    println!("page_1=2 next_cursor={cursor}");

    let page_2 = json(
        app.clone()
            .oneshot(request(
                "GET",
                &format!("/v0/management/history/events?limit=2&cursor={cursor}&{query}"),
                false,
            ))
            .await
            .expect("manual QA second page"),
    )
    .await;
    assert_eq!(page_2["events"].as_array().expect("events").len(), 1);
    println!("page_2=1");

    let count = json(
        app.clone()
            .oneshot(request(
                "GET",
                &format!("/v0/management/history/count?{query}"),
                false,
            ))
            .await
            .expect("manual QA count"),
    )
    .await;
    assert_eq!(count["count"], 3);
    println!("filtered_count=3");

    let detail = json(
        app.clone()
            .oneshot(request(
                "GET",
                "/v0/management/history/events/manual-0",
                false,
            ))
            .await
            .expect("manual QA detail"),
    )
    .await;
    assert_eq!(detail["event"]["event-id"], "manual-0");
    println!("detail=manual-0");

    let csv = app
        .clone()
        .oneshot(request(
            "GET",
            &format!("/v0/management/history/export?format=csv&{query}"),
            true,
        ))
        .await
        .expect("manual QA CSV");
    assert!(csv.status().is_success());
    let csv = String::from_utf8(
        csv.into_body()
            .collect()
            .await
            .expect("manual QA CSV body")
            .to_bytes()
            .to_vec(),
    )
    .expect("manual QA CSV UTF-8");
    assert_eq!(csv.lines().count(), 4);
    assert!(!csv.contains(MANAGEMENT_KEY));
    assert!(!csv.contains(EXPORT_SECRET));
    println!("csv_rows=3 redacted=true");

    let clear = json(
        app.clone()
            .oneshot(request(
                "DELETE",
                &format!("/v0/management/history/events?confirm=true&{query}"),
                false,
            ))
            .await
            .expect("manual QA clear"),
    )
    .await;
    assert_eq!(clear["deleted"], 3);
    assert_eq!(clear["dashboard-history-removed"], true);
    assert_eq!(clear["proxy-file-logs-removed"], false);
    println!("cleared=3 dashboard_removed=true proxy_files_removed=false");

    let remaining = json(
        app.oneshot(request("GET", "/v0/management/history/count", false))
            .await
            .expect("manual QA remaining count"),
    )
    .await;
    assert_eq!(remaining["count"], 3);
    println!("remaining=3");

    std::fs::remove_dir_all(&dir).expect("manual QA cleanup");
    println!("cleanup=ok");
}
