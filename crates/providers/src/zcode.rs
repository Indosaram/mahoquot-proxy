//! GLM ZCode (Z.AI) accounts.
//!
//! Contract mirrored from gjc `packages/ai/src/utils/oauth/glm-zcode.ts`.
//!
//! UNOFFICIAL: the reference implementation states this is not an official Z.AI
//! OAuth client, that it may break at any time, and that it may violate the
//! ZCode/Z.AI terms of service. It is reproduced here to match that reference,
//! not because Z.AI publishes this flow.
//!
//! Two consequences shape the types below. The redirect target is the custom
//! scheme `zcode://oauth/callback`, which a server cannot receive, so the code
//! is pasted by the operator rather than captured by a listener. And the stored
//! `access_token` is not the OAuth token at all: the flow exchanges that token
//! for a long-lived provisioned API key of the form `{id}.{secret}`, which is
//! what inference actually sends.

use std::path::{Path, PathBuf};

use crate::account::LoadError;

pub const ZCODE_OAUTH_AUTHORIZE_URL: &str = "https://chat.z.ai/api/oauth/authorize";
pub const ZCODE_OAUTH_CLIENT_ID: &str = "client_P8X5CMWmlaRO9gyO-KSqtg";
pub const ZCODE_OAUTH_REDIRECT_URI: &str = "zcode://oauth/callback";
pub const ZCODE_OAUTH_BROKER_TOKEN_URL: &str = "https://zcode.z.ai/api/v1/oauth/token";
pub const ZCODE_LOGIN_URL: &str = "https://api.z.ai/api/auth/z/login";
pub const ZCODE_USERINFO_URL: &str = "https://chat.z.ai/api/oauth/userinfo";

pub const ZCODE_API_KEY_NAME: &str = "zcode-api-key";

pub const ZCODE_API_BASE: &str = "https://api.z.ai";

/// Inference speaks the Anthropic wire format under this prefix.
pub const ZCODE_ANTHROPIC_BASE: &str = "https://api.z.ai/api/anthropic";
pub const ZCODE_MESSAGES_PATH: &str = "/v1/messages";

pub const ZCODE_MODELS: &[&str] = &["glm-5.3", "glm-5.3-flash", "glm-5.2", "glm-5.1", "glm-4.6"];

use serde_json::Value;

pub fn is_zcode_model(model: &str) -> bool {
    ZCODE_MODELS.contains(&model)
}

pub fn zcode_messages_url(upstream_base: &str) -> String {
    format!(
        "{}{}",
        upstream_base.trim_end_matches('/'),
        ZCODE_MESSAGES_PATH
    )
}

/// Authorize URL for the ZCode desktop login flow. The redirect target is the
/// custom-scheme `zcode://oauth/callback`, so the operator pastes the final
/// redirect URL back instead of the gateway receiving it.
pub fn zcode_authorize_url(state: &str) -> String {
    format!(
        "{}?redirect_uri={}&response_type=code&client_id={}&state={}",
        ZCODE_OAUTH_AUTHORIZE_URL,
        form_encode(ZCODE_OAUTH_REDIRECT_URI),
        ZCODE_OAUTH_CLIENT_ID,
        form_encode(state)
    )
}

fn form_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Validate a pasted `zcode://oauth/callback?...` URL exactly as the reference
/// implementation does, and return the authorization code. The state must
/// match the session that generated the authorize URL.
pub fn extract_callback_code(callback_url: &str, expected_state: &str) -> Result<String, String> {
    let input = callback_url.trim();
    if input.is_empty() {
        return Err("GLM ZCode authorization callback URL is required".to_string());
    }
    let Some((scheme, rest)) = input.split_once("://") else {
        return Err("GLM ZCode requires the complete zcode:// callback URL".to_string());
    };
    if scheme != "zcode" {
        return Err("GLM ZCode callback URL is invalid".to_string());
    }
    let (authority, query) = rest
        .split_once('?')
        .ok_or_else(|| "GLM ZCode callback URL is invalid".to_string())?;
    let (host, path) = authority
        .split_once('/')
        .ok_or_else(|| "GLM ZCode callback URL is invalid".to_string())?;
    if host != "oauth" || path != "callback" {
        return Err("GLM ZCode callback URL is invalid".to_string());
    }

    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        match key {
            "code" if value.is_empty() => {
                return Err(
                    "GLM ZCode callback URL must contain exactly one non-empty code and state"
                        .to_string(),
                )
            }
            "code" => {
                if code.is_some() {
                    return Err(
                        "GLM ZCode callback URL must contain exactly one non-empty code and state"
                            .to_string(),
                    );
                }
                code = Some(value.to_string());
            }
            "state" if value.is_empty() => {
                return Err(
                    "GLM ZCode callback URL must contain exactly one non-empty code and state"
                        .to_string(),
                )
            }
            "state" => {
                if state.is_some() {
                    return Err(
                        "GLM ZCode callback URL must contain exactly one non-empty code and state"
                            .to_string(),
                    );
                }
                state = Some(value.to_string());
            }
            _ => {}
        }
    }
    let Some(code) = code else {
        return Err(
            "GLM ZCode callback URL must contain exactly one non-empty code and state".to_string(),
        );
    };
    let Some(state) = state else {
        return Err(
            "GLM ZCode callback URL must contain exactly one non-empty code and state".to_string(),
        );
    };
    if state != expected_state {
        return Err("GLM ZCode callback state did not match".to_string());
    }
    Ok(code)
}

/// `{"data": {...}}` envelopes unwrap to the inner object; bare bodies pass
/// through, matching the reference `data()` helper. Arrays are not unwrapped.
fn envelope(body: &Value) -> &Value {
    match body.get("data") {
        Some(inner) if inner.is_object() => inner,
        _ => body,
    }
}

fn required_str<'a>(body: &'a Value, keys: &[&str]) -> Result<&'a str, String> {
    let node = envelope(body);
    for key in keys {
        if let Some(value) = node.get(*key).and_then(Value::as_str) {
            if !value.trim().is_empty() {
                return Ok(value);
            }
        }
    }
    Err(format!("GLM ZCode response missing {}", keys.join(" or ")))
}

/// `data.zai.access_token` from the broker token exchange.
pub fn parse_broker_token(body: &Value) -> Result<String, String> {
    let zai = envelope(body)
        .get("zai")
        .ok_or_else(|| "GLM ZCode broker response missing data.zai.access_token".to_string())?;
    required_str(zai, &["access_token"]).map(str::to_string)
}

/// `data.access_token` from the z.ai business login.
pub fn parse_business_token(body: &Value) -> Result<String, String> {
    required_str(body, &["access_token"]).map(str::to_string)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZcodeCustomerInfo {
    pub organization_id: String,
    pub project_id: String,
    pub email: String,
    pub account_id: String,
}

/// Default organization/project plus identity from `getCustomerInfo`. The
/// reference picks `isDefault == true` when present and falls back to the
/// first entry of each list.
pub fn parse_customer_info(body: &Value) -> Result<ZcodeCustomerInfo, String> {
    let customer = envelope(body);
    let email = customer
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let account_id = match customer.get("id") {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Number(value)) => value.to_string(),
        _ => String::new(),
    };
    let organizations = customer
        .get("organizations")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            "GLM ZCode getCustomerInfo response missing default organization/project".to_string()
        })?;
    let organization = organizations
        .iter()
        .find(|entry| entry.get("isDefault").and_then(Value::as_bool) == Some(true))
        .or_else(|| organizations.first())
        .ok_or_else(|| {
            "GLM ZCode getCustomerInfo response missing default organization/project".to_string()
        })?;
    let projects = organization
        .get("projects")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            "GLM ZCode getCustomerInfo response missing default organization/project".to_string()
        })?;
    let project = projects
        .iter()
        .find(|entry| entry.get("isDefault").and_then(Value::as_bool) == Some(true))
        .or_else(|| projects.first())
        .ok_or_else(|| {
            "GLM ZCode getCustomerInfo response missing default organization/project".to_string()
        })?;
    let organization_id = organization
        .get("organizationId")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            "GLM ZCode getCustomerInfo response missing default organization/project".to_string()
        })?;
    let project_id = project
        .get("projectId")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            "GLM ZCode getCustomerInfo response missing default organization/project".to_string()
        })?;
    Ok(ZcodeCustomerInfo {
        organization_id: organization_id.to_string(),
        project_id: project_id.to_string(),
        email,
        account_id,
    })
}

/// The provisioned key named `zcode-api-key` from a list response, if present.
pub fn find_existing_api_key(body: &Value) -> Option<String> {
    body.get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|key| key.get("name").and_then(Value::as_str) == Some(ZCODE_API_KEY_NAME))
        .and_then(|key| {
            key.get("apiKey")
                .or_else(|| key.get("id"))
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
        })
}

/// `data.apiKey` from a key-create response.
pub fn parse_created_api_key(body: &Value) -> Result<String, String> {
    required_str(body, &["apiKey", "id"]).map(str::to_string)
}

/// `data.secretKey` from the key-copy endpoint; together with the key id this
/// forms the provisioned `{id}.{secret}` access token.
pub fn parse_copied_secret(body: &Value) -> Result<String, String> {
    required_str(body, &["secretKey"]).map(str::to_string)
}

/// A provisioned Z.AI key is `{id}.{secret}`; both halves must be non-empty for
/// the upstream to accept it.
pub fn is_provisioned_api_key(token: &str) -> bool {
    match token.split_once('.') {
        Some((id, secret)) => !id.is_empty() && !secret.is_empty(),
        None => false,
    }
}

/// Pick the provisioned `{id}.{secret}` GLM API key the ZCode desktop app
/// already saved for its coding plans (`config.json` → `provider` →
/// `builtin:zai-*` → `options.apiKey`). Prefers the coding-plan entry. Returns
/// None when the app has no usable key yet (never signed in on this Mac).
pub fn pick_desktop_api_key(config: &Value) -> Option<String> {
    let providers = config.get("provider")?.as_object()?;
    let mut candidates: Vec<(bool, String)> = providers
        .iter()
        .filter(|(name, _)| name.as_str().starts_with("builtin:zai-"))
        .filter_map(|(name, entry)| {
            let key = entry.get("options")?.get("apiKey")?.as_str()?;
            is_provisioned_api_key(key).then(|| (name.contains("coding"), key.to_string()))
        })
        .collect();
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates.into_iter().next().map(|(_, key)| key)
}

#[derive(Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct ZcodeAccount {
    #[serde(default)]
    pub identity_slug: String,
    /// The provisioned `{id}.{secret}` API key, not the OAuth access token.
    pub access_token: String,
    /// Upstream Z.AI OAuth token, used to re-provision the API key.
    pub refresh_token: String,
    pub email: String,
    pub expired: String,
    #[serde(default)]
    pub disabled: bool,
    #[serde(rename = "type")]
    pub r#type: String,
}

impl std::fmt::Debug for ZcodeAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZcodeAccount")
            .field("identity_slug", &self.identity_slug)
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("email", &self.email)
            .field("expired", &self.expired)
            .field("disabled", &self.disabled)
            .field("type", &self.r#type)
            .finish()
    }
}

pub fn list_zcode_auth_files(dir: &Path) -> Result<Vec<PathBuf>, LoadError> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("zcode-") && n.ends_with(".json"))
        })
        .collect();
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_takes_zcode_files_and_leaves_other_providers_alone() {
        let dir = std::env::temp_dir().join(format!("qp-zcode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        for name in [
            "zcode-a.json",
            "zcode-b.json",
            "kiro-a.json",
            "codex-a.json",
            "zcode.json",
        ] {
            std::fs::write(dir.join(name), "{}").expect("write");
        }

        let found = list_zcode_auth_files(&dir).expect("listing");
        let names: Vec<String> = found
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(String::from))
            .collect();

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(names, vec!["zcode-a.json", "zcode-b.json"]);
    }

    #[test]
    fn credential_parses_and_redacts_tokens_in_debug() {
        let account: ZcodeAccount = serde_json::from_str(
            r#"{"access_token":"keyid.keysecret","refresh_token":"secret-refresh",
                "email":"u@example.com","expired":"2027-01-01T00:00:00Z","type":"zcode"}"#,
        )
        .expect("deserialize");

        assert_eq!(account.r#type, "zcode");

        let rendered = format!("{account:?}");
        assert!(!rendered.contains("keyid.keysecret"));
        assert!(!rendered.contains("secret-refresh"));
        assert!(rendered.contains("[REDACTED]"));
    }

    #[test]
    fn messages_url_uses_anthropic_prefix_and_override() {
        assert_eq!(
            zcode_messages_url(ZCODE_ANTHROPIC_BASE),
            "https://api.z.ai/api/anthropic/v1/messages"
        );
        assert_eq!(
            zcode_messages_url("http://127.0.0.1:18893/"),
            "http://127.0.0.1:18893/v1/messages"
        );
    }

    #[test]
    fn provisioned_key_requires_both_halves() {
        assert!(is_provisioned_api_key("abc.def"));
        assert!(!is_provisioned_api_key("abcdef"));
        assert!(!is_provisioned_api_key(".def"));
        assert!(!is_provisioned_api_key("abc."));
    }

    #[test]
    fn authorize_url_carries_client_redirect_and_state() {
        let url = zcode_authorize_url("st-1");
        assert!(url.starts_with("https://chat.z.ai/api/oauth/authorize?"));
        assert!(url.contains("redirect_uri=zcode%3A%2F%2Foauth%2Fcallback"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=client_P8X5CMWmlaRO9gyO-KSqtg"));
        assert!(url.contains("state=st-1"));
    }

    #[test]
    fn callback_code_accepts_the_canonical_redirect() {
        assert_eq!(
            extract_callback_code("zcode://oauth/callback?code=zc_1&state=st-1", "st-1").unwrap(),
            "zc_1"
        );
    }

    #[test]
    fn callback_code_rejects_malformed_redirects() {
        let invalid = [
            "",
            "  ",
            "https://oauth/callback?code=c&state=st-1",
            "zcode://oauth/callback",
            "zcode://other/callback?code=c&state=st-1",
            "zcode://oauth/other?code=c&state=st-1",
            "zcode://oauth/callback?state=st-1",
            "zcode://oauth/callback?code=&state=st-1",
            "zcode://oauth/callback?code=c1&code=c2&state=st-1",
            "zcode://oauth/callback?code=c&state=st-1&state=st-1",
            "zcode://oauth/callback?code=c",
        ];
        for url in invalid {
            assert!(
                extract_callback_code(url, "st-1").is_err(),
                "accepted {url}"
            );
        }
        assert_eq!(
            extract_callback_code("zcode://oauth/callback?code=c&state=other", "st-1").unwrap_err(),
            "GLM ZCode callback state did not match"
        );
    }

    #[test]
    fn broker_and_business_tokens_unwrap_the_data_envelope() {
        assert_eq!(
            parse_broker_token(&serde_json::json!({
                "data": { "zai": { "access_token": "up" } }
            }))
            .unwrap(),
            "up"
        );
        assert!(parse_broker_token(&serde_json::json!({ "data": {} })).is_err());
        assert_eq!(
            parse_business_token(&serde_json::json!({ "data": { "access_token": "biz" } }))
                .unwrap(),
            "biz"
        );
    }

    #[test]
    fn customer_info_prefers_default_org_and_project() {
        let info = parse_customer_info(&serde_json::json!({
            "data": {
                "email": "User@Example.com",
                "id": 42,
                "organizations": [
                    {"organizationId": "org-a", "projects": []},
                    {"organizationId": "org-b", "isDefault": true, "projects": [
                        {"projectId": "proj-x"},
                        {"projectId": "proj-y", "isDefault": true}
                    ]}
                ]
            }
        }))
        .unwrap();
        assert_eq!(info.organization_id, "org-b");
        assert_eq!(info.project_id, "proj-y");
        assert_eq!(info.email, "user@example.com");
        assert_eq!(info.account_id, "42");
    }

    #[test]
    fn api_key_lookup_finds_the_named_key_and_ignores_others() {
        let listed = serde_json::json!({
            "data": [
                {"name": "other", "apiKey": "k0"},
                {"name": "zcode-api-key", "id": "k1"}
            ]
        });
        assert_eq!(find_existing_api_key(&listed).as_deref(), Some("k1"));
        assert_eq!(
            find_existing_api_key(&serde_json::json!({"data": []})),
            None
        );
        assert_eq!(
            parse_created_api_key(&serde_json::json!({"data": {"apiKey": "k2"}})).unwrap(),
            "k2"
        );
        assert_eq!(
            parse_copied_secret(&serde_json::json!({"data": {"secretKey": "s"}})).unwrap(),
            "s"
        );
    }

    #[test]
    fn model_matcher_accepts_known_models_only() {
        assert!(is_zcode_model("glm-5.2"));
        assert!(is_zcode_model("glm-5.3-flash"));
        assert!(!is_zcode_model("claude-sonnet-4-5-20250929"));
    }
}
