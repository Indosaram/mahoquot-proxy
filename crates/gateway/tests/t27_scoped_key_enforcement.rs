//! Scoped inbound keys, end to end: minting them through the control plane,
//! the three-way scope they impose on relayed traffic (providers, accounts,
//! models), the token budget that retires them, and the accounting that keeps
//! that budget honest across a restart.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use http_body_util::BodyExt;
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::inbound::ApiKeys;
use mahoquot_gateway::management::settings::ScopedApiKey;
use mahoquot_gateway::request_history::stable_key_identifier;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use serde_json::{json, Value};
use tower::ServiceExt;

use common::{create_auth_file_json, unique_temp_dir};

const MASTER: &str = "t27-master-key";

/// The fixture upstream reports this much usage per buffered reply, so the
/// budget assertions can name an exact number rather than a threshold.
const FIXTURE_TOTAL_TOKENS: u64 = 18;

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

impl Fixture {
    async fn send(&self, request: Request<Body>) -> Response {
        self.app
            .clone()
            .oneshot(request)
            .await
            .expect("gateway response")
    }

    async fn get(&self, uri: &str, key: &str) -> Response {
        self.send(request("GET", uri, key, Body::empty())).await
    }

    /// Relay one non-streaming completion. Buffered replies are recorded before
    /// the response is handed back, so usage is settled when this returns.
    async fn relay(&self, key: &str, model: &str) -> Response {
        let payload = json!({
            "model": model,
            "stream": false,
            "messages": [{"role": "user", "content": "hi"}],
        });
        self.send(request(
            "POST",
            "/v1/chat/completions",
            key,
            Body::from(payload.to_string()),
        ))
        .await
    }

    fn persisted(&self, id: &str) -> ScopedApiKey {
        self.state
            .settings
            .current()
            .scoped_api_keys
            .iter()
            .find(|key| key.id == id)
            .cloned()
            .expect("scoped key present in settings")
    }
}

fn request(method: &str, uri: &str, key: &str, body: Body) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(body)
        .expect("request")
}

async fn json_body(response: Response) -> Value {
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

/// A codex-shaped upstream that always answers with a completed SSE turn
/// carrying usage. Bound on an ephemeral port so tests never collide.
async fn spawn_upstream() -> String {
    const SSE: &str = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_t27\"}}\n\n",
        "event: response.output_item.added\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_t27\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"content_index\":0,\"delta\":\"ok\",\"item_id\":\"msg_t27\",\"output_index\":0}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_t27\",\"status\":\"completed\",\"usage\":{\"input_tokens\":11,\"output_tokens\":7,\"total_tokens\":18}}}\n\n",
    );
    let app = Router::new().route(
        "/backend-api/codex/responses",
        post(|| async move {
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from(SSE))
                .expect("upstream response")
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind t27 upstream");
    let address = listener.local_addr().expect("upstream address");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve t27 upstream");
    });
    format!("http://{address}")
}

/// A gateway with one codex account pointed at `upstream`, guarded by the
/// master key. Scoped keys are minted per test through the control plane.
async fn fixture(label: &str) -> Fixture {
    let upstream = spawn_upstream().await;
    let dir = unique_temp_dir(&format!("qg-t27-{label}"));
    std::fs::write(
        dir.join("codex-t27.json"),
        create_auth_file_json(
            "t27-account",
            "account-27",
            "upstream-token",
            Some(&upstream),
        ),
    )
    .expect("credential fixture");
    let config_path = dir.join("config.yaml");
    std::fs::write(&config_path, "logging-to-file: false\n").expect("config fixture");
    let config = GatewayConfig {
        auth_dir: dir.clone(),
        config_path,
        api_keys: ApiKeys::new(vec![MASTER.to_string()]),
        auth_refresh_enabled: false,
        ..GatewayConfig::default()
    };
    let state = Arc::new(AppState::new(&config).expect("gateway state"));
    let app = create_app(Arc::clone(&state));
    Fixture { state, app, dir }
}

/// Mint a scoped key through the control plane and return `(raw key, id)`.
async fn mint(fixture: &Fixture, body: Value) -> (String, String) {
    let response = fixture
        .send(request(
            "POST",
            "/v0/management/scoped-keys",
            MASTER,
            Body::from(body.to_string()),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let payload = json_body(response).await;
    let raw = payload["api_key"]
        .as_str()
        .expect("minted key material")
        .to_string();
    let id = payload["key"]["id"].as_str().expect("key id").to_string();
    (raw, id)
}

/// The id of the single loaded account, which is what `allowed_accounts` names.
fn only_account_id(fixture: &Fixture) -> String {
    let pool = fixture.state.pool.load();
    assert_eq!(pool.members.len(), 1, "fixture pools exactly one account");
    pool.members[0].id.clone()
}

#[tokio::test]
async fn minting_returns_the_key_once_and_persists_only_its_identifier() {
    // given a gateway with no scoped keys
    let fixture = fixture("mint").await;

    // when the operator mints one
    let (raw, id) = mint(
        &fixture,
        json!({
            "name": "study buddy",
            "allowed_providers": ["codex"],
            "allowed_models": ["gpt-5.6-sol"],
            "token_limit": 500,
        }),
    )
    .await;

    // then the raw key is returned exactly once, under the documented prefix
    assert!(raw.starts_with("mq-sh-"), "unexpected key shape: {raw}");

    // and only its one-way identifier is written to the settings document
    let stored = fixture.persisted(&id);
    assert_eq!(stored.key_identifier, stable_key_identifier(&raw));
    assert_eq!(stored.token_limit, 500);
    assert_eq!(stored.token_used, 0);
    assert!(stored.is_active);
    assert!(stored.key_prefix.starts_with("mq-sh-"));
    let raw_on_disk = std::fs::read_to_string(fixture.state.settings.path()).expect("config file");
    assert!(
        !raw_on_disk.contains(&raw),
        "raw key material must never reach disk"
    );

    // and listing reports it without handing the secret back
    let listed = json_body(fixture.get("/v0/management/scoped-keys", MASTER).await).await;
    let rows = listed["scoped_keys"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], id);
    assert_eq!(rows[0]["token_used"], 0);
    assert_eq!(rows[0]["is_exhausted"], false);
    assert!(rows[0].get("api_key").is_none());
    assert!(
        !rows[0].to_string().contains(&raw),
        "no listed field may echo the raw key: {}",
        rows[0]
    );
}

#[tokio::test]
async fn patch_edits_scope_and_budget_while_delete_revokes_the_key() {
    // given a minted key
    let fixture = fixture("crud").await;
    let (raw, id) = mint(
        &fixture,
        json!({ "name": "before", "allowed_models": ["gpt-5.6-sol"], "token_limit": 10 }),
    )
    .await;

    // when it is edited
    let patched = fixture
        .send(request(
            "PATCH",
            &format!("/v0/management/scoped-keys/{id}"),
            MASTER,
            Body::from(
                json!({
                    "name": "after",
                    "token_limit": 4_000,
                    "allowed_providers": ["codex"],
                    "allowed_accounts": ["acct-x"],
                    "allowed_models": ["*"],
                })
                .to_string(),
            ),
        ))
        .await;

    // then every named field moves and the rest is untouched
    assert_eq!(patched.status(), StatusCode::OK);
    let view = json_body(patched).await;
    assert_eq!(view["key"]["name"], "after");
    assert_eq!(view["key"]["token_limit"], 4_000);
    assert_eq!(view["key"]["allowed_providers"], json!(["codex"]));
    assert_eq!(view["key"]["allowed_accounts"], json!(["acct-x"]));
    assert_eq!(view["key"]["allowed_models"], json!(["*"]));
    assert_eq!(view["key"]["is_active"], true);
    let stored = fixture.persisted(&id);
    assert_eq!(stored.name, "after");
    assert_eq!(stored.token_limit, 4_000);

    // and deactivating through the same route stops it authenticating
    let deactivated = fixture
        .send(request(
            "PATCH",
            &format!("/v0/management/scoped-keys/{id}"),
            MASTER,
            Body::from(json!({ "is_active": false }).to_string()),
        ))
        .await;
    assert_eq!(deactivated.status(), StatusCode::OK);
    assert_eq!(
        fixture.get("/v1/models", &raw).await.status(),
        StatusCode::UNAUTHORIZED
    );

    // and deleting it removes it from the document for good
    let deleted = fixture
        .send(request(
            "DELETE",
            &format!("/v0/management/scoped-keys/{id}"),
            MASTER,
            Body::empty(),
        ))
        .await;
    assert_eq!(deleted.status(), StatusCode::OK);
    assert!(fixture.state.settings.current().scoped_api_keys.is_empty());
    assert_eq!(
        fixture.get("/v1/models", &raw).await.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn editing_an_unknown_scoped_key_is_a_not_found() {
    // given a gateway with no scoped keys
    let fixture = fixture("missing").await;

    // when an unknown id is edited or deleted
    let patched = fixture
        .send(request(
            "PATCH",
            "/v0/management/scoped-keys/shk_nope",
            MASTER,
            Body::from(json!({ "name": "ghost" }).to_string()),
        ))
        .await;
    let deleted = fixture
        .send(request(
            "DELETE",
            "/v0/management/scoped-keys/shk_nope",
            MASTER,
            Body::empty(),
        ))
        .await;

    // then neither invents a key
    assert_eq!(patched.status(), StatusCode::NOT_FOUND);
    assert_eq!(deleted.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_scoped_key_is_refused_on_the_control_plane_but_served_on_the_relay() {
    // given a scoped key with no restrictions
    let fixture = fixture("control-plane").await;
    let (raw, _) = mint(&fixture, json!({ "name": "delegate" })).await;

    // when it reaches for management and admin surfaces
    for path in [
        "/v0/management/scoped-keys",
        "/v0/management/api-keys",
        "/admin/stats",
    ] {
        // then it is refused with 403 rather than served
        assert_eq!(
            fixture.get(path, &raw).await.status(),
            StatusCode::FORBIDDEN,
            "path: {path}"
        );
    }

    // and the relay surface still accepts it
    assert_eq!(
        fixture.get("/v1/models", &raw).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_model_outside_the_allow_list_is_forbidden_before_any_upstream_call() {
    // given a key restricted to one model
    let fixture = fixture("model-scope").await;
    let (raw, _) = mint(
        &fixture,
        json!({ "name": "single-model", "allowed_models": ["gpt-5.6-sol"] }),
    )
    .await;

    // when it requests a different model
    let response = fixture.relay(&raw, "gpt-5.6-luna").await;

    // then it is forbidden, naming the model it asked for
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let payload = json_body(response).await;
    assert_eq!(
        payload["error"]["message"],
        "Model 'gpt-5.6-luna' is not allowed for this API key"
    );

    // and the model it is allowed still routes
    assert_eq!(
        fixture.relay(&raw, "gpt-5.6-sol").await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_wildcard_model_list_admits_every_model() {
    // given a key whose model list is an explicit wildcard
    let fixture = fixture("model-wildcard").await;
    let (raw, _) = mint(
        &fixture,
        json!({ "name": "wildcard", "allowed_models": ["*"] }),
    )
    .await;

    // when it requests an arbitrary catalogue model
    let response = fixture.relay(&raw, "gpt-5.6-luna").await;

    // then the wildcard admits it
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_provider_outside_the_allow_list_leaves_no_eligible_account() {
    // given a key scoped to a provider that is not in the pool
    let fixture = fixture("provider-scope").await;
    let (raw, _) = mint(
        &fixture,
        json!({ "name": "claude-only", "allowed_providers": ["claude"] }),
    )
    .await;

    // when it relays a model the pool could otherwise serve
    let response = fixture.relay(&raw, "gpt-5.6-sol").await;

    // then it is forbidden rather than routed to the codex account
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let payload = json_body(response).await;
    assert_eq!(
        payload["error"]["message"],
        "no permitted accounts/providers available for this API key"
    );
}

#[tokio::test]
async fn an_account_outside_the_allow_list_leaves_no_eligible_account() {
    // given a key pinned to an account that is not loaded
    let fixture = fixture("account-scope").await;
    let (stranger, _) = mint(
        &fixture,
        json!({ "name": "stranger", "allowed_accounts": ["some-other-account"] }),
    )
    .await;
    // and one pinned to the account that is
    let (resident, _) = mint(
        &fixture,
        json!({ "name": "resident", "allowed_accounts": [only_account_id(&fixture)] }),
    )
    .await;

    // when both relay the same model
    let refused = fixture.relay(&stranger, "gpt-5.6-sol").await;
    let served = fixture.relay(&resident, "gpt-5.6-sol").await;

    // then only the pinned-to-a-real-account key is routed
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(served.status(), StatusCode::OK);
}

#[tokio::test]
async fn the_models_catalogue_is_narrowed_to_what_the_key_may_route_to() {
    // given the unrestricted catalogue
    let fixture = fixture("models-catalogue").await;
    let full = json_body(fixture.get("/v1/models", MASTER).await).await;
    let full_ids: Vec<String> = full["data"]
        .as_array()
        .expect("catalogue rows")
        .iter()
        .map(|row| row["id"].as_str().expect("model id").to_string())
        .collect();
    assert!(full_ids.len() > 1, "fixture must publish several models");

    // when a key restricted to one model asks
    let (narrow, _) = mint(
        &fixture,
        json!({ "name": "narrow", "allowed_models": ["gpt-5.6-sol"] }),
    )
    .await;
    let narrowed = json_body(fixture.get("/v1/models", &narrow).await).await;

    // then it only sees that model
    assert_eq!(narrowed["data"].as_array().expect("rows").len(), 1);
    assert_eq!(narrowed["data"][0]["id"], "gpt-5.6-sol");

    // and a key scoped to a provider that owns nothing in the pool sees nothing
    let (foreign, _) = mint(
        &fixture,
        json!({ "name": "foreign", "allowed_providers": ["claude"] }),
    )
    .await;
    let empty = json_body(fixture.get("/v1/models", &foreign).await).await;
    assert!(empty["data"].as_array().expect("rows").is_empty());

    // and a key scoped to the loaded account keeps the full catalogue
    let (resident, _) = mint(
        &fixture,
        json!({ "name": "resident", "allowed_accounts": [only_account_id(&fixture)] }),
    )
    .await;
    let resident_view = json_body(fixture.get("/v1/models", &resident).await).await;
    let resident_ids: Vec<String> = resident_view["data"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["id"].as_str().expect("model id").to_string())
        .collect();
    assert_eq!(resident_ids, full_ids);
}

#[tokio::test]
async fn relayed_usage_is_charged_to_the_key_and_persisted() {
    // given a key with room for exactly one fixture reply
    let fixture = fixture("accounting").await;
    let (raw, id) = mint(
        &fixture,
        json!({ "name": "budgeted", "token_limit": FIXTURE_TOTAL_TOKENS }),
    )
    .await;
    let identifier = stable_key_identifier(&raw);

    // when it relays one buffered request
    assert_eq!(
        fixture.relay(&raw, "gpt-5.6-sol").await.status(),
        StatusCode::OK
    );

    // then the live counter carries the reply's tokens
    let entry = fixture
        .state
        .scoped_keys
        .get(&identifier)
        .expect("tracked key");
    assert_eq!(entry.token_used(), FIXTURE_TOTAL_TOKENS);
    assert!(entry.is_exhausted());

    // and the settings document carries them too, so a restart resumes spent
    assert_eq!(fixture.persisted(&id).token_used, FIXTURE_TOTAL_TOKENS);

    // and the control plane reports the same figure
    let listed = json_body(fixture.get("/v0/management/scoped-keys", MASTER).await).await;
    assert_eq!(listed["scoped_keys"][0]["token_used"], FIXTURE_TOTAL_TOKENS);
    assert_eq!(listed["scoped_keys"][0]["is_exhausted"], true);
}

#[tokio::test]
async fn an_exhausted_budget_answers_429_and_a_raised_limit_revives_the_key() {
    // given a key whose budget is spent
    let fixture = fixture("budget").await;
    let (raw, id) = mint(
        &fixture,
        json!({ "name": "spent", "token_limit": FIXTURE_TOTAL_TOKENS }),
    )
    .await;
    assert_eq!(
        fixture.relay(&raw, "gpt-5.6-sol").await.status(),
        StatusCode::OK
    );

    // when it relays again
    let response = fixture.relay(&raw, "gpt-5.6-sol").await;

    // then it is throttled rather than routed
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let payload = json_body(response).await;
    assert!(
        payload["error"]["message"]
            .as_str()
            .expect("error message")
            .starts_with("Token quota exceeded for this API key"),
        "unexpected message: {payload}"
    );

    // and raising the limit through the control plane puts it back to work
    let patched = fixture
        .send(request(
            "PATCH",
            &format!("/v0/management/scoped-keys/{id}"),
            MASTER,
            Body::from(json!({ "token_limit": 10_000 }).to_string()),
        ))
        .await;
    assert_eq!(patched.status(), StatusCode::OK);
    assert_eq!(
        fixture.relay(&raw, "gpt-5.6-sol").await.status(),
        StatusCode::OK
    );
    // and the earlier spend is still counted against the new limit
    assert_eq!(
        fixture.persisted(&id).token_used,
        FIXTURE_TOTAL_TOKENS * 2,
        "raising a limit must not forgive spent tokens"
    );
}

#[tokio::test]
async fn a_zero_limit_means_unlimited() {
    // given a key minted without a budget
    let fixture = fixture("unlimited").await;
    let (raw, id) = mint(&fixture, json!({ "name": "unlimited" })).await;

    // when it relays repeatedly
    for _ in 0..3 {
        assert_eq!(
            fixture.relay(&raw, "gpt-5.6-sol").await.status(),
            StatusCode::OK
        );
    }

    // then usage still accrues but never trips a limit
    assert_eq!(fixture.persisted(&id).token_used, FIXTURE_TOTAL_TOKENS * 3);
    let listed = json_body(fixture.get("/v0/management/scoped-keys", MASTER).await).await;
    assert_eq!(listed["scoped_keys"][0]["is_exhausted"], false);
}

#[tokio::test]
async fn a_master_key_is_neither_scoped_nor_charged() {
    // given a gateway with one scoped key present
    let fixture = fixture("master").await;
    let (_, id) = mint(
        &fixture,
        json!({ "name": "bystander", "token_limit": 1, "allowed_models": ["nothing"] }),
    )
    .await;

    // when the master key relays a model the scoped key could not
    assert_eq!(
        fixture.relay(MASTER, "gpt-5.6-luna").await.status(),
        StatusCode::OK
    );

    // then the scoped key's budget is untouched
    assert_eq!(fixture.persisted(&id).token_used, 0);
}
