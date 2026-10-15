mod common;

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use http_body_util::BodyExt;
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::inbound::ApiKeys;
use mahoquot_gateway::management::settings::Settings;
use mahoquot_gateway::management::store::SettingsStore;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use serde_json::{json, Value};
use tower::ServiceExt;

const ADMIN_KEY: &str = "omo-profile-admin";
const URI: &str = "/v0/management/omo-profile";

struct Fixture {
    app: Router,
    dir: PathBuf,
    config_path: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture(label: &str) -> Fixture {
    let dir = common::unique_temp_dir(&format!("omo-profile-{label}"));
    let auth_dir = dir.join("auth");
    std::fs::create_dir_all(&auth_dir).expect("auth dir");
    let config_path = dir.join("config.yaml");
    std::fs::write(
        &config_path,
        "logging-to-file: false\ntls:\n  enable: true\nomo-profile-unmodelled-probe: kept\n",
    )
    .expect("config fixture");
    let config = GatewayConfig {
        auth_dir,
        config_path: config_path.clone(),
        api_keys: ApiKeys::new(vec![ADMIN_KEY.to_string()]),
        auth_refresh_enabled: false,
        ..GatewayConfig::default()
    };
    let state = Arc::new(AppState::new(&config).expect("gateway state"));
    Fixture {
        app: create_app(state),
        dir,
        config_path,
    }
}

impl Fixture {
    async fn send(&self, request: Request<Body>) -> Response {
        self.app
            .clone()
            .oneshot(request)
            .await
            .expect("gateway response")
    }

    async fn get(&self, key: Option<&str>, if_none_match: Option<&str>) -> Response {
        let mut builder = Request::builder().method("GET").uri(URI);
        if let Some(key) = key {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {key}"));
        }
        if let Some(value) = if_none_match {
            builder = builder.header(header::IF_NONE_MATCH, value);
        }
        self.send(builder.body(Body::empty()).expect("request")).await
    }

    async fn put(&self, key: Option<&str>, if_match: Option<&str>, body: Value) -> Response {
        let mut builder = Request::builder()
            .method("PUT")
            .uri(URI)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(key) = key {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {key}"));
        }
        if let Some(value) = if_match {
            builder = builder.header(header::IF_MATCH, value);
        }
        self.send(builder.body(Body::from(body.to_string())).expect("request"))
            .await
    }
}

fn etag(response: &Response) -> String {
    response
        .headers()
        .get(header::ETAG)
        .expect("etag header")
        .to_str()
        .expect("etag text")
        .to_string()
}

async fn json_body(response: Response) -> Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("response body")
        .to_bytes();
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("invalid json {error}: {}", String::from_utf8_lossy(&bytes)))
}

fn profile() -> Value {
    json!({
        "compat": {"sendSessionAffinityHeaders": true},
        "models": [
            {"id": "glm-5.3-flash", "contextWindow": 200000},
            {"id": "gemini-3.8-flash"},
        ],
        "remove": ["retired-model"],
    })
}

#[tokio::test]
async fn an_unset_profile_reads_as_revision_zero() {
    // given a gateway that never stored a profile
    let fixture = fixture("unset");
    // when the profile is read
    let response = fixture.get(Some(ADMIN_KEY), None).await;
    // then it is revision 0 with empty collections and a matching ETag
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(etag(&response), "\"r0\"");
    let body = json_body(response).await;
    assert_eq!(body["revision"], 0);
    assert_eq!(body["updated_at"], 0);
    assert_eq!(body["compat"], json!({}));
    assert_eq!(body["models"], json!([]));
    assert_eq!(body["remove"], json!([]));
}

#[tokio::test]
async fn put_requires_if_match() {
    // given a gateway with no profile
    let fixture = fixture("no-if-match");
    // when a PUT omits If-Match
    let response = fixture.put(Some(ADMIN_KEY), None, profile()).await;
    // then it is refused as a missing precondition
    assert_eq!(response.status(), StatusCode::PRECONDITION_REQUIRED);
}

#[tokio::test]
async fn put_stores_the_profile_and_get_honours_if_none_match() {
    // given a gateway with no profile
    let fixture = fixture("put-get");
    // when a PUT carries the current ETag
    let response = fixture
        .put(Some(ADMIN_KEY), Some("\"r0\""), profile())
        .await;
    // then revision 1 is returned with its ETag
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(etag(&response), "\"r1\"");
    let body = json_body(response).await;
    assert_eq!(body["revision"], 1);
    assert!(body["updated_at"].as_u64().unwrap() > 0);
    assert_eq!(body["models"][0]["contextWindow"], 200000);
    // and the config file holds the profile next to the unrelated key
    let on_disk = std::fs::read_to_string(&fixture.config_path).expect("config.yaml");
    assert!(on_disk.contains("omo-profile:"), "{on_disk}");
    assert!(on_disk.contains("tls:"), "{on_disk}");
    // and GET returns the stored document
    let read = json_body(fixture.get(Some(ADMIN_KEY), None).await).await;
    assert_eq!(read["revision"], 1);
    assert_eq!(read["compat"]["sendSessionAffinityHeaders"], true);
    assert_eq!(read["remove"], json!(["retired-model"]));
    // and a matching If-None-Match gets an empty 304
    let cached = fixture.get(Some(ADMIN_KEY), Some("\"r1\"")).await;
    assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(etag(&cached), "\"r1\"");
    let bytes = cached.into_body().collect().await.expect("body").to_bytes();
    assert!(bytes.is_empty());
    // while a stale If-None-Match still returns the body
    let stale = fixture.get(Some(ADMIN_KEY), Some("\"r0\"")).await;
    assert_eq!(stale.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_stale_if_match_is_refused_with_the_current_profile() {
    // given a stored revision 1
    let fixture = fixture("stale");
    let first = fixture
        .put(Some(ADMIN_KEY), Some("\"r0\""), profile())
        .await;
    assert_eq!(first.status(), StatusCode::OK);
    // when a second writer still holds the r0 ETag
    let response = fixture
        .put(
            Some(ADMIN_KEY),
            Some("\"r0\""),
            json!({"compat": {}, "models": [{"id": "other"}], "remove": []}),
        )
        .await;
    // then it gets 412 carrying the current body and ETag
    assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    assert_eq!(etag(&response), "\"r1\"");
    let body = json_body(response).await;
    assert_eq!(body["revision"], 1);
    assert_eq!(body["models"][0]["id"], "glm-5.3-flash");
    // and the stored profile was not replaced
    let read = json_body(fixture.get(Some(ADMIN_KEY), None).await).await;
    assert_eq!(read["models"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn invalid_profiles_are_rejected_with_400() {
    let fixture = fixture("invalid");
    let cases = [
        (
            "duplicate id",
            json!({"compat": {}, "models": [{"id": "a"}, {"id": "a"}], "remove": []}),
        ),
        (
            "id in both lists",
            json!({"compat": {}, "models": [{"id": "a"}], "remove": ["a"]}),
        ),
        (
            "missing id",
            json!({"compat": {}, "models": [{"name": "a"}], "remove": []}),
        ),
        (
            "empty id",
            json!({"compat": {}, "models": [{"id": ""}], "remove": []}),
        ),
        (
            "non-string id",
            json!({"compat": {}, "models": [{"id": 7}], "remove": []}),
        ),
        (
            "empty remove entry",
            json!({"compat": {}, "models": [], "remove": [""]}),
        ),
        (
            "duplicate remove entry",
            json!({"compat": {}, "models": [], "remove": ["a", "a"]}),
        ),
        (
            "unknown field",
            json!({"compat": {}, "models": [], "remove": [], "extra": 1}),
        ),
    ];
    for (name, body) in cases {
        // when an invalid body arrives with a correct If-Match
        let response = fixture.put(Some(ADMIN_KEY), Some("\"r0\""), body).await;
        // then it is a 400 with an error message
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{name}");
        assert!(json_body(response).await["error"].is_string(), "{name}");
    }
    // and nothing was stored
    let read = json_body(fixture.get(Some(ADMIN_KEY), None).await).await;
    assert_eq!(read["revision"], 0);
}

#[tokio::test]
async fn an_oversized_body_is_refused_by_the_route_limit() {
    // given a body just over 1 MiB
    let fixture = fixture("oversized");
    let big = "x".repeat(1024 * 1024 + 1);
    // when it is PUT
    let response = fixture
        .put(
            Some(ADMIN_KEY),
            Some("\"r0\""),
            json!({"compat": {"blob": big}, "models": [], "remove": []}),
        )
        .await;
    // then it is refused as too large and nothing is stored
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let read = json_body(fixture.get(Some(ADMIN_KEY), None).await).await;
    assert_eq!(read["revision"], 0);
}

#[tokio::test]
async fn a_missing_key_is_401_and_a_scoped_key_is_403() {
    // given a gateway and a scoped key minted through the control plane
    let fixture = fixture("auth");
    let minted = fixture
        .send(
            Request::builder()
                .method("POST")
                .uri("/v0/management/scoped-keys")
                .header(header::AUTHORIZATION, format!("Bearer {ADMIN_KEY}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({"name": "omo-profile-scoped"}).to_string()))
                .expect("request"),
        )
        .await;
    assert_eq!(minted.status(), StatusCode::CREATED);
    let scoped = json_body(minted).await["api_key"]
        .as_str()
        .expect("minted key material")
        .to_string();
    // when the profile is requested without a key
    // then it is unauthorized
    assert_eq!(fixture.get(None, None).await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        fixture.put(None, Some("\"r0\""), profile()).await.status(),
        StatusCode::UNAUTHORIZED
    );
    // and a scoped key may neither read nor write it
    assert_eq!(
        fixture.get(Some(&scoped), None).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        fixture
            .put(Some(&scoped), Some("\"r0\""), profile())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn the_profile_and_unrelated_keys_survive_a_reload_from_disk() {
    // given a stored profile
    let fixture = fixture("reload");
    let response = fixture
        .put(Some(ADMIN_KEY), Some("\"r0\""), profile())
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    // when a fresh store loads the same file
    let reloaded = SettingsStore::load_or(fixture.config_path.clone(), Settings::default())
        .expect("reloads");
    // then the profile is intact
    let stored = reloaded.current().omo_profile.clone().expect("profile persisted");
    assert_eq!(stored.revision, 1);
    assert_eq!(stored.models.len(), 2);
    assert_eq!(stored.remove, vec!["retired-model".to_string()]);
    // and the key this build does not model is still there
    assert!(reloaded
        .current()
        .extra
        .contains_key("omo-profile-unmodelled-probe"));
    // and the modelled passthrough key is still carried through
    assert!(reloaded.current().tls.extra.contains_key("enable"));
}
