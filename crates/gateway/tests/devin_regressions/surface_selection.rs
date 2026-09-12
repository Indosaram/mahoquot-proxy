use super::*;
use axum::body::Body;
use axum::http::Request;
use mahoquot_gateway::url::build_provider_url;
use mahoquot_types::Health;
use tower::ServiceExt;

struct Fixture {
    state: Arc<AppState>,
    mock: DevinMockProcess,
    auth_dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.auth_dir).unwrap();
    }
}

impl Fixture {
    fn new(explicit_override: bool) -> Self {
        let mock = DevinMockProcess::start("default");
        let auth_dir = common::unique_temp_dir("devin-surface-selection");
        let mut credential = json!({
            "type": "devin", "identity_slug": "fixture", "access_token": "fixture-token",
            "api_server_url": mock.url
        });
        if explicit_override {
            credential["upstream_override"] = json!(mock.url);
        }
        std::fs::write(auth_dir.join("devin-fixture.json"), credential.to_string()).unwrap();
        let config = GatewayConfig {
            auth_dir: auth_dir.clone(),
            config_path: auth_dir.join("config.yaml"),
            api_keys: mahoquot_gateway::inbound::ApiKeys::from_env_value("test-inbound-key"),
            auth_refresh_enabled: false,
            ..GatewayConfig::default()
        };
        let state = Arc::new(AppState::new(&config).unwrap());
        register_devin_models(&state, &["devin/glm-5-2", "devin/swe-1-7"]);
        Self { state, mock, auth_dir }
    }

    async fn request(&self, path: &str, body: serde_json::Value) -> StatusCode {
        // Fail locally before any network I/O if the relay could send the fake token off-host.
        let pool = self.state.pool.load_full();
        let member = &pool.members[0];
        let chat_path = "/exa.api_server_pb.ApiServerService/GetChatMessage";
        assert_eq!(
            build_provider_url(member.kind(), member.upstream_override.as_deref(), chat_path),
            format!("{}{chat_path}", self.mock.url),
        );
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            create_app(Arc::clone(&self.state)).oneshot(
                Request::post(path)
                    .header(header::AUTHORIZATION, "Bearer test-inbound-key")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string())).unwrap(),
            ),
        ).await.unwrap().unwrap();
        let status = response.status();
        tokio::time::timeout(
            Duration::from_secs(5),
            axum::body::to_bytes(response.into_body(), 1024 * 1024),
        ).await.unwrap().unwrap();
        status
    }

    async fn assert_selected(&self) {
        let state = self.mock.get_state().await;
        let captures = state["captures"].as_array().unwrap();
        assert_eq!(captures.len(), 1);
        assert_eq!(captures[0]["sanitized_body"]["chat_model_uid"], "glm-5-2");
    }
}

fn messages_body() -> serde_json::Value {
    json!({"model":"devin/glm-5-2", "max_tokens":32, "stream":false,
        "messages":[{"role":"user", "content":"fixture"}]})
}

fn gemini_body() -> serde_json::Value {
    json!({"contents":[{"role":"user", "parts":[{"text":"fixture"}]}]})
}

#[tokio::test]
async fn messages_selects_discovered_account_when_only_native_endpoint_is_set() {
    // Given a healthy snapshot declaring the discovered public model IDs.
    let fixture = Fixture::new(false);
    // When Messages resolves and selects its account through the real route.
    let status = fixture.request("/v1/messages", messages_body()).await;
    // Then the selected account sends the canonical model's UID to the mock.
    assert_eq!(status, StatusCode::OK);
    fixture.assert_selected().await;
}

#[tokio::test]
async fn gemini_selects_discovered_account_when_only_native_endpoint_is_set() {
    // Given a healthy snapshot declaring the discovered public model IDs.
    let fixture = Fixture::new(false);
    // When Gemini resolves and selects its account through the real route.
    let status = fixture.request("/v1beta/models/devin/glm-5-2:generateContent", gemini_body()).await;
    // Then the selected account sends the canonical model's UID to the mock.
    assert_eq!(status, StatusCode::OK);
    fixture.assert_selected().await;
}

#[tokio::test]
async fn messages_selects_discovered_account_when_endpoint_is_explicit() {
    // Given endpoint resolution independent of the native-endpoint regression.
    let fixture = Fixture::new(true);
    // When the healthy account is selected through Messages.
    let status = fixture.request("/v1/messages", messages_body()).await;
    // Then the existing model normalization already selects the account.
    assert_eq!(status, StatusCode::OK);
    fixture.assert_selected().await;
}

#[tokio::test]
async fn gemini_selects_discovered_account_when_endpoint_is_explicit() {
    // Given endpoint resolution independent of the native-endpoint regression.
    let fixture = Fixture::new(true);
    // When the healthy account is selected through Gemini.
    let status = fixture.request("/v1beta/models/devin/glm-5-2:generateContent", gemini_body()).await;
    // Then the existing model normalization already selects the account.
    assert_eq!(status, StatusCode::OK);
    fixture.assert_selected().await;
}

#[tokio::test]
async fn messages_returns_unavailable_when_discovered_account_is_auth_failed() {
    // Given the health state produced by an upstream 401, despite a valid catalog.
    let fixture = Fixture::new(true);
    fixture.state.pool.load().members[0].set_health(Health::AuthFailed);
    // When Messages tries to select the account.
    let status = fixture.request("/v1/messages", messages_body()).await;
    // Then health, not model normalization, blocks selection before upstream I/O.
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(fixture.mock.get_state().await["captures"], json!([]));
}

#[tokio::test]
async fn gemini_returns_unavailable_when_discovered_account_is_auth_failed() {
    // Given the health state produced by an upstream 401, despite a valid catalog.
    let fixture = Fixture::new(true);
    fixture.state.pool.load().members[0].set_health(Health::AuthFailed);
    // When Gemini tries to select the account.
    let status = fixture.request("/v1beta/models/devin/glm-5-2:generateContent", gemini_body()).await;
    // Then health, not model normalization, blocks selection before upstream I/O.
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(fixture.mock.get_state().await["captures"], json!([]));
}
