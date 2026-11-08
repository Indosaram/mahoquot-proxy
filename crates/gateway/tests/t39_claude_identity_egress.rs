mod common;

use std::collections::BTreeMap;

use mahoquot_gateway::account::{
    is_valid_claude_device_id, load_account_members,
    CLAUDE_DEVICE_IDENTITY_STORE,
};
use mahoquot_gateway::config::GatewayConfig;
use mahoquot_gateway::management::settings::ProviderProxyPolicy;
use mahoquot_gateway::state::AppState;
use serde_json::json;

#[test]
fn claude_device_id_persists_across_loads_in_separate_store() {
    let auth_dir = common::unique_temp_dir("claude-id-persist");
    let cred = json!({
        "type": "claude",
        "identity_slug": "claude-persist-user",
        "access_token": "token-1",
        "refresh_token": "refresh-1",
        "account_id": "user_01persist",
        "email": "user@example.com",
        "expired": "2099-01-01T00:00:00Z"
    });
    std::fs::write(auth_dir.join("claude-1.json"), cred.to_string()).unwrap();

    let members1 = load_account_members(&auth_dir).unwrap();
    assert_eq!(members1.len(), 1);
    let dev_id1 = members1[0].claude_device_id().to_string();
    assert_eq!(dev_id1.len(), 64);
    assert!(is_valid_claude_device_id(&dev_id1));

    let store_path = auth_dir.join(CLAUDE_DEVICE_IDENTITY_STORE);
    assert!(store_path.exists());

    let cred_content = std::fs::read_to_string(auth_dir.join("claude-1.json")).unwrap();
    let cred_json: serde_json::Value = serde_json::from_str(&cred_content).unwrap();
    assert!(cred_json.get("device_id").is_none());

    let members2 = load_account_members(&auth_dir).unwrap();
    assert_eq!(members2.len(), 1);
    assert_eq!(members2[0].claude_device_id(), dev_id1);

    std::fs::remove_dir_all(auth_dir).ok();
}

#[test]
fn claude_device_id_preserves_explicit_credential_field() {
    let auth_dir = common::unique_temp_dir("claude-id-explicit");
    let expected_id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let cred = json!({
        "type": "claude",
        "identity_slug": "claude-explicit-user",
        "access_token": "token-2",
        "refresh_token": "refresh-2",
        "account_id": "user_01explicit",
        "device_id": expected_id,
        "email": "user@example.com",
        "expired": "2099-01-01T00:00:00Z"
    });
    std::fs::write(auth_dir.join("claude-explicit.json"), cred.to_string()).unwrap();

    let members = load_account_members(&auth_dir).unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].claude_device_id(), expected_id);

    std::fs::remove_dir_all(auth_dir).ok();
}

#[test]
fn claude_device_id_token_reload_does_not_clobber() {
    let auth_dir = common::unique_temp_dir("claude-id-reload");
    let cred = json!({
        "type": "claude",
        "identity_slug": "claude-reload-user",
        "access_token": "token-old",
        "refresh_token": "refresh-old",
        "account_id": "user_01reload",
        "email": "user@example.com",
        "expired": "2099-01-01T00:00:00Z"
    });
    std::fs::write(auth_dir.join("claude-reload.json"), cred.to_string()).unwrap();

    let member = load_account_members(&auth_dir).unwrap().remove(0);
    let original_id = member.claude_device_id().to_string();
    assert!(is_valid_claude_device_id(&original_id));

    let mutated = json!({
        "type": "claude",
        "identity_slug": "claude-reload-user",
        "access_token": "token-new",
        "refresh_token": "refresh-new",
        "account_id": "user_01reload",
        "email": "user@example.com",
        "expired": "2099-01-02T00:00:00Z"
    });
    std::fs::write(auth_dir.join("claude-reload.json"), mutated.to_string()).unwrap();

    member.reload_from_file().unwrap();
    assert_eq!(member.access_token(), "token-new");
    assert_eq!(member.claude_device_id(), original_id);

    std::fs::remove_dir_all(auth_dir).ok();
}

#[test]
fn claude_egress_direct_absent_proxy_preserves_injection() {
    let auth_dir = common::unique_temp_dir("claude-egress-direct");
    let cred = json!({
        "type": "claude",
        "identity_slug": "claude-direct-user",
        "access_token": "token-d",
        "refresh_token": "refresh-d",
        "account_id": "user_01direct",
        "email": "user@example.com",
        "expired": "2099-01-01T00:00:00Z"
    });
    std::fs::write(auth_dir.join("claude-direct.json"), cred.to_string()).unwrap();

    let config = GatewayConfig {
        auth_dir: auth_dir.clone(),
        config_path: auth_dir.join("config.yaml"),
        auth_refresh_enabled: false,
        ..GatewayConfig::default()
    };
    let mut state = AppState::new(&config).unwrap();
    let injected_direct = reqwest::Client::builder().build().unwrap();
    state.claude_direct_client = injected_direct;

    let member = state.find_member("claude-direct-user").unwrap();
    let selected = state.client_for_member(&member);
    assert_eq!(state.claude_device_id(&member), member.claude_device_id());
    assert!(is_valid_claude_device_id(state.claude_device_id(&member)));
    assert_eq!(
        state.claude_device_id_for_account("claude-direct-user").as_deref(),
        Some(member.claude_device_id())
    );
    drop(selected);

    std::fs::remove_dir_all(auth_dir).ok();
}

#[tokio::test]
async fn claude_egress_respects_provider_proxy_and_compression() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let proxy = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let read = stream.read(&mut buf).await.unwrap();
            assert!(read > 0);
            request.extend_from_slice(&buf[..read]);
        }
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await.unwrap();
        String::from_utf8(request).unwrap()
    });
    let auth_dir = common::unique_temp_dir("claude-egress-provider");
    let cred = json!({
        "type": "claude",
        "identity_slug": "claude-provider-user",
        "access_token": "token-p",
        "refresh_token": "refresh-p",
        "account_id": "user_01provider",
        "email": "user@example.com",
        "expired": "2099-01-01T00:00:00Z"
    });
    std::fs::write(auth_dir.join("claude-p.json"), cred.to_string()).unwrap();

    let mut providers = BTreeMap::new();
    providers.insert(
        "claude".to_string(),
        ProviderProxyPolicy {
            enabled: true,
            sticky: true,
            ttl_secs: 600,
            url: format!("http://{proxy_addr}"),
        },
    );
    let mut settings = mahoquot_gateway::management::settings::Settings::default();
    settings.proxy_providers = providers;
    std::fs::write(auth_dir.join("config.yaml"), serde_yaml::to_string(&settings).unwrap()).unwrap();

    let config = GatewayConfig {
        auth_dir: auth_dir.clone(),
        config_path: auth_dir.join("config.yaml"),
        auth_refresh_enabled: false,
        ..GatewayConfig::default()
    };
    let state = AppState::new(&config).unwrap();
    let member = state.find_member("claude-provider-user").unwrap();
    assert!(!state.claude_egress_invalid(&member));

    let client = state.client_for_member(&member);
    assert_eq!(state.proxy_client_count(), 1);
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), client.get("http://claude-egress.invalid/probe").send()).await.unwrap().unwrap();
    assert_eq!(response.text().await.unwrap(), "ok");
    let request = tokio::time::timeout(std::time::Duration::from_secs(5), proxy).await.unwrap().unwrap();
    assert!(request.starts_with("GET http://claude-egress.invalid/probe HTTP/1.1"));
    let lower = request.to_ascii_lowercase();
    assert!(lower.contains("accept-encoding:"));
    for encoding in ["gzip", "deflate", "br", "zstd"] {
        assert!(lower.contains(encoding));
    }

    std::fs::remove_dir_all(auth_dir).ok();
}

#[test]
fn claude_egress_respects_global_proxy() {
    let auth_dir = common::unique_temp_dir("claude-egress-global");
    let cred = json!({
        "type": "claude",
        "identity_slug": "claude-global-user",
        "access_token": "token-g",
        "refresh_token": "refresh-g",
        "account_id": "user_01global",
        "email": "user@example.com",
        "expired": "2099-01-01T00:00:00Z"
    });
    std::fs::write(auth_dir.join("claude-g.json"), cred.to_string()).unwrap();

    let mut settings = mahoquot_gateway::management::settings::Settings::default();
    settings.proxy_url = "http://127.0.0.1:3128".to_string();
    std::fs::write(auth_dir.join("config.yaml"), serde_yaml::to_string(&settings).unwrap()).unwrap();

    let config = GatewayConfig {
        auth_dir: auth_dir.clone(),
        config_path: auth_dir.join("config.yaml"),
        auth_refresh_enabled: false,
        ..GatewayConfig::default()
    };
    let state = AppState::new(&config).unwrap();
    let member = state.find_member("claude-global-user").unwrap();
    assert!(!state.claude_egress_invalid(&member));

    let client = state.client_for_member(&member);
    assert_eq!(state.proxy_client_count(), 1);
    drop(client);

    std::fs::remove_dir_all(auth_dir).ok();
}

#[test]
fn claude_egress_invalid_refuses_silent_unproxied_fallback() {
    let auth_dir = common::unique_temp_dir("claude-egress-invalid");
    let cred = json!({
        "type": "claude",
        "identity_slug": "claude-bad-user",
        "access_token": "token-b",
        "refresh_token": "refresh-b",
        "account_id": "user_01bad",
        "email": "user@example.com",
        "expired": "2099-01-01T00:00:00Z"
    });
    std::fs::write(auth_dir.join("claude-b.json"), cred.to_string()).unwrap();

    let mut settings = mahoquot_gateway::management::settings::Settings::default();
    settings.claude_account_egress.insert(
        "claude-bad-user".to_string(),
        "not-a-valid-proxy-url@@:".to_string(),
    );
    std::fs::write(auth_dir.join("config.yaml"), serde_yaml::to_string(&settings).unwrap()).unwrap();

    let config = GatewayConfig {
        auth_dir: auth_dir.clone(),
        config_path: auth_dir.join("config.yaml"),
        auth_refresh_enabled: false,
        ..GatewayConfig::default()
    };
    let state = AppState::new(&config).unwrap();
    let member = state.find_member("claude-bad-user").unwrap();
    assert!(state.claude_egress_invalid(&member));

    let client = state.client_for_member(&member);
    drop(client);

    std::fs::remove_dir_all(auth_dir).ok();
}
