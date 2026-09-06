mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::routing::post;
use axum::Router;
use common::{create_auth_file_json, unique_temp_dir};
use http_body_util::BodyExt;
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::management::settings::{ScopedApiKey, Settings};
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use tower::ServiceExt;

const SCOPED_RAW: &str = "unconfigured-scoped-raw";
const MASTER: &str = "master-key";

fn scoped_key(is_active: bool) -> ScopedApiKey {
    ScopedApiKey {
        id: "sk-open-1".to_string(),
        name: "delegated".to_string(),
        key_identifier: mahoquot_gateway::request_history::stable_key_identifier(SCOPED_RAW),
        key_prefix: "unconfig".to_string(),
        raw_key: Some(SCOPED_RAW.to_string()),
        allowed_providers: Vec::new(),
        allowed_accounts: Vec::new(),
        allowed_models: Vec::new(),
        token_limit: 1_000,
        token_used: 0,
        is_active,
        created_at_ms: 0,
        expires_at_ms: None,
    }
}

fn state_with(masters: Vec<String>, scoped: Vec<ScopedApiKey>) -> Arc<AppState> {
    let dir = unique_temp_dir("qg-review-auth-recovery");
    let config_path = dir.join("config.yaml");
    let settings = Settings {
        api_keys: masters,
        scoped_api_keys: scoped,
        auth_dir: dir.to_string_lossy().to_string(),
        ..Settings::default()
    };
    settings.persist(&config_path).expect("persist config");
    let config = GatewayConfig {
        auth_dir: dir.clone(),
        config_path,
        ..GatewayConfig::default()
    };
    Arc::new(AppState::new(&config).expect("state"))
}

async fn status_for(state: Arc<AppState>, path: &str, key: Option<&str>) -> StatusCode {
    let mut request = Request::builder().uri(path);
    if let Some(key) = key {
        request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    create_app(state)
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("response")
        .status()
}

/// An unconfigured gateway (no master key) still has to tell a minted scoped
/// key apart from an anonymous local caller, or delegating quota would hand the
/// holder the whole control plane.
#[tokio::test]
async fn a_minted_scoped_key_stays_scoped_without_a_master_key() {
    // given a gateway with no master key and one usable scoped key
    let state = state_with(Vec::new(), vec![scoped_key(true)]);

    // when the scoped secret is presented
    let relay = status_for(Arc::clone(&state), "/v1/models", Some(SCOPED_RAW)).await;
    let management =
        status_for(Arc::clone(&state), "/v0/management/api-keys", Some(SCOPED_RAW)).await;

    // then it relays but never reaches management
    assert_eq!(relay, StatusCode::OK);
    assert_eq!(management, StatusCode::FORBIDDEN);
}

/// The same open gateway must stay open for the local console, which calls it
/// without any credential at all.
#[tokio::test]
async fn an_unconfigured_gateway_stays_open_for_callers_that_are_not_scoped() {
    // given the same gateway
    let state = state_with(Vec::new(), vec![scoped_key(true)]);

    // when no credential, an unknown credential, or an unusable scoped key is presented
    let anonymous = status_for(Arc::clone(&state), "/v0/management/api-keys", None).await;
    let unknown = status_for(
        Arc::clone(&state),
        "/v0/management/api-keys",
        Some("not-a-minted-key"),
    )
    .await;
    let disabled = state_with(Vec::new(), vec![scoped_key(false)]);
    let inactive = status_for(disabled, "/v0/management/api-keys", Some(SCOPED_RAW)).await;

    // then each keeps full authority instead of being locked out
    assert_eq!(anonymous, StatusCode::OK);
    assert_eq!(unknown, StatusCode::OK);
    assert_eq!(inactive, StatusCode::OK);
}

/// Configuring a master key closes the gateway again: the scoped key keeps its
/// relay access and an unknown credential is refused outright.
#[tokio::test]
async fn a_configured_master_key_closes_the_open_path() {
    // given a gateway with a master key alongside the scoped key
    let state = state_with(vec![MASTER.to_string()], vec![scoped_key(true)]);

    // when the three credential shapes are presented
    let unknown = status_for(
        Arc::clone(&state),
        "/v0/management/api-keys",
        Some("not-a-minted-key"),
    )
    .await;
    let scoped =
        status_for(Arc::clone(&state), "/v0/management/api-keys", Some(SCOPED_RAW)).await;
    let master = status_for(Arc::clone(&state), "/v0/management/api-keys", Some(MASTER)).await;

    // then only the master key manages, and the unknown key is rejected
    assert_eq!(unknown, StatusCode::UNAUTHORIZED);
    assert_eq!(scoped, StatusCode::FORBIDDEN);
    assert_eq!(master, StatusCode::OK);
}

async fn bind_fixture_listener() -> tokio::net::TcpListener {
    for port in 18840..=18899 {
        match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => return listener,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => panic!("fixture bind failed on {port}: {error}"),
        }
    }
    panic!("no available fixture port in 18840-18899");
}

/// A 401 replay storm presents the same already-rotated token from several
/// requests. Only the first may spend a refresh; the rest have to observe the
/// rotation and return without touching the token endpoint again.
#[tokio::test]
async fn a_replayed_stale_token_does_not_spend_a_second_refresh() {
    // given a token endpoint that counts exchanges and rotates the access token
    let exchanges = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&exchanges);
    let listener = bind_fixture_listener().await;
    let port = listener.local_addr().expect("addr").port();
    let token_app = Router::new().route(
        "/oauth/token",
        post(move || {
            let counter = Arc::clone(&counter);
            async move {
                let nth = counter.fetch_add(1, Ordering::SeqCst) + 1;
                axum::Json(serde_json::json!({
                    "access_token": format!("rotated_{nth}"),
                    "refresh_token": "fake_rt",
                    "expires_in": 3600,
                }))
            }
        }),
    );
    let (shutdown, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, token_app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
            .expect("token fixture");
    });

    let dir = unique_temp_dir("qg-review-stale-refresh");
    std::fs::write(
        dir.join("codex-stale.json"),
        create_auth_file_json("stale", "acc_stale", "token_original", None),
    )
    .expect("credential");
    let config_path = dir.join("config.yaml");
    std::fs::write(&config_path, "logging-to-file: false\n").expect("config");
    let state = Arc::new(
        AppState::new(&GatewayConfig {
            auth_dir: dir,
            config_path,
            refresh_url: format!("http://127.0.0.1:{port}/oauth/token"),
            auth_refresh_enabled: true,
            ..GatewayConfig::default()
        })
        .expect("state"),
    );
    let member = state
        .pool
        .load()
        .members
        .first()
        .cloned()
        .expect("one account");
    assert_eq!(member.access_token(), "token_original");

    // when the first caller refreshes and a second caller replays the same stale token
    let first = state
        .refresh_member(&member, Some("token_original"))
        .await
        .expect("first refresh");
    let rotated = member.access_token();
    let replay = state
        .refresh_member(&member, Some("token_original"))
        .await
        .expect("stale replay");

    // then only the first exchanged a token
    assert!(first, "the first caller refreshes");
    assert!(!replay, "a stale presented token must not refresh again");
    assert_eq!(rotated, "rotated_1");
    assert_eq!(member.access_token(), "rotated_1");
    assert_eq!(exchanges.load(Ordering::SeqCst), 1);

    // and a caller presenting the current token still refreshes
    let current = state
        .refresh_member(&member, Some(&rotated))
        .await
        .expect("current refresh");
    assert!(current);
    assert_eq!(member.access_token(), "rotated_2");
    assert_eq!(exchanges.load(Ordering::SeqCst), 2);

    let _ = shutdown.send(());
    let _ = server.await;
}

async fn patch_scoped_key(state: Arc<AppState>, id: &str, payload: serde_json::Value) -> Response {
    let response = create_app(state)
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/v0/management/scoped-keys/{id}"))
                .header(header::AUTHORIZATION, format!("Bearer {MASTER}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(payload.to_string()))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    Response {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    }
}

struct Response {
    status: StatusCode,
    body: serde_json::Value,
}

/// Rotating a scoped key is destructive: a blank secret must be treated as
/// "leave it alone" rather than silently re-deriving the identity from an empty
/// string and revoking the holder.
#[tokio::test]
async fn a_blank_replacement_secret_leaves_the_scoped_identity_intact() {
    // given a live scoped key on a gateway with a master key
    let state = state_with(vec![MASTER.to_string()], vec![scoped_key(true)]);
    let identifier = mahoquot_gateway::request_history::stable_key_identifier(SCOPED_RAW);

    // when the key is patched with a whitespace-only secret
    let patched = patch_scoped_key(
        Arc::clone(&state),
        "sk-open-1",
        serde_json::json!({ "name": "renamed", "raw_key": "   " }),
    )
    .await;

    // then the rename lands but the identity and the working secret do not move
    assert_eq!(patched.status, StatusCode::OK);
    assert_eq!(patched.body["key"]["name"], "renamed");
    assert_eq!(patched.body["key"]["key_identifier"], identifier);
    assert_eq!(patched.body["key"]["key_prefix"], "unconfig");
    assert_eq!(
        status_for(Arc::clone(&state), "/v1/models", Some(SCOPED_RAW)).await,
        StatusCode::OK
    );
}

/// The same endpoint with a real secret must rotate: the old secret stops
/// working and the new one carries the spent allowance forward.
#[tokio::test]
async fn a_real_replacement_secret_rotates_the_key_and_keeps_usage() {
    // given a scoped key that has already spent part of its allowance
    let state = state_with(vec![MASTER.to_string()], vec![scoped_key(true)]);
    let identifier = mahoquot_gateway::request_history::stable_key_identifier(SCOPED_RAW);
    state.scoped_keys.record_usage(Some(&identifier), 250);

    // when a new secret is issued for it
    let patched = patch_scoped_key(
        Arc::clone(&state),
        "sk-open-1",
        serde_json::json!({ "raw_key": "rotated-scoped-raw" }),
    )
    .await;

    // then the old secret is refused, the new one works, and usage survives
    assert_eq!(patched.status, StatusCode::OK);
    assert_eq!(patched.body["key"]["token_used"], 250);
    assert_eq!(
        status_for(Arc::clone(&state), "/v1/models", Some(SCOPED_RAW)).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status_for(Arc::clone(&state), "/v1/models", Some("rotated-scoped-raw")).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn patching_an_unknown_scoped_key_reports_not_found() {
    // given a gateway with one scoped key
    let state = state_with(vec![MASTER.to_string()], vec![scoped_key(true)]);

    // when a different id is patched
    let patched = patch_scoped_key(
        Arc::clone(&state),
        "sk-does-not-exist",
        serde_json::json!({ "name": "ghost" }),
    )
    .await;

    // then the request is refused and the existing key is untouched
    assert_eq!(patched.status, StatusCode::NOT_FOUND);
    assert_eq!(state.settings.current().scoped_api_keys.len(), 1);
    assert_eq!(state.settings.current().scoped_api_keys[0].name, "delegated");
}
