mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::State as AxumState;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use mahoquot_gateway::account::BenchKind;
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::inbound::ApiKeys;
use mahoquot_gateway::relay::{
    classify_kiro_payment_error, cooldown_deadline_from_headers, KiroPaymentClassification,
};
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use mahoquot_gateway::usage::parse_kiro_usage_summary;
use mahoquot_types::{Health, PoolMember, Strategy};
use sha2::{Digest, Sha256};

const MODEL: &str = "kiro/claude-opus-5.5";
const PROFILE_ARN: &str = "arn:aws:codewhisperer:eu-central-1:123:profile/t40";
const API_KEY: &str = "relay-key";

static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
enum Reply {
    MonthlyLimit,
    CauseNestedMonthlyLimit,
    OverageLimit,
    CreditBalanceExhausted,
    SeatUnavailable,
    ConversationLimit,
    UnknownReason,
    EventStream,
    CreateResponseSse,
}

#[derive(Clone)]
struct CapturedReq {
    path: String,
    headers: HeaderMap,
    body: serde_json::Value,
}

#[derive(Clone)]
struct MockState {
    label: &'static str,
    order: Arc<Mutex<Vec<String>>>,
    captured: Arc<Mutex<Vec<CapturedReq>>>,
    reply: Reply,
    create_response_flag: Arc<AtomicBool>,
}

fn create_response_feature_key() -> String {
    let mut hasher = Sha256::new();
    hasher.update(mahoquot_providers::KIRO_FEATURE_CONFIG_SALT.as_bytes());
    hasher.update(b"create_response_api");
    hex::encode(hasher.finalize())
}

async fn mock_feature_config(
    AxumState(state): AxumState<MockState>,
    headers: HeaderMap,
) -> Response {
    if !state.create_response_flag.load(Ordering::SeqCst) {
        return StatusCode::NOT_FOUND.into_response();
    }
    state.captured.lock().unwrap().push(CapturedReq {
        path: "/featureConfiguration".to_string(),
        headers,
        body: serde_json::Value::Null,
    });
    let key = create_response_feature_key();
    let payload = serde_json::json!({
        "configuration": {
            key: "create_response"
        }
    });
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        payload.to_string(),
    )
        .into_response()
}

async fn mock_upstream(
    AxumState(state): AxumState<MockState>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    state.order.lock().unwrap().push(state.label.to_string());
    let parsed = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    state.captured.lock().unwrap().push(CapturedReq {
        path: uri.path().to_string(),
        headers,
        body: parsed,
    });
    match state.reply {
        Reply::MonthlyLimit => (
            StatusCode::PAYMENT_REQUIRED,
            [("content-type", "application/json")],
            br#"{"message":"You have reached the limit.","reason":"MONTHLY_REQUEST_COUNT"}"#
                .to_vec(),
        )
            .into_response(),
        Reply::CauseNestedMonthlyLimit => (
            StatusCode::PAYMENT_REQUIRED,
            [("content-type", "application/json")],
            br#"{"message":"You have reached the limit.","cause":{"name":"ServiceQuotaExceededException","reason":"MONTHLY_REQUEST_COUNT"}}"#
                .to_vec(),
        )
            .into_response(),
        Reply::OverageLimit => (
            StatusCode::PAYMENT_REQUIRED,
            [("content-type", "application/json")],
            br#"{"message":"Overage request limit exceeded.","reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"}"#
                .to_vec(),
        )
            .into_response(),
        Reply::CreditBalanceExhausted => (
            StatusCode::PAYMENT_REQUIRED,
            [("content-type", "application/json")],
            br#"{"error":{"type":"insufficient_quota","code":"credit_balance_exhausted","message":"No credits remaining"}}"#
                .to_vec(),
        )
            .into_response(),
        Reply::SeatUnavailable => (
            StatusCode::PAYMENT_REQUIRED,
            [("content-type", "application/json")],
            br#"{"message":"Seat unavailable","reason":"SEAT_UNAVAILABLE"}"#.to_vec(),
        )
            .into_response(),
        Reply::ConversationLimit => (
            StatusCode::PAYMENT_REQUIRED,
            [("content-type", "application/json")],
            br#"{"message":"Conversation limit exceeded","reason":"CONVERSATION_LIMIT_EXCEEDED"}"#
                .to_vec(),
        )
            .into_response(),
        Reply::UnknownReason => (
            StatusCode::PAYMENT_REQUIRED,
            [("content-type", "application/json")],
            br#"{"message":"MONTHLY_REQUEST_COUNT","reason":"OTHER"}"#.to_vec(),
        )
            .into_response(),
        Reply::EventStream => (
            StatusCode::OK,
            [("content-type", "application/vnd.amazon.eventstream")],
            frame(br#"{"content":"kiro-rotated"}"#),
        )
            .into_response(),
        Reply::CreateResponseSse => {
            let sse = [
                r#"data: {"type":"kiro:keep_alive"}"#,
                r#"data: {"type":"response.created","response":{"id":"resp_v3_1"}}"#,
                r#"data: {"type":"response.reasoning_text.delta","delta":"thinking"}"#,
                r#"data: {"type":"response.output_text.delta","delta":"kiro-v3-responses"}"#,
                r#"data: {"type":"response.completed","response":{"status":"completed","usage":{"kiro:context_usage_percentage":15.0}}}"#,
            ]
            .join("\n\n")
                + "\n\n";
            (
                StatusCode::OK,
                [("content-type", "text/event-stream")],
                sse.into_bytes(),
            )
                .into_response()
        }
    }
}

fn frame(payload: &[u8]) -> Vec<u8> {
    fn crc(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb88320 & (0u32.wrapping_sub(crc & 1)));
            }
        }
        !crc
    }
    let mut frame = ((16 + payload.len()) as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(&0u32.to_be_bytes());
    frame.extend_from_slice(&crc(&frame).to_be_bytes());
    frame.extend_from_slice(payload);
    frame.extend_from_slice(&crc(&frame).to_be_bytes());
    frame
}

async fn spawn_mock_with_flag(
    label: &'static str,
    reply: Reply,
    order: Arc<Mutex<Vec<String>>>,
    captured: Arc<Mutex<Vec<CapturedReq>>>,
    use_create_response: bool,
) -> (String, tokio::task::JoinHandle<()>) {
    let state = MockState {
        label,
        order,
        captured,
        reply,
        create_response_flag: Arc::new(AtomicBool::new(use_create_response)),
    };
    let app = Router::new()
        .route("/featureConfiguration", get(mock_feature_config))
        .fallback(post(mock_upstream))
        .with_state(state);
    let mut bound = None;
    for port in 18840..=18899 {
        match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => {
                bound = Some(listener);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => panic!("fixture bind: {error}"),
        }
    }
    let listener = bound.expect("available fixture port");
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    (upstream, task)
}

async fn spawn_mock(
    label: &'static str,
    reply: Reply,
    order: Arc<Mutex<Vec<String>>>,
) -> (String, tokio::task::JoinHandle<()>) {
    spawn_mock_with_flag(
        label,
        reply,
        order,
        Arc::new(Mutex::new(Vec::new())),
        false,
    )
    .await
}

fn credential(slug: &str, token: &str, upstream: &str) -> String {
    serde_json::json!({
        "identity_slug": slug,
        "access_token": token,
        "refresh_token": format!("{slug}-refresh"),
        "email": format!("{slug}@kiro.test"),
        "expired": "2099-01-01T00:00:00Z",
        "type": "kiro",
        "auth_mode": "idc",
        "region": "eu-central-1",
        "profileArn": PROFILE_ARN,
        "upstream_override": upstream,
    })
    .to_string()
}

async fn start_gateway(
    accounts: &[(&str, &str)],
) -> (
    String,
    std::path::PathBuf,
    Arc<AppState>,
    tokio::task::JoinHandle<()>,
) {
    let sequence = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let auth_dir = common::unique_temp_dir(&format!("t40-kiro-402-{sequence}"));
    let mut order = Vec::new();
    for (slug, upstream) in accounts {
        let file = format!("{slug}.json");
        std::fs::write(
            auth_dir.join(&file),
            credential(slug, &format!("{slug}-token"), upstream),
        )
        .unwrap();
        order.push(file);
    }
    std::fs::write(
        auth_dir.join(".mahoquot-account-order.json"),
        serde_json::to_vec(&order).unwrap(),
    )
    .unwrap();

    let config = GatewayConfig {
        auth_dir: auth_dir.clone(),
        api_keys: ApiKeys::from_env_value(API_KEY),
        auth_refresh_enabled: false,
        max_failover: 4,
        strategy: Strategy::FillFirst,
        config_path: auth_dir.join("config.yaml"),
        ..GatewayConfig::default()
    };
    let state = Arc::new(AppState::new(&common::managed_ref(&config)).unwrap());
    assert_eq!(state.pool.load().members.len(), accounts.len());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = create_app(state.clone());
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    (format!("http://{addr}"), auth_dir, state, task)
}

async fn ask(gateway: &str) -> (StatusCode, reqwest::header::HeaderMap, String) {
    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .bearer_auth(API_KEY)
        .json(&serde_json::json!({
            "model": MODEL,
            "stream": true,
            "messages": [
                {"role":"system","content":"be brief"},
                {"role":"user","content":"ping"}
            ]
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    (status, headers, response.text().await.unwrap())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[test]
fn kiro_v3_payment_classifier_distinguishes_every_documented_reason() {
    assert_eq!(
        classify_kiro_payment_error(br#"{"reason":"MONTHLY_REQUEST_COUNT"}"#),
        KiroPaymentClassification::QuotaExhausted(BenchKind::MonthlyQuotaExhausted)
    );
    assert_eq!(
        classify_kiro_payment_error(
            br#"{"cause":{"name":"ServiceQuotaExceededException","reason":"DAILY_REQUEST_COUNT"}}"#
        ),
        KiroPaymentClassification::QuotaExhausted(BenchKind::QuotaExhausted)
    );
    assert_eq!(
        classify_kiro_payment_error(br#"{"reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"}"#),
        KiroPaymentClassification::QuotaExhausted(BenchKind::OutOfCredit)
    );
    assert_eq!(
        classify_kiro_payment_error(
            br#"{"error":{"type":"insufficient_quota","code":"credit_balance_exhausted"}}"#
        ),
        KiroPaymentClassification::QuotaExhausted(BenchKind::OutOfCredit)
    );
    assert_eq!(
        classify_kiro_payment_error(br#"{"reason":"SEAT_UNAVAILABLE"}"#),
        KiroPaymentClassification::SeatUnavailable
    );
    assert_eq!(
        classify_kiro_payment_error(br#"{"reason":"CONVERSATION_LIMIT_EXCEEDED"}"#),
        KiroPaymentClassification::ConversationLimit
    );
    assert_eq!(
        classify_kiro_payment_error(br#"{"message":"MONTHLY_REQUEST_COUNT","reason":"OTHER"}"#),
        KiroPaymentClassification::Unclassified
    );
    assert_eq!(
        classify_kiro_payment_error(b"not json"),
        KiroPaymentClassification::Unclassified
    );
}

#[test]
fn kiro_v3_usage_parser_reads_scientific_float_reset_and_management_url() {
    let usage = parse_kiro_usage_summary(
        &serde_json::json!({
            "nextDateReset": 1.7960832e9,
            "usageBreakdownList": [{
                "displayName": "Credit",
                "currentUsage": 10000,
                "usageLimit": 10000,
                "nextDateReset": 1.7960832e9
            }]
        }),
        1_795_772_731,
    );
    let bucket = &usage.groups[0].buckets[0];
    assert_eq!(bucket.used_percent, Some(100.0));
    assert_eq!(bucket.reset_at_unix, Some(1_796_083_200));

    let url = mahoquot_providers::kiro_usage_url(None, "us-east-1", Some(PROFILE_ARN));
    assert!(url.starts_with("https://management.us-east-1.kiro.dev/Get-Usage-Limits?origin=KIRO_CLI&resourceType=AGENTIC_REQUEST&profileArn="));
}

#[test]
fn kiro_v3_ratelimit_ms_header_drives_cooldown_deadline() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-amzn-kiro-ratelimit-retry-after", "4500".parse().unwrap());
    let now = 1_700_000_000_000i64;
    let deadline = cooldown_deadline_from_headers(&headers, now);
    assert_eq!(deadline, now + 5_000);
}

fn build_ustar_entry(path: &str, body: &[u8]) -> Vec<u8> {
    let mut hdr = [0u8; 512];
    let name_bytes = path.as_bytes();
    hdr[..name_bytes.len()].copy_from_slice(name_bytes);
    hdr[100..108].copy_from_slice(b"0000644\0");
    hdr[108..116].copy_from_slice(b"0000000\0");
    hdr[116..124].copy_from_slice(b"0000000\0");
    let size_oct = format!("{:011o}\0", body.len());
    hdr[124..136].copy_from_slice(size_oct.as_bytes());
    hdr[136..148].copy_from_slice(b"00000000000\0");
    hdr[148..156].copy_from_slice(b"        ");
    hdr[156] = b'0';
    hdr[257..263].copy_from_slice(b"ustar\0");
    hdr[263..265].copy_from_slice(b"00");
    let checksum: u32 = hdr.iter().map(|&b| u32::from(b)).sum();
    let chk = format!("{checksum:06o}\0 ");
    hdr[148..156].copy_from_slice(chk.as_bytes());

    let mut out = Vec::new();
    out.extend_from_slice(&hdr);
    out.extend_from_slice(body);
    let pad = (512 - (body.len() % 512)) % 512;
    out.resize(out.len() + pad, 0);
    out
}

#[test]
fn kiro_v3_all_auth_modes_map_exact_token_type_headers() {
    use mahoquot_providers::{kiro_token_type_header, KiroAccount, KiroAuthMode};
    for (raw, mode, expected_header, expected_static) in [
        ("social", KiroAuthMode::Social, None, false),
        ("IdC", KiroAuthMode::Idc, Some("SSO_OIDC"), false),
        ("external_idp", KiroAuthMode::ExternalIdp, Some("EXTERNAL_IDP"), true),
        ("api_key", KiroAuthMode::ApiKey, Some("API_KEY"), true),
        ("machine_token", KiroAuthMode::MachineToken, Some("KIRO_MACHINE_TOKEN"), true),
    ] {
        let doc = serde_json::json!({
            "type": "kiro",
            "authMethod": raw,
            "kiroApiKey": "secret-k-key",
            "access_token": if raw == "api_key" { "" } else { "secret-tok" },
        });
        let account: KiroAccount = serde_json::from_value(doc).expect("deserialize");
        assert_eq!(account.auth_mode, mode);
        assert_eq!(kiro_token_type_header(account.auth_mode), expected_header);
        assert_eq!(account.is_static_token(), expected_static);
        if raw == "api_key" {
            assert_eq!(account.effective_access_token(), "secret-k-key");
        } else {
            assert_eq!(account.effective_access_token(), "secret-tok");
        }
        let dbg = format!("{account:?}");
        assert!(!dbg.contains("secret-k-key"));
        assert!(!dbg.contains("secret-tok"));
    }
}

#[tokio::test]
async fn kiro_v3_manifest_refresh_updates_cli_and_kas_versions_from_tar_zst() {
    let mut inner_tar = build_ustar_entry(
        "node_modules/@kiro/agent/package.json",
        br#"{"name":"@kiro/agent","version":"0.67.9-hotfix.2"}"#,
    );
    inner_tar.extend_from_slice(&[0u8; 1024]);
    let inner_zst = zstd::bulk::compress(&inner_tar, 1).expect("compress inner");

    let mut fake_elf = vec![0x7f, b'E', b'L', b'F'];
    fake_elf.extend_from_slice(&[0x41u8; 4096]);
    fake_elf.extend_from_slice(&inner_zst);
    let mut outer_tar = build_ustar_entry("kirocli/bin/kiro-cli", &fake_elf);
    outer_tar.extend_from_slice(&[0u8; 1024]);
    let outer_zst = Arc::new(zstd::bulk::compress(&outer_tar, 1).expect("compress outer"));
    let pkg_bytes = Arc::clone(&outer_zst);

    let app = Router::new()
        .route(
            "/stable/latest/manifest.json",
            get(|| async {
                (
                    StatusCode::OK,
                    [("content-type", "application/json")],
                    r#"{"version":"3.9.1","packages":[{"fileType":"tarZst","size":1024,"download":"3.9.1/kirocli-aarch64-linux.tar.zst"}]}"#,
                )
            }),
        )
        .route(
            "/stable/3.9.1/kirocli-aarch64-linux.tar.zst",
            get(move || {
                let bytes = Arc::clone(&pkg_bytes);
                async move { (StatusCode::OK, (*bytes).clone()) }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    assert!(mahoquot_providers::kiro_cli_user_agent().starts_with("KiroCLI/"));
    assert!(!mahoquot_providers::kiro_cli_user_agent().contains("KiroIDE-0.7.45"));

    let client = reqwest::Client::new();
    let changed = mahoquot_providers::refresh_kiro_cli_version_from_url(
        &client,
        &format!("http://{addr}/stable/latest/manifest.json"),
    )
    .await
    .unwrap();
    assert!(changed);
    assert_eq!(mahoquot_providers::current_kiro_cli_version(), "3.9.1");
    assert_eq!(mahoquot_providers::current_kiro_kas_version(), "0.67.9-hotfix.2");
    assert!(mahoquot_providers::kiro_cli_user_agent().contains("KiroCLI/3.9.1 KAS/0.67.9-hotfix.2"));
    assert!(!mahoquot_providers::update_kiro_cli_version("3.6.0"));
    assert!(!mahoquot_providers::update_kiro_cli_version("not-a-version"));
    assert_eq!(mahoquot_providers::current_kiro_cli_version(), "3.9.1");

    task.abort();
}

#[tokio::test]
async fn kiro_v3_routes_create_response_experiment_accounts_to_v1_responses() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (upstream, mock) =
        spawn_mock_with_flag("v3", Reply::CreateResponseSse, order.clone(), captured.clone(), true)
            .await;
    let (gateway, auth_dir, _state, gateway_task) = start_gateway(&[("kiro-v3", &upstream)]).await;

    let (status, _, body) = ask(&gateway).await;
    assert_eq!(status, StatusCode::OK, "gateway response: {body}");
    assert!(body.contains("kiro-v3-responses"), "client body: {body}");

    let reqs = captured.lock().unwrap();
    assert_eq!(reqs[0].path, "/featureConfiguration");
    assert_eq!(
        reqs[0].headers.get("x-amzn-kiro-profile-arn").and_then(|v| v.to_str().ok()),
        Some(PROFILE_ARN)
    );
    let post_req = reqs.iter().find(|r| r.path == "/v1/responses").expect("POST /v1/responses");
    assert_eq!(
        post_req.headers.get("x-amzn-kiro-origin").and_then(|v| v.to_str().ok()),
        Some("KIRO_CLI")
    );
    assert_eq!(
        post_req.headers.get("x-amzn-kiro-profile").and_then(|v| v.to_str().ok()),
        Some(PROFILE_ARN)
    );
    assert_eq!(
        post_req.headers.get("x-amzn-kiro-attempt").and_then(|v| v.to_str().ok()),
        Some("1;max=3")
    );
    assert_eq!(
        reqs[0].headers.get("tokentype").and_then(|v| v.to_str().ok()),
        Some("SSO_OIDC")
    );
    assert_eq!(
        post_req.headers.get("tokentype").and_then(|v| v.to_str().ok()),
        Some("SSO_OIDC")
    );
    assert_eq!(post_req.headers.get_all("accept").iter().count(), 1);
    assert!(post_req
        .headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ua| {
            ua.starts_with("KiroCLI/")
                && ua.contains("KAS/")
                && ua.ends_with("acp-client/kiro-cli")
        }));
    assert_eq!(post_req.body["origin"], "KIRO_CLI");
    assert_eq!(post_req.body["instructions"], "be brief");
    assert_eq!(post_req.body["stream"], true);

    gateway_task.abort();
    mock.abort();
    std::fs::remove_dir_all(auth_dir).ok();
}

#[tokio::test]
async fn unknown_or_conversation_402_preserves_error_without_benching_or_rotation() {
    for (reply, expected_reason) in [
        (Reply::UnknownReason, "OTHER"),
        (Reply::ConversationLimit, "CONVERSATION_LIMIT_EXCEEDED"),
    ] {
        let order = Arc::new(Mutex::new(Vec::new()));
        let (a, mock_a) = spawn_mock("a", reply, order.clone()).await;
        let (b, mock_b) = spawn_mock("b", Reply::EventStream, order.clone()).await;
        let (gateway, auth_dir, state, task) =
            start_gateway(&[("kiro-a", &a), ("kiro-b", &b)]).await;

        let (status, headers, body) = ask(&gateway).await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
        assert!(body.contains(expected_reason), "body: {body}");
        assert!(!headers.contains_key("retry-after"));
        assert_eq!(*order.lock().unwrap(), ["a"]);
        let member = state.find_member("kiro-a").unwrap();
        assert!(matches!(member.health(), Health::Available));
        assert!(member.group_available(MODEL, now_ms()));

        task.abort();
        mock_a.abort();
        mock_b.abort();
        std::fs::remove_dir_all(auth_dir).ok();
    }
}

#[tokio::test]
async fn confirmed_402_reasons_bench_model_only_and_rotate_to_funded_account() {
    for (reply, expected_kind) in [
        (Reply::MonthlyLimit, BenchKind::MonthlyQuotaExhausted),
        (Reply::CauseNestedMonthlyLimit, BenchKind::MonthlyQuotaExhausted),
        (Reply::OverageLimit, BenchKind::OutOfCredit),
        (Reply::CreditBalanceExhausted, BenchKind::OutOfCredit),
        (Reply::SeatUnavailable, BenchKind::QuotaExhausted),
    ] {
        let order = Arc::new(Mutex::new(Vec::new()));
        let (upstream_a, mock_a) = spawn_mock("a", reply, order.clone()).await;
        let (upstream_b, mock_b) = spawn_mock("b", Reply::EventStream, order.clone()).await;
        let (gateway, auth_dir, state, gateway_task) =
            start_gateway(&[("kiro-a", &upstream_a), ("kiro-b", &upstream_b)]).await;

        let (status, _, body) = ask(&gateway).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert!(body.contains("kiro-rotated"), "client body: {body}");
        assert_eq!(*order.lock().unwrap(), vec!["a".to_string(), "b".to_string()]);

        let now = now_ms();
        let a = state.find_member("kiro-a").expect("account a");
        let b = state.find_member("kiro-b").expect("account b");
        assert!(a.model_reset_at_unix(MODEL).is_some());
        assert!(!a.group_available(MODEL, now));
        assert_eq!(a.bench_kind_for_model(MODEL, now), Some(expected_kind));
        assert!(matches!(a.health(), Health::Available));
        assert!(a.group_available("kiro/some-other-model", now));
        assert!(a.unsupported_models.read().unwrap().is_empty());
        assert_eq!(b.model_reset_at_unix(MODEL), None);

        gateway_task.abort();
        mock_a.abort();
        mock_b.abort();
        std::fs::remove_dir_all(auth_dir).ok();
    }
}

#[tokio::test]
async fn every_kiro_account_confirmed_out_of_plan_answers_retryable_503() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let (upstream_a, mock_a) = spawn_mock("a", Reply::MonthlyLimit, order.clone()).await;
    let (upstream_b, mock_b) = spawn_mock("b", Reply::OverageLimit, order.clone()).await;
    let (gateway, auth_dir, _state, gateway_task) =
        start_gateway(&[("kiro-a", &upstream_a), ("kiro-b", &upstream_b)]).await;

    let (status, headers, body) = ask(&gateway).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    assert_eq!(*order.lock().unwrap(), vec!["a".to_string(), "b".to_string()]);

    let retry_after = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .expect("503 must carry Retry-After");
    assert!(retry_after > 0);

    let parsed: serde_json::Value = serde_json::from_str(&body).expect("503 JSON");
    assert_eq!(parsed["error"]["code"], "MODEL_QUOTA_EXHAUSTED");

    gateway_task.abort();
    mock_a.abort();
    mock_b.abort();
    std::fs::remove_dir_all(auth_dir).ok();
}

#[tokio::test]
async fn kiro_v3_monthly_402_uses_real_reset_renders_monthly_eta_and_clears_on_recovery() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let (upstream_a, mock_a) = spawn_mock("a", Reply::MonthlyLimit, order.clone()).await;
    let (gateway, auth_dir, state, gateway_task) = start_gateway(&[("kiro-a", &upstream_a)]).await;

    let now_unix = now_ms() / 1000;
    let reset_unix = now_unix + 3 * 86_400 + 4 * 3600;
    let a = state.find_member("kiro-a").expect("account a");
    a.set_usage(parse_kiro_usage_summary(
        &serde_json::json!({
            "nextDateReset": reset_unix as f64,
            "usageBreakdownList": [{
                "displayName": "Credit",
                "currentUsage": 10000,
                "usageLimit": 10000,
                "nextDateReset": reset_unix as f64
            }]
        }),
        now_unix,
    ));

    let (status, headers, body) = ask(&gateway).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    let retry_after = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .expect("Retry-After header");
    assert!(retry_after > 3 * 86_400, "retry_after={retry_after}");
    assert!(
        body.contains("monthly-exhausted") && body.contains("3d "),
        "expected monthly-exhausted with real multi-day ETA: {body}"
    );
    assert!(!a.group_available(MODEL, now_ms()));

    // When quota recovers, clearing model cooldowns immediately restores routing.
    a.clear_model_cooldowns();
    assert!(a.group_available(MODEL, now_ms()));

    gateway_task.abort();
    mock_a.abort();
    std::fs::remove_dir_all(auth_dir).ok();
}

#[tokio::test]
async fn kiro_v3_responses_413_returns_verbatim_without_account_fault_or_rotation() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let key = create_response_feature_key();
    let order_a = order.clone();
    let app_a = Router::new()
        .route(
            "/featureConfiguration",
            get({
                let key = key.clone();
                move || {
                    let key = key.clone();
                    async move {
                        (
                            StatusCode::OK,
                            [("content-type", "application/json")],
                            serde_json::json!({"configuration": {key: "create_response"}})
                                .to_string(),
                        )
                    }
                }
            }),
        )
        .fallback(post(move || {
            let order_a = order_a.clone();
            async move {
                order_a.lock().unwrap().push("a".to_string());
                (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    [("content-type", "application/json")],
                    r#"{"error":{"type":"invalid_request","code":"context_length_exceeded","message":"Input exceeds context window"}}"#,
                )
            }
        }));
    let listener_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_a = format!("http://{}", listener_a.local_addr().unwrap());
    let mock_a = tokio::spawn(async move { axum::serve(listener_a, app_a).await.unwrap() });

    let (upstream_b, mock_b) = spawn_mock("b", Reply::EventStream, order.clone()).await;
    let (gateway, auth_dir, state, gateway_task) =
        start_gateway(&[("kiro-a", &upstream_a), ("kiro-b", &upstream_b)]).await;

    let (status, _, body) = ask(&gateway).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "body: {body}");
    assert!(body.contains("context_length_exceeded"), "body: {body}");
    assert_eq!(*order.lock().unwrap(), vec!["a".to_string()]);
    let a = state.find_member("kiro-a").unwrap();
    assert!(matches!(a.health(), Health::Available));
    assert_eq!(a.fail_count.load(Ordering::Relaxed), 0);

    gateway_task.abort();
    mock_a.abort();
    mock_b.abort();
    std::fs::remove_dir_all(auth_dir).ok();
}

#[tokio::test]
async fn kiro_v3_responses_429_and_sse_preamble_402_bench_model_only_and_rotate() {
    let key = create_response_feature_key();

    // Case 1: HTTP 429 on /v1/responses must bench model-only (not account-wide Health::Cooldown)
    {
        let order = Arc::new(Mutex::new(Vec::new()));
        let order_a = order.clone();
        let app_a = Router::new()
            .route(
                "/featureConfiguration",
                get({
                    let key = key.clone();
                    move || {
                        let key = key.clone();
                        async move {
                            (
                                StatusCode::OK,
                                [("content-type", "application/json")],
                                serde_json::json!({"configuration": {key: "create_response"}})
                                    .to_string(),
                            )
                        }
                    }
                }),
            )
            .fallback(post(move || {
                let order_a = order_a.clone();
                async move {
                    order_a.lock().unwrap().push("a".to_string());
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        [
                            ("content-type", "application/json"),
                            ("x-amzn-kiro-ratelimit-retry-after", "60000"),
                        ],
                        r#"{"error":{"type":"too_many_requests","code":"rate_limit_exceeded","message":"Slow down"}}"#,
                    )
                }
            }));
        let listener_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_a = format!("http://{}", listener_a.local_addr().unwrap());
        let mock_a = tokio::spawn(async move { axum::serve(listener_a, app_a).await.unwrap() });
        let (upstream_b, mock_b) = spawn_mock("b", Reply::EventStream, order.clone()).await;
        let (gateway, auth_dir, state, gateway_task) =
            start_gateway(&[("kiro-a", &upstream_a), ("kiro-b", &upstream_b)]).await;

        let (status, _, body) = ask(&gateway).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert!(body.contains("kiro-rotated"), "body: {body}");
        assert_eq!(*order.lock().unwrap(), vec!["a".to_string(), "b".to_string()]);
        let a = state.find_member("kiro-a").unwrap();
        assert!(
            matches!(a.health(), Health::Available),
            "KiroResponses 429 must stay model-scoped, got {:?}",
            a.health()
        );
        assert!(!a.group_available(MODEL, now_ms()));
        assert!(a.group_available("kiro/some-other-model", now_ms()));

        gateway_task.abort();
        mock_a.abort();
        mock_b.abort();
        std::fs::remove_dir_all(auth_dir).ok();
    }

    // Case 2: HTTP 200 SSE preamble carrying response.failed(credit_balance_exhausted) rotates to B
    {
        let order = Arc::new(Mutex::new(Vec::new()));
        let order_a = order.clone();
        let app_a = Router::new()
            .route(
                "/featureConfiguration",
                get({
                    let key = key.clone();
                    move || {
                        let key = key.clone();
                        async move {
                            (
                                StatusCode::OK,
                                [("content-type", "application/json")],
                                serde_json::json!({"configuration": {key: "create_response"}})
                                    .to_string(),
                            )
                        }
                    }
                }),
            )
            .fallback(post(move || {
                let order_a = order_a.clone();
                async move {
                    order_a.lock().unwrap().push("a".to_string());
                    let sse = [
                        r#"data: {"type":"response.created","response":{"id":"resp_fail_1"}}"#,
                        r#"data: {"type":"response.failed","response":{"error":{"type":"insufficient_quota","code":"credit_balance_exhausted","message":"No credits"}}}"#,
                    ]
                    .join("\n\n")
                        + "\n\n";
                    (
                        StatusCode::OK,
                        [("content-type", "text/event-stream")],
                        sse,
                    )
                }
            }));
        let listener_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_a = format!("http://{}", listener_a.local_addr().unwrap());
        let mock_a = tokio::spawn(async move { axum::serve(listener_a, app_a).await.unwrap() });
        let (upstream_b, mock_b) = spawn_mock("b", Reply::EventStream, order.clone()).await;
        let (gateway, auth_dir, state, gateway_task) =
            start_gateway(&[("kiro-a", &upstream_a), ("kiro-b", &upstream_b)]).await;

        let (status, _, body) = ask(&gateway).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert!(body.contains("kiro-rotated"), "body: {body}");
        assert_eq!(*order.lock().unwrap(), vec!["a".to_string(), "b".to_string()]);
        let a = state.find_member("kiro-a").unwrap();
        assert_eq!(
            a.bench_kind_for_model(MODEL, now_ms()),
            Some(BenchKind::OutOfCredit)
        );

        gateway_task.abort();
        mock_a.abort();
        mock_b.abort();
        std::fs::remove_dir_all(auth_dir).ok();
    }
}

#[tokio::test]
async fn kiro_v3_responses_supports_input_file_and_stable_session_conversation_id() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (upstream, mock) =
        spawn_mock_with_flag("v3", Reply::CreateResponseSse, order.clone(), captured.clone(), true)
            .await;
    let (gateway, auth_dir, _state, gateway_task) = start_gateway(&[("kiro-v3", &upstream)]).await;

    let client = reqwest::Client::new();
    for _ in 0..2 {
        let resp = client
            .post(format!("{gateway}/v1/chat/completions"))
            .bearer_auth(API_KEY)
            .header("x-session-id", "sess-stable-42")
            .json(&serde_json::json!({
                "model": MODEL,
                "stream": true,
                "messages": [{
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "summarize"},
                        {"type": "file", "file": {"filename": "spec.pdf", "file_data": "data:application/pdf;base64,JVBERi0x"}}
                    ]
                }]
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body = resp.text().await.unwrap();
        assert_eq!(status, StatusCode::OK, "body: {body}");
    }

    let reqs = captured.lock().unwrap();
    let posts: Vec<&CapturedReq> = reqs.iter().filter(|r| r.path == "/v1/responses").collect();
    assert_eq!(posts.len(), 2);
    let conv_0 = posts[0].body["metadata"]["kiro:conversation_id"].as_str().unwrap();
    let conv_1 = posts[1].body["metadata"]["kiro:conversation_id"].as_str().unwrap();
    assert_eq!(conv_0, conv_1, "session affinity must yield stable conversation_id");
    let parts = posts[0].body["input"][0]["content"].as_array().unwrap();
    assert!(parts.iter().any(|p| p["type"] == "input_file" && p["filename"] == "spec.pdf"));

    gateway_task.abort();
    mock.abort();
    std::fs::remove_dir_all(auth_dir).ok();
}

#[tokio::test]
async fn kiro_v3_kas_extractor_handles_split_chunks_across_64k_boundaries() {
    // Pad ELF with 90 KiB + fake ZSTD magic noise so the real inner zstd frame
    // straddles multiple 64 KiB outer-decompression chunks.
    let mut inner_tar = build_ustar_entry("node_modules/filler/pad.bin", &vec![b'x'; 200_000]);
    inner_tar.extend_from_slice(&build_ustar_entry(
        "node_modules/@kiro/agent/package.json",
        br#"{"name":"@kiro/agent","version":"0.68.4-hotfix.3"}"#,
    ));
    inner_tar.extend_from_slice(&[0u8; 1024]);
    let inner_zst = zstd::bulk::compress(&inner_tar, 1).expect("compress inner");

    let mut fake_elf = vec![0x7f, b'E', b'L', b'F'];
    fake_elf.resize(65_500, 0x90);
    // False ZSTD magic right before a 64 KiB boundary
    fake_elf.extend_from_slice(&[0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x01, 0x02, 0x03]);
    fake_elf.resize(131_068, 0x55);
    // Real inner zstd frame starting 4 bytes before a 64 KiB boundary
    fake_elf.extend_from_slice(&inner_zst);
    let mut outer_tar = build_ustar_entry("kirocli/bin/kiro-cli", &fake_elf);
    outer_tar.extend_from_slice(&[0u8; 1024]);
    let outer_zst = Arc::new(zstd::bulk::compress(&outer_tar, 1).expect("compress outer"));

    let app = Router::new().route(
        "/pkg.tar.zst",
        get(move || {
            let bytes = Arc::clone(&outer_zst);
            async move {
                // Stream in tiny 512-byte chunks to stress every chunk boundary.
                let chunks: Vec<Result<Bytes, std::io::Error>> = bytes
                    .chunks(512)
                    .map(|c| Ok(Bytes::copy_from_slice(c)))
                    .collect();
                (StatusCode::OK, axum::body::Body::from_stream(futures::stream::iter(chunks)))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = reqwest::Client::new();
    let found = mahoquot_providers::fetch_kas_version_from_tar_zst_url(
        &client,
        &format!("http://{addr}/pkg.tar.zst"),
    )
    .await
    .unwrap();
    assert_eq!(found.as_deref(), Some("0.68.4-hotfix.3"));

    task.abort();
}

#[tokio::test]
async fn kiro_v3_anthropic_and_responses_inbound_preserve_documents_reasoning_and_sse_done_events() {
    let key = create_response_feature_key();
    let captured = Arc::new(Mutex::new(Vec::<CapturedReq>::new()));
    let cap_clone = captured.clone();
    let app = Router::new()
        .route(
            "/featureConfiguration",
            get({
                let key = key.clone();
                move || {
                    let key = key.clone();
                    async move {
                        (
                            StatusCode::OK,
                            [("content-type", "application/json")],
                            serde_json::json!({"configuration": {key: "create_response"}})
                                .to_string(),
                        )
                    }
                }
            }),
        )
        .fallback(post(move |uri: Uri, headers: HeaderMap, body: Bytes| {
            let cap_clone = cap_clone.clone();
            async move {
                let parsed = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                cap_clone.lock().unwrap().push(CapturedReq {
                    path: uri.path().to_string(),
                    headers,
                    body: parsed,
                });
                // Emit done-only text + reasoning summary delta + arguments.done to test edge events.
                let sse = [
                    r#"data: {"type":"response.created","response":{"id":"resp_edge_1"}}"#,
                    r#"data: {"type":"response.reasoning_summary_text.delta","delta":"sum-think"}"#,
                    r#"data: {"type":"response.output_text.done","text":"done-only-answer"}"#,
                    r#"data: {"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_edge_1","name":"lookup"}}"#,
                    r#"data: {"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{"q":"ok"}"}"#,
                    r#"data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":120,"output_tokens":30}}}"#,
                ]
                .join("\n\n")
                    + "\n\n";
                (
                    StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    sse,
                )
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let mock = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (gateway, auth_dir, _state, gateway_task) = start_gateway(&[("kiro-v3", &upstream)]).await;

    let client = reqwest::Client::new();

    // 1. Anthropic /v1/messages with document + redacted_thinking + adaptive thinking
    let resp1 = client
        .post(format!("{gateway}/v1/messages"))
        .bearer_auth(API_KEY)
        .header("anthropic-version", "2023-06-01")
        .json(&serde_json::json!({
            "model": MODEL,
            "max_tokens": 128,
            "stream": true,
            "thinking": {"type": "adaptive"},
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "read this pdf"},
                        {
                            "type": "document",
                            "title": "guide.pdf",
                            "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0x"}
                        }
                    ]
                },
                {
                    "role": "assistant",
                    "content": [
                        {"type": "redacted_thinking", "data": "opaque-blob"},
                        {"type": "text", "text": "checked"}
                    ]
                },
                {"role": "user", "content": "next"}
            ]
        }))
        .send()
        .await
        .unwrap();
    let status1 = resp1.status();
    let body1 = resp1.text().await.unwrap();
    assert_eq!(status1, StatusCode::OK, "anthropic body: {body1}");
    assert!(body1.contains("done-only-answer"), "done-only text lost: {body1}");
    assert!(body1.contains("sum-think"), "reasoning summary delta lost: {body1}");
    assert!(body1.contains("lookup"), "tool call lost: {body1}");

    // 2. OpenAI Responses /v1/responses with input_file + Kiro reasoning envelope + flat tool
    let resp2 = client
        .post(format!("{gateway}/v1/responses"))
        .bearer_auth(API_KEY)
        .json(&serde_json::json!({
            "model": MODEL,
            "stream": true,
            "reasoning": {"effort": "high", "summary": "concise"},
            "tools": [{"type": "function", "name": "lookup", "description": "find", "parameters": {"type": "object"}}],
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "check doc"},
                        {"type": "input_file", "filename": "report.pdf", "file_data": "data:application/pdf;base64,JVBERi0x"}
                    ]
                },
                {
                    "type": "reasoning",
                    "encrypted_content": r#"{"redactedContent":"resp-blob"}"#
                },
                {"type": "message", "role": "user", "content": "continue"}
            ]
        }))
        .send()
        .await
        .unwrap();
    let status2 = resp2.status();
    let body2 = resp2.text().await.unwrap();
    assert_eq!(status2, StatusCode::OK, "responses body: {body2}");
    assert!(body2.contains("done-only-answer"), "responses done-only text lost: {body2}");

    let reqs = captured.lock().unwrap();
    let posts: Vec<&CapturedReq> = reqs.iter().filter(|r| r.path == "/v1/responses").collect();
    assert_eq!(posts.len(), 2);

    // Verify /v1/messages translation
    let p1 = &posts[0].body;
    let user0_parts = p1["input"][0]["content"].as_array().expect("user0 multipart");
    assert!(user0_parts.iter().any(|part| {
        part["type"] == "input_file"
            && part["filename"] == "guide.pdf"
            && part["file_data"] == "data:application/pdf;base64,JVBERi0x"
    }));
    assert!(p1["input"].as_array().unwrap().iter().any(|item| {
        item["type"] == "reasoning"
            && item["encrypted_content"]
                .as_str()
                .is_some_and(|s| s.contains("opaque-blob"))
    }));
    assert_eq!(p1["reasoning"]["type"], "adaptive");
    assert!(p1["reasoning"].get("effort").is_none(), "invalid effort 'auto' must not be sent: {p1}");

    // Verify /v1/responses translation
    let p2 = &posts[1].body;
    let r_user0 = p2["input"][0]["content"].as_array().expect("responses user0 multipart");
    assert!(r_user0.iter().any(|part| {
        part["type"] == "input_file"
            && part["filename"] == "report.pdf"
            && part["file_data"] == "data:application/pdf;base64,JVBERi0x"
    }));
    assert!(p2["input"].as_array().unwrap().iter().any(|item| {
        item["type"] == "reasoning"
            && item["encrypted_content"]
                .as_str()
                .is_some_and(|s| s.contains("resp-blob"))
    }));
    assert_eq!(p2["reasoning"]["effort"], "high");
    assert_eq!(p2["reasoning"]["summary"], "concise");
    assert_eq!(p2["tools"][0]["name"], "lookup");

    gateway_task.abort();
    mock.abort();
    std::fs::remove_dir_all(auth_dir).ok();
}

#[tokio::test]
#[ignore]
async fn live_official_kiro_tar_zst_extracts_kas_version() {
    let client = reqwest::Client::new();
    let found = mahoquot_providers::fetch_kas_version_from_tar_zst_url(
        &client,
        "https://prod.download.cli.kiro.dev/stable/3.6.2/kirocli-aarch64-linux.tar.zst",
    )
    .await
    .unwrap();
    assert_eq!(found.as_deref(), Some("0.66.127-hotfix.1"));
}
