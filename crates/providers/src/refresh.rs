pub const REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const REFRESH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const CLAUDE_REFRESH_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const CLAUDE_REFRESH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const CLINE_REFRESH_URL: &str = "https://api.cline.bot/api/v1/auth/refresh";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshRequest {
    pub url: String,
    pub form_fields: Vec<(String, String)>,
    pub json_body: Option<serde_json::Value>,
    pub headers: Vec<(String, String)>,
}

impl RefreshRequest {
    pub fn refresh_token(&self) -> Option<&str> {
        if let Some(body) = &self.json_body {
            if let Some(token) = body.get("refresh_token").and_then(|v| v.as_str()) {
                return Some(token);
            }
            if let Some(token) = body.get("refreshToken").and_then(|v| v.as_str()) {
                return Some(token);
            }
        }
        for (k, v) in &self.form_fields {
            if k == "refresh_token" || k == "refreshToken" {
                return Some(v.as_str());
            }
        }
        None
    }
}

#[derive(Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("id_token", &self.id_token.as_ref().map(|_| "[REDACTED]"))
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

pub fn build_refresh_request(refresh_token: &str) -> RefreshRequest {
    RefreshRequest {
        url: REFRESH_TOKEN_URL.to_string(),
        form_fields: vec![
            ("client_id".to_string(), REFRESH_CLIENT_ID.to_string()),
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), refresh_token.to_string()),
            ("scope".to_string(), "openid profile email".to_string()),
        ],
        json_body: None,
        headers: Vec::new(),
    }
}

pub fn build_antigravity_refresh_request(refresh_token: &str) -> RefreshRequest {
    RefreshRequest {
        url: crate::antigravity::ANTIGRAVITY_TOKEN_URL.to_string(),
        form_fields: vec![
            (
                "client_id".to_string(),
                crate::antigravity::antigravity_client_id().to_string(),
            ),
            (
                "client_secret".to_string(),
                crate::antigravity::antigravity_client_secret().to_string(),
            ),
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), refresh_token.to_string()),
        ],
        json_body: None,
        headers: Vec::new(),
    }
}

pub fn build_claude_refresh_request(refresh_token: &str) -> RefreshRequest {
    RefreshRequest {
        url: CLAUDE_REFRESH_TOKEN_URL.to_string(),
        form_fields: Vec::new(),
        json_body: Some(serde_json::json!({
            "client_id": CLAUDE_REFRESH_CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "scope": "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
        })),
        headers: vec![
            ("accept".to_string(), "application/json, text/plain, */*".to_string()),
            ("user-agent".to_string(), "axios/1.15.2".to_string()),
        ],
    }
}

pub fn build_claude_refresh_request_with_scope(
    refresh_token: &str,
    scope: Option<&str>,
) -> RefreshRequest {
    let mut body = serde_json::json!({
        "client_id": CLAUDE_REFRESH_CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
    });
    if let Some(s) = scope {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("scope".to_string(), serde_json::Value::String(s.to_string()));
        }
    }
    RefreshRequest {
        url: CLAUDE_REFRESH_TOKEN_URL.to_string(),
        form_fields: Vec::new(),
        json_body: Some(body),
        headers: vec![
            ("accept".to_string(), "application/json, text/plain, */*".to_string()),
            ("user-agent".to_string(), "axios/1.15.2".to_string()),
        ],
    }
}

pub fn build_cursor_refresh_request(refresh_token: &str) -> RefreshRequest {
    RefreshRequest {
        url: crate::cursor::CURSOR_REFRESH_URL.to_string(),
        form_fields: Vec::new(),
        json_body: Some(serde_json::json!({})),
        headers: vec![(
            "authorization".to_string(),
            format!("Bearer {refresh_token}"),
        )],
    }
}

pub fn build_kiro_social_refresh_request(refresh_token: &str, region: &str) -> RefreshRequest {
    RefreshRequest {
        url: crate::kiro::kiro_refresh_url(crate::kiro::KiroAuthMode::Social, region),
        form_fields: Vec::new(),
        json_body: Some(serde_json::json!({"refreshToken": refresh_token})),
        headers: vec![(
            "user-agent".to_string(),
            "KiroIDE-0.7.45-mahoquot".to_string(),
        )],
    }
}

pub fn build_kiro_idc_refresh_request(
    refresh_token: &str,
    region: &str,
    client_id: &str,
    client_secret: &str,
) -> RefreshRequest {
    RefreshRequest {
        url: crate::kiro::kiro_refresh_url(crate::kiro::KiroAuthMode::Idc, region),
        form_fields: Vec::new(),
        json_body: Some(serde_json::json!({
            "grantType": "refresh_token",
            "clientId": client_id,
            "clientSecret": client_secret,
            "refreshToken": refresh_token,
        })),
        headers: Vec::new(),
    }
}

/// Cline CLI login (WorkOS OAuth) refresh. The CLI stores
/// `providers.json` auth as `{accessToken, refreshToken, expiresAt}` and
/// refreshes server-side via `POST /api/v1/auth/refresh` with
/// `{"grantType":"refresh_token","refreshToken"}`; the response nests the
/// rotated pair under `data` (`{data:{accessToken, refreshToken}}`).
/// Unlike the form-based OAuth grants, `parse_refresh_response` only reads
/// the top level, so this path needs its own executor (see
/// `execute_cline_refresh`). Verified live 2026-09-10.
pub fn build_cline_refresh_request(refresh_token: &str) -> RefreshRequest {
    RefreshRequest {
        url: CLINE_REFRESH_URL.to_string(),
        form_fields: Vec::new(),
        json_body: Some(serde_json::json!({
            "grantType": "refresh_token",
            "refreshToken": refresh_token,
        })),
        headers: Vec::new(),
    }
}

pub fn parse_refresh_response(json_str: &str) -> Result<Tokens, String> {
    let mut value: serde_json::Value = serde_json::from_str(json_str).map_err(|e| e.to_string())?;
    if let Some(object) = value.as_object_mut() {
        for (camel, snake) in [
            ("accessToken", "access_token"),
            ("refreshToken", "refresh_token"),
            ("expiresIn", "expires_in"),
        ] {
            if let Some(value) = object.remove(camel) {
                object.insert(snake.to_string(), value);
            }
        }
    }
    serde_json::from_value::<Tokens>(value).map_err(|e| e.to_string())
}
