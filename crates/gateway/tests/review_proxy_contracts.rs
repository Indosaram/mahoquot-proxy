mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mahoquot_gateway::compat::events::{CodexEvent, SseParser};
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::state::AppState;
use serde_json::json;
use tower::ServiceExt;

#[test]
fn relay_identity_uses_endpoint_hosts_not_account_names_or_url_paths() {
    // Given: an official account whose user-chosen name contains relay text.
    let mut member = mahoquot_gateway::account::AccountMember::for_test_with_id(
        "nekos-fan-ccapi",
        mahoquot_gateway::account::ProviderAccount::Claude(
            mahoquot_providers::ClaudeAccount::default(),
        ),
    );
    member.upstream_override = Some("https://api.anthropic.com/v1?label=ccapi".to_string());

    // When: provider identity is used to decide prefix eligibility.
    let classified_as_relay = member.is_nekos_relay();

    // Then: account labels and arbitrary URL text cannot change its provider.
    assert!(!classified_as_relay);
    assert!(member.supports_model("anthropic-claude-3-7-sonnet-20250219"));
    assert!(!member.supports_model("nekos-claude-3-7-sonnet-20250219"));
    member.usage_override = Some("https://claude.nekos.me".to_string());
    assert!(member.is_nekos_relay());
}

#[tokio::test]
async fn anthropic_oauth_reports_unavailable_callback_listener() {
    // Given: the fixed redirect port is occupied before authorization starts.
    let _occupied = tokio::net::TcpListener::bind("127.0.0.1:54545")
        .await
        .unwrap();
    let auth_dir = common::unique_temp_dir("proxy-oauth-bind");
    let state = Arc::new(
        AppState::new(&GatewayConfig {
            auth_dir: auth_dir.clone(),
            config_path: auth_dir.join("config.yaml"),
            auth_refresh_enabled: false,
            ..GatewayConfig::default()
        })
        .unwrap(),
    );

    // When: the management router attempts to start the default callback flow.
    let response = mahoquot_gateway::management::oauth::oauth_routes()
        .with_state(state)
        .oneshot(
            Request::get("/anthropic-auth-url")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Then: clients must not receive a successful URL for an unusable listener.
    assert_eq!(response.status(), StatusCode::CONFLICT);
    std::fs::remove_dir_all(auth_dir).unwrap();
}

#[test]
fn runtime_candidates_obey_exclusions_and_canonical_unsupported_models() {
    // Given: one Claude account with a rejected canonical model.
    let auth_dir = common::unique_temp_dir("proxy-runtime-candidates");
    std::fs::write(
        auth_dir.join("claude-fixture.json"),
        json!({
            "type": "claude", "identity_slug": "official",
            "access_token": "fixture", "expired": "2099-01-01T00:00:00Z",
            "upstream_override": "http://127.0.0.1:18899"
        })
        .to_string(),
    )
    .unwrap();
    let state = AppState::new(&GatewayConfig {
        auth_dir: auth_dir.clone(),
        config_path: auth_dir.join("config.yaml"),
        auth_refresh_enabled: false,
        ..GatewayConfig::default()
    })
    .unwrap();
    let mut pool = (*state.pool.load_full()).clone();
    assert_eq!(pool.members.len(), 1);
    let canonical = "claude-3-7-sonnet-20250219";
    pool.members[0].mark_model_unsupported(canonical);

    // When: the runtime projects candidates through a provider-prefixed ID.
    let candidates = pool.routable_accounts_for_model(&format!("anthropic-{canonical}"));

    // Then: the prefix cannot hide the canonical rejection.
    assert!(candidates.is_empty());

    pool.members[0].unsupported_models.write().unwrap().clear();
    let mut registry = (*pool.registry).clone();
    registry.exclusions.insert(mahoquot_registry::ModelExclusionRule {
        model_id: mahoquot_registry::ModelId::new(canonical).unwrap(),
        provider_id: None,
    });
    pool.registry = Arc::new(registry);
    assert!(pool.routable_accounts_for_model(canonical).is_empty());
    std::fs::remove_dir_all(auth_dir).unwrap();
}

#[test]
fn sse_decodes_optional_spaces_multiline_data_and_all_line_endings() {
    // Given: equivalent SSE events, including a split UTF-8 character.
    for frame in [
        "data:{\"type\":\"response.output_text.delta\",\"delta\":\"한\"}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\ndata: \"delta\":\"한\"}\n\n",
        "data:{\"type\":\"response.output_text.delta\",\"delta\":\"한\"}\r\n\r\n",
        "data:{\"type\":\"response.output_text.delta\",\"delta\":\"한\"}\r\r",
    ] {
        for split in 0..=frame.len() {
            let mut parser = SseParser::default();
            let mut events = Vec::new();

            // When: the wire stream is split at any byte boundary.
            parser.push(&frame.as_bytes()[..split], &mut events);
            parser.push(&frame.as_bytes()[split..], &mut events);
            parser.finish(&mut events);

            // Then: framing differences do not discard or duplicate the delta.
            assert_eq!(events, vec![CodexEvent::TextDelta("한".to_string())]);
        }
    }
}

#[tokio::test]
async fn count_tokens_rejects_models_without_a_loaded_capable_binding() {
    // Given: an empty account pool, including no official or relay Claude account.
    let auth_dir = common::unique_temp_dir("proxy-count-contract");
    let state = Arc::new(
        AppState::new(&GatewayConfig {
            auth_dir: auth_dir.clone(),
            config_path: auth_dir.join("config.yaml"),
            auth_refresh_enabled: false,
            ..GatewayConfig::default()
        })
        .unwrap(),
    );
    let app = create_app(state);
    for model in ["gpt-5.6-sol", "anthropic-nonexistent", "nekos-nonexistent"] {
        // When: the real route receives an unsupported model.
        let response = app
            .clone()
            .oneshot(
                Request::post("/v1/messages/count_tokens")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model": model, "messages": []}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Then: model spelling alone cannot grant token-count capability.
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{model}");
    }
    std::fs::remove_dir_all(auth_dir).unwrap();
}
