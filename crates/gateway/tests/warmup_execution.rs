mod common;
use axum::{routing::post, Router};
use mahoquot_gateway::{config::GatewayConfig, state::AppState, warmup::warm_account};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[tokio::test]
async fn warmup_actual_http_validation_and_zero_hit_eligibility() {
    let listener = {
        let mut bound = None;
        for port in 18840..=18899 {
            if let Ok(l) =
                tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await
            {
                bound = Some(l);
                break;
            }
        }
        bound.expect("mock port")
    };
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mode = Arc::new(AtomicUsize::new(0));
    let hits = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let app = Router::new().route(
        common::CODEX_PATH,
        post({
            let mode = mode.clone();
            let hits = hits.clone();
            let entered = entered.clone();
            let release = release.clone();
            move |headers: axum::http::HeaderMap,
                  axum::Json(body): axum::Json<serde_json::Value>| {
                let mode = mode.clone();
                let hits = hits.clone();
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    assert!(body.get("max_output_tokens").is_none());
                    assert_eq!(body["stream"], true);
                    if mode.load(Ordering::SeqCst) == 5 {
                        if headers["authorization"] == "Bearer mock" {
                            return (axum::http::StatusCode::UNAUTHORIZED, String::new());
                        }
                        assert_eq!(headers["authorization"], "Bearer refreshed-mock");
                    }
                    if mode.load(Ordering::SeqCst) == 4 {
                        entered.add_permits(1);
                        release.acquire().await.unwrap().forget();
                    }
                    let output = match mode.load(Ordering::SeqCst) {
                        0 => "data: [DONE]\n\n".to_owned(),
                        1 => String::new(),
                        2 => format!(
                            "{}data: {{\"error\":{{\"message\":\"late\"}}}}\n\n",
                            common::codex_sse("x")
                        ),
                        _ => common::codex_sse("x"),
                    };
                    (axum::http::StatusCode::OK, output)
                }
            }
        }),
    );
    let cline_hits = Arc::new(AtomicUsize::new(0));
    let app=app.route("/refresh",post(|| async { axum::Json(json!({"access_token":"refreshed-mock","refresh_token":"fake_rt","expires_in":3600,"token_type":"Bearer"})) }));
    let app = app.route(
        "/v1/chat/completions",
        post({
            let hits = cline_hits.clone();
            let mode = mode.clone();
            move |axum::Json(body): axum::Json<serde_json::Value>| {
                let hits = hits.clone();
                let mode = mode.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(body["model"], "z-ai/glm-5.3-flash");
                    if mode.load(Ordering::SeqCst) == 6 {
                        return (axum::http::StatusCode::OK, [("content-type","text/event-stream")], "data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n");
                    }
                    (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        [("retry-after", "7200")],
                        r#"{"error":{"code":"INFERENCE_CAP_ERROR","message":"Daily free limit reached on model z-ai/glm-5.3-flash. Try again in 2h 30m"}}"#,
                    )
                }
            }
        }),
    );
    let ag_hits = Arc::new(AtomicUsize::new(0));
    let app=app.route("/v1internal:streamGenerateContent",post({let hits=ag_hits.clone(); move |axum::extract::Query(query):axum::extract::Query<std::collections::HashMap<String,String>>, axum::Json(body):axum::Json<serde_json::Value>| {let hits=hits.clone();async move {
        hits.fetch_add(1,Ordering::SeqCst); assert_eq!(query.get("alt").map(String::as_str),Some("sse")); assert_eq!(body["project"],"mock-project");
        assert_eq!(body["request"]["generationConfig"]["maxOutputTokens"],1);
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"x\"}]},\"finishReason\":\"STOP\"}]}\n\n"
    }}}));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = common::unique_temp_dir("warmup-http");
    std::fs::write(
        dir.join("codex-test.json"),
        common::create_auth_file_json("test", "test", "mock", Some(&base)),
    )
    .unwrap();
    let state = Arc::new(
        AppState::new(&GatewayConfig {
            auth_dir: dir.clone(),
            config_path: dir.join("config.yaml"),
            refresh_url: format!("{base}/refresh"),
            auth_refresh_enabled: false,
            ..Default::default()
        })
        .unwrap(),
    );
    let member = state.pool.load().members[0].clone();
    assert!(!mahoquot_gateway::warmup::available_models(&state, &member).is_empty());
    for i in 0..4 {
        mode.store(i, Ordering::SeqCst);
        let r = warm_account(&state, &member).await;
        println!("mock case {i}: {}", serde_json::to_string(&r).unwrap());
        assert_eq!(r.ok, i == 3);
        assert_eq!(r.stream_validated, i == 3);
        assert_eq!(r.status, 200);
    }
    mode.store(4, Ordering::SeqCst);
    let signal = entered.acquire();
    let calls = (0..5).map(|_| warm_account(&state, &member));
    let joined = futures::future::join_all(calls);
    tokio::pin!(joined);
    assert!(futures::poll!(&mut joined).is_pending());
    tokio::time::timeout(std::time::Duration::from_secs(5), signal)
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert_eq!(hits.load(Ordering::SeqCst), 5);
    release.add_permits(1);
    let results = joined.await;
    assert!(results.iter().all(|r| r == &results[0]));
    for index in 0..6 {
        std::fs::write(
            dir.join(format!("codex-extra-{index}.json")),
            common::create_auth_file_json(&format!("extra-{index}"), "mock", "mock", Some(&base)),
        )
        .unwrap();
    }
    state.rescan_pool().unwrap();
    let bulk = mahoquot_gateway::warmup::warm_all(&state);
    tokio::pin!(bulk);
    assert!(futures::poll!(&mut bulk).is_pending());
    // Each blocked upstream reports entry through a counted channel, not a delay.
    for _ in 0..4 {
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
    }
    assert_eq!(hits.load(Ordering::SeqCst), 9);
    release.add_permits(7);
    let bulk_results = bulk.await;
    assert_eq!(bulk_results.len(), 7);
    assert!(bulk_results.iter().all(|r| r.ok));
    assert_eq!(hits.load(Ordering::SeqCst), 12);
    for index in 0..6 {
        std::fs::remove_file(dir.join(format!("codex-extra-{index}.json"))).unwrap();
    }
    state.rescan_pool().unwrap();
    let guard = member.begin_activity();
    assert_eq!(
        warm_account(&state, &member).await.detail.as_deref(),
        Some("account_active")
    );
    state.rescan_pool().unwrap();
    let reloaded = state.find_member(&member.id).unwrap();
    assert!(Arc::ptr_eq(
        &member.active_requests,
        &reloaded.active_requests
    ));
    assert_eq!(reloaded.active_requests.load(Ordering::Relaxed), 1);
    drop(guard);
    assert_eq!(reloaded.active_requests.load(Ordering::Relaxed), 0);
    reloaded.set_health(mahoquot_types::Health::Disabled);
    assert!(!warm_account(&state, &member).await.ok);
    assert_eq!(hits.load(Ordering::SeqCst), 12);
    println!("mock hits={}", hits.load(Ordering::SeqCst));
    let gateway = mahoquot_gateway::routes::create_app(state.clone());
    use tower::ServiceExt;
    let response = gateway
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/v0/management/warmup/settings/provider/codex")
                .method("PUT")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"enabled":true,"model":null,"idle_secs":3600,"min_interval_secs":300})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    println!("PUT policy 200 {}", String::from_utf8_lossy(&bytes));
    let loaded = AppState::new(&GatewayConfig {
        auth_dir: dir.clone(),
        config_path: dir.join("config.yaml"),
        ..Default::default()
    })
    .unwrap();
    assert!(
        loaded
            .settings
            .current()
            .warmup
            .effective_provider_policy("codex")
            .enabled
    );
    std::fs::write(dir.join("generic-cline.json"),json!({"type":"generic","identity_slug":"cline-fixture","provider":"cline","adapter":"openai","base_url":base,"upstream_override":base,"api_key":"mock","models":["z-ai/glm-5.3-flash"]}).to_string()).unwrap();
    state.rescan_pool().unwrap();
    let cline = state
        .pool
        .load()
        .members
        .iter()
        .find(|m| m.provider_name() == "cline")
        .unwrap()
        .clone();
    let first = warm_account(&state, &cline).await;
    println!("Cline429 {}", serde_json::to_string(&first).unwrap());
    assert_eq!(first.status, 429);
    assert_eq!(
        warm_account(&state, &cline).await.detail.as_deref(),
        Some("cooldown_model_quota")
    );
    assert_eq!(cline_hits.load(Ordering::SeqCst), 1);
    let usage = cline.usage_snapshot();
    let bucket = &usage.groups[0].buckets[0];
    assert_eq!(bucket.bucket_id.as_deref(), Some("z-ai/glm-5.3-flash"));
    assert_eq!(bucket.used_percent, Some(100.0));
    assert_eq!(
        bucket.reset_at_unix.unwrap() - usage.observed_at_unix.unwrap(),
        9000
    );
    cline.group_cooldowns.write().unwrap().clear();
    cline.set_usage(Default::default());
    mode.store(6, Ordering::SeqCst);
    assert!(warm_account(&state, &cline).await.ok);
    mode.store(5, Ordering::SeqCst);
    std::fs::write(dir.join("antigravity-fixture.json"),json!({"type":"antigravity","identity_slug":"ag-fixture","access_token":"mock","refresh_token":"mock","email":"mock@example.com","expired":"2099-01-01T00:00:00Z","project_id":"mock-project","upstream_override":base}).to_string()).unwrap();
    state.rescan_pool().unwrap();
    let ag = state.find_member("ag-fixture").unwrap();
    let ag_result = warm_account(&state, &ag).await;
    assert!(ag_result.ok, "{ag_result:?}");
    assert_eq!(ag_hits.load(Ordering::SeqCst), 1);
    println!("AG mock {}", serde_json::to_string(&ag_result).unwrap());
    let refreshed = warm_account(&state, &state.find_member("test").unwrap()).await;
    assert!(refreshed.ok, "{refreshed:?}");
    mode.store(3, Ordering::SeqCst);
    state.settings.mutate(|settings| {
        settings.api_keys=vec!["master-fixture".into()];
        settings.scoped_api_keys=vec![serde_json::from_value(json!({"id":"scope","name":"scope","key_identifier":mahoquot_gateway::request_history::stable_key_identifier("scoped-fixture"),"key_prefix":"scoped","raw_key":"scoped-fixture","is_active":true})).unwrap()];
    }).unwrap();
    let gateway_listener = {
        let mut bound = None;
        for port in 18840..=18899 {
            if let Ok(l) =
                tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await
            {
                bound = Some(l);
                break;
            }
        }
        bound.unwrap()
    };
    let gateway_base = format!("http://{}", gateway_listener.local_addr().unwrap());
    let gateway = mahoquot_gateway::routes::create_app(state.clone());
    let gateway_task = tokio::spawn(async move {
        axum::serve(gateway_listener, gateway).await.unwrap();
    });
    let client = reqwest::Client::new();
    for path in [
        "/v0/management/warmup/settings",
        "/v0/management/warmup/status",
    ] {
        for (key, expected) in [("master-fixture", 200), ("scoped-fixture", 403)] {
            let response = client
                .get(format!("{gateway_base}{path}"))
                .bearer_auth(key)
                .send()
                .await
                .unwrap();
            let code = response.status().as_u16();
            let body = response.text().await.unwrap();
            println!("HTTP GET {path} {key}: {code} {body}");
            assert_eq!(code, expected);
        }
    }
    for (key, expected) in [("master-fixture", 200), ("scoped-fixture", 403)] {
        let response = client
            .post(format!("{gateway_base}/admin/accounts/test/warmup"))
            .bearer_auth(key)
            .send()
            .await
            .unwrap();
        let code = response.status().as_u16();
        let body = response.text().await.unwrap();
        println!("HTTP manual {key}: {code} {body}");
        assert_eq!(code, expected);
    }
    let invalid = client
        .put(format!(
            "{gateway_base}/v0/management/warmup/settings/provider/codex"
        ))
        .bearer_auth("master-fixture")
        .json(&json!({"enabled":true,"model":null,"idle_secs":0,"min_interval_secs":300}))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 400);
    println!("HTTP invalid PUT: 400 {}", invalid.text().await.unwrap());
    mode.store(4, Ordering::SeqCst);
    for index in 0..6 {
        std::fs::write(
            dir.join(format!("codex-shutdown-{index}.json")),
            common::create_auth_file_json(
                &format!("shutdown-{index}"),
                "mock",
                "mock",
                Some(&base),
            ),
        )
        .unwrap();
    }
    state.rescan_pool().unwrap();
    let members: Vec<_> = state
        .pool
        .load()
        .members
        .iter()
        .filter(|m| m.provider_name() == "codex")
        .cloned()
        .collect();
    let pending = futures::future::join_all(members.iter().map(|m| warm_account(&state, m)));
    tokio::pin!(pending);
    // Drain entry permits from the completed bulk batch before subscribing.
    while let Ok(permit) = entered.try_acquire() {
        permit.forget();
    }
    assert!(futures::poll!(&mut pending).is_pending());
    let before_shutdown_hits = hits.load(Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.acquire_many(4))
        .await
        .unwrap()
        .unwrap()
        .forget();
    state.shutdown.notify_waiters();
    let stopped = tokio::time::timeout(std::time::Duration::from_secs(5), pending)
        .await
        .unwrap();
    assert!(stopped
        .iter()
        .all(|r| r.detail.as_deref() == Some("shutdown")));
    assert_eq!(hits.load(Ordering::SeqCst), before_shutdown_hits + 4);
    mode.store(3, Ordering::SeqCst);
    assert!(warm_account(&state, &members[0]).await.ok);
    // Enable due automatic work, then stop its blocked batch via the same event.
    for member in &members {
        *member.last_activity.lock().unwrap() =
            tokio::time::Instant::now() - std::time::Duration::from_secs(86401);
    }
    state
        .settings
        .mutate(|s| {
            s.warmup
                .providers
                .get_mut("codex")
                .unwrap()
                .min_interval_secs = 1;
        })
        .unwrap();
    // New account has no prior attempt, hence no interval dependency.
    std::fs::write(
        dir.join("codex-auto.json"),
        common::create_auth_file_json("auto", "mock", "mock", Some(&base)),
    )
    .unwrap();
    state.rescan_pool().unwrap();
    *state
        .find_member("auto")
        .unwrap()
        .last_activity
        .lock()
        .unwrap() = tokio::time::Instant::now() - std::time::Duration::from_secs(86401);
    mode.store(4, Ordering::SeqCst);
    let automatic = mahoquot_gateway::warmup::spawn_warmup_loop(
        state.clone(),
        std::time::Duration::from_secs(15),
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    state.shutdown.notify_waiters();
    tokio::time::timeout(std::time::Duration::from_secs(5), automatic)
        .await
        .unwrap()
        .unwrap();
    gateway_task.abort();
    let _ = gateway_task.await;
    server.abort();
    let _ = server.await;
    std::fs::remove_dir_all(dir).unwrap();
}
