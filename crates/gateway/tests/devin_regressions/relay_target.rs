use mahoquot_gateway::account::load_account_members;
use mahoquot_gateway::url::build_provider_url;
use serde_json::json;

const CHAT_PATH: &str = "/exa.api_server_pb.ApiServerService/GetChatMessage";

#[test]
fn relay_uses_api_server_url_when_explicit_override_is_absent() {
    // Given a Devin credential with only its native endpoint field.
    let dir = super::common::unique_temp_dir("devin-native-endpoint");
    std::fs::write(
        dir.join("devin-fixture.json"),
        json!({
            "type": "devin", "identity_slug": "fixture", "access_token": "fixture-token",
            "api_server_url": "http://127.0.0.1:54321"
        }).to_string(),
    ).unwrap();

    // When the loader feeds the actual relay URL builder.
    let members = load_account_members(&dir).unwrap();
    let member = &members[0];
    let target = build_provider_url(member.kind(), member.upstream_override.as_deref(), CHAT_PATH);
    std::fs::remove_dir_all(dir).unwrap();

    // Then it targets the fixture, never the default API server (no HTTP is sent).
    assert_eq!(target, format!("http://127.0.0.1:54321{CHAT_PATH}"));
}

#[test]
fn relay_uses_explicit_override_when_native_endpoint_differs() {
    // Given distinct native and explicit endpoints so precedence is observable.
    let dir = super::common::unique_temp_dir("devin-explicit-endpoint");
    std::fs::write(
        dir.join("devin-fixture.json"),
        json!({
            "type": "devin", "identity_slug": "fixture", "access_token": "fixture-token",
            "api_server_url": "http://127.0.0.1:54321",
            "upstream_override": "http://127.0.0.1:54322"
        }).to_string(),
    ).unwrap();

    // When the loader feeds the actual relay URL builder.
    let members = load_account_members(&dir).unwrap();
    let member = &members[0];
    let target = build_provider_url(member.kind(), member.upstream_override.as_deref(), CHAT_PATH);
    std::fs::remove_dir_all(dir).unwrap();

    // Then the explicit override wins.
    assert_eq!(target, format!("http://127.0.0.1:54322{CHAT_PATH}"));
}
