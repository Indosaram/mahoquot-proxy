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

use base64::Engine as _;

use crate::account::LoadError;

pub const ZCODE_OAUTH_AUTHORIZE_URL: &str = "https://chat.z.ai/api/oauth/authorize";
pub const ZCODE_OAUTH_CLIENT_ID: &str = "client_P8X5CMWmlaRO9gyO-KSqtg";
pub const ZCODE_OAUTH_REDIRECT_URI: &str = "zcode://oauth/callback";
pub const ZCODE_OAUTH_BROKER_TOKEN_URL: &str = "https://zcode.z.ai/api/v1/oauth/token";
pub const ZCODE_LOGIN_URL: &str = "https://api.z.ai/api/auth/z/login";
pub const ZCODE_USERINFO_URL: &str = "https://chat.z.ai/api/oauth/userinfo";

pub const ZCODE_API_KEY_NAME: &str = "zcode-api-key";

pub const ZCODE_API_BASE: &str = "https://api.z.ai";

/// The plan gateway fronts the Z.ai Start Plan quota that ships with the ZCode
/// desktop client. The JWT from its OAuth CLI login only authorizes this route —
/// the general `api.z.ai` path answers 1113 (no balance) for it. Inference speaks
/// the Anthropic wire format under this prefix.
pub const ZCODE_ANTHROPIC_BASE: &str = "https://zcode.z.ai/api/v1/zcode-plan/anthropic";
pub const ZCODE_MESSAGES_PATH: &str = "/v1/messages";

// ── Plan-gateway auth surface (mirrors opencodex PR #4437) ───────────────────
//
// The plan gateway is what the ZCode desktop client talks to for Start Plan
// quota. Its OAuth CLI flow issues a JWT with no `exp` claim (rejection is
// terminal and surfaces as re-login), the plan route is exempt from any V4
// request signing (`Authorization: Bearer` + `anthropic-version` only), and the
// gateway fingerprints the caller: without the ZCode identity system blocks,
// metadata.user_id, and the client identity headers it answers biz code 3012,
// and unfamiliar callers are challenged by the Aliyun WAF (biz 3007 in the
// body, or a non-empty `x-aliyun-captcha-verify-param` response header).

/// Plan-gateway control-plane origin (login, client config, billing).
pub const ZCODE_PLAN_ORIGIN: &str = "https://zcode.z.ai";
/// OAuth CLI flow: init returns the authorize URL, poll/{flow_id} returns the JWT.
pub const ZCODE_OAUTH_CLI_INIT_URL: &str = "https://zcode.z.ai/api/v1/oauth/cli/init";
pub const ZCODE_OAUTH_CLI_POLL_PREFIX: &str = "/api/v1/oauth/cli/poll";
/// Billing balance the desktop client reads for Start Plan usage (JWT + X-Device-Mid).
pub const ZCODE_PLAN_BILLING_BALANCE_URL: &str =
    "https://zcode.z.ai/api/v1/zcode-plan/billing/balance";
/// Version string the desktop client sends on every plan-gateway call.
pub const ZCODE_APP_VERSION: &str = "3.11.2";
pub const ZCODE_SDK_UA: &str = "ZCode/3.11.2";
/// The AI SDK appends its identity to the UA on Anthropic-wire calls.
pub const ZCODE_ANTHROPIC_SDK_UA: &str = "ai-sdk/anthropic/3.0.81";
/// Response header carrying the WAF challenge parameter on non-2xx replies.
pub const ZCODE_CAPTCHA_PARAM_HEADER: &str = "x-aliyun-captcha-verify-param";
/// Request headers a challenged request replays with after a local solve
/// (mirrors the official client's retry posture; params are single-use).
pub const ZCODE_CAPTCHA_VERIFY_PARAM_HEADER: &str = "X-Aliyun-Captcha-Verify-Param";
pub const ZCODE_CAPTCHA_VERIFY_REGION_HEADER: &str = "X-Aliyun-Captcha-Verify-Region";
/// Solve budget handed to the captcha sidecar (the reference's default).
pub const ZCODE_CAPTCHA_SOLVE_TIMEOUT_MS: u64 = 30_000;
/// In-body magic of the WAF challenge, in both JSON spacing styles.
pub const ZCODE_CAPTCHA_BODY_MARKERS: &[&str] = &["\"code\":3007", "\"code\": 3007"];

/// Plan-meter model ids: the gateway's client config publishes uppercase ids,
/// while gateway clients request the catalog's lowercase aliases.
pub const ZCODE_PLAN_MODEL_IDS: &[(&str, &str)] = &[
    ("glm-5.3", "GLM-5.3"),
    ("glm-5.3-flash", "GLM-5.3-Flash"),
    ("glm-5.2", "GLM-5.2"),
    ("glm-5-turbo", "GLM-5-Turbo"),
];

/// Map a requested model id onto the plan gateway's client-config id. Unknown
/// ids pass through unchanged (the gateway, not the relay, owns that verdict).
pub fn normalize_plan_model(model: &str) -> String {
    let requested = model.trim();
    for (alias, plan_id) in ZCODE_PLAN_MODEL_IDS {
        if requested.eq_ignore_ascii_case(alias) {
            return (*plan_id).to_string();
        }
    }
    requested.to_string()
}

/// Decode the `user_id` claim from a plan JWT without verification — it is the
/// token the gateway issued to us, and it only feeds `metadata.user_id`.
pub fn plan_user_id_from_jwt(jwt: &str) -> Option<String> {
    let payload = jwt.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let value: Value = serde_json::from_slice(&decoded).ok()?;
    let user_id = value.get("user_id")?.as_str()?;
    let trimmed = user_id.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct ZcodeCliInit {
    pub flow_id: String,
    pub authorize_url: String,
    pub poll_interval_sec: Option<u64>,
}

/// Parse `/api/v1/oauth/cli/init` (status 200, `code` 0, `data` carrying the
/// flow id and authorize URL).
pub fn parse_cli_init(body: &Value) -> Result<ZcodeCliInit, String> {
    let data = body
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| "missing data".to_string())?;
    let non_empty = |key: &str| -> Option<String> {
        data.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let flow_id = non_empty("flow_id").ok_or_else(|| "missing flow_id".to_string())?;
    let authorize_url =
        non_empty("authorize_url").ok_or_else(|| "missing authorize_url".to_string())?;
    let poll_interval_sec = data.get("poll_interval_sec").and_then(Value::as_u64);
    Ok(ZcodeCliInit {
        flow_id,
        authorize_url,
        poll_interval_sec,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum ZcodeCliPoll {
    Pending,
    Ready {
        token: String,
        email: Option<String>,
        user_id: Option<String>,
        zai_access_token: Option<String>,
    },
    Failed(String),
}

/// Parse `/api/v1/oauth/cli/poll/{flow_id}`.
pub fn parse_cli_poll(body: &Value) -> ZcodeCliPoll {
    let data = match body.get("data") {
        Some(data) => data,
        None => return ZcodeCliPoll::Pending,
    };
    let non_empty = |key: &str| -> Option<String> {
        data.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    match data
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "ready" => {
            // A ready poll without a token is an upstream protocol violation.
            // Report it as a failure instead of panicking on remote JSON.
            let Some(token) = non_empty("token") else {
                return ZcodeCliPoll::Failed("ready poll without token".to_string());
            };
            ZcodeCliPoll::Ready {
                token,
                email: data
                    .get("user")
                    .and_then(|user| user.get("email"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                user_id: data
                    .get("user")
                    .and_then(|user| user.get("user_id"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                zai_access_token: data
                    .get("zai")
                    .and_then(|zai| zai.get("access_token"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
            }
        }
        "failed" => ZcodeCliPoll::Failed(
            body.get("msg")
                .and_then(Value::as_str)
                .unwrap_or("authorization denied")
                .to_string(),
        ),
        _ => ZcodeCliPoll::Pending,
    }
}

/// True when the response is an Aliyun WAF captcha challenge: a non-2xx status
/// with a verify-param header, or the in-body 3007 marker.
pub fn is_plan_captcha_challenge(status: u16, verify_param: Option<&str>, body: &str) -> bool {
    if (200..300).contains(&status) {
        return false;
    }
    if verify_param.map(str::trim).is_some_and(|v| !v.is_empty()) {
        return true;
    }
    ZCODE_CAPTCHA_BODY_MARKERS.iter().any(|m| body.contains(m))
}

/// HTTP status + Anthropic error type for an in-200 business error. Only 1005
/// is a rate limit (per-window plan quota); everything else is upstream-class.
/// Plan biz code for an exhausted plan quota (`exceed quota limit`).
pub const PLAN_QUOTA_BIZ_CODE: i64 = 1005;

pub fn plan_biz_error(code: i64) -> (u16, &'static str) {
    if code == PLAN_QUOTA_BIZ_CODE {
        (429, "rate_limit_error")
    } else {
        (502, "upstream_error")
    }
}

/// Captcha scene advertised by the plan gateway's public client config.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanCaptchaScene {
    pub scene_id: String,
    pub prefix: String,
    pub region: String,
}

/// Read the captcha scene out of a `client/configs` body. `None` when the
/// config endpoint is unreachable-by-shape or the captcha is disabled — the
/// challenge handler then skips straight to the static 502 mapping.
pub fn parse_captcha_scene(body: &Value) -> Option<PlanCaptchaScene> {
    let cfg = body
        .get("data")?
        .get("configs")?
        .get("captcha")?
        .as_object()?;
    if cfg.get("enabled").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let scene_id = cfg.get("sceneId").and_then(Value::as_str)?.trim();
    let prefix = cfg.get("prefix").and_then(Value::as_str)?.trim();
    if scene_id.is_empty() || prefix.is_empty() {
        return None;
    }
    Some(PlanCaptchaScene {
        scene_id: scene_id.to_string(),
        prefix: prefix.to_string(),
        region: cfg
            .get("region")
            .and_then(Value::as_str)
            .unwrap_or("sgp")
            .trim()
            .to_string(),
    })
}

/// Query for the public client-config endpoint (the desktop client's captcha
/// scene source). Tests override the origin via `ZCODE_CAPTCHA_CONFIG_URL`.
pub fn captcha_config_url(origin: &str, app_version: &str, platform: &str) -> String {
    format!(
        "{origin}/api/v1/client/configs?app_version={}&platform={}",
        urlencode_component(app_version),
        urlencode_component(platform)
    )
}

fn urlencode_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Companion headers the desktop client sends on plan-gateway billing calls —
/// the identity set without `X-ZCode-Agent` (the client omits it on the control
/// plane), plus the required `X-Device-Mid` (its absence answers biz 3001).
pub fn plan_billing_headers(
    jwt: &str,
    device_mid: &str,
    platform: &str,
    os_category: &str,
    language: &str,
    timezone: &str,
) -> Vec<(String, String)> {
    vec![
        ("Accept".to_string(), "application/json".to_string()),
        ("Authorization".to_string(), format!("Bearer {jwt}")),
        ("HTTP-Referer".to_string(), ZCODE_PLAN_ORIGIN.to_string()),
        ("User-Agent".to_string(), ZCODE_SDK_UA.to_string()),
        (
            "X-ZCode-App-Version".to_string(),
            ZCODE_APP_VERSION.to_string(),
        ),
        ("X-Title".to_string(), "Z Code@cli".to_string()),
        ("X-Release-Channel".to_string(), "production".to_string()),
        ("X-Client-Language".to_string(), language.to_string()),
        ("X-Client-Timezone".to_string(), timezone.to_string()),
        ("X-Platform".to_string(), platform.to_string()),
        ("X-Os-Category".to_string(), os_category.to_string()),
        ("X-Device-Mid".to_string(), device_mid.to_string()),
    ]
}

#[derive(Debug, Clone, PartialEq)]
pub struct ZcodePlanBalanceRow {
    pub show_name: String,
    pub total_units: f64,
    pub used_units: f64,
    pub period_end_unix: Option<i64>,
}

/// Parse the balance rows out of a `billing/balance` response body (`code` 0,
/// `data.balances[]` with `show_name`, `total_units`, `used_units` or
/// `remaining_units`, and an optional `expires_at`).
pub fn parse_plan_balances(data: &Value) -> Vec<ZcodePlanBalanceRow> {
    let Some(rows) = data.get("balances").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for row in rows {
        let Some(obj) = row.as_object() else { continue };
        let total = obj
            .get("total_units")
            .or_else(|| obj.get("totalUnits"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if total <= 0.0 {
            continue;
        }
        let show_name = obj
            .get("show_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("balance")
            .to_string();
        let used = obj
            .get("used_units")
            .or_else(|| obj.get("usedUnits"))
            .and_then(Value::as_f64)
            .unwrap_or_else(|| {
                let remaining = obj
                    .get("remaining_units")
                    .or_else(|| obj.get("remainingUnits"))
                    .and_then(Value::as_f64);
                remaining.map(|r| (total - r).max(0.0)).unwrap_or(0.0)
            });
        let period_end_unix = obj
            .get("expires_at")
            .or_else(|| obj.get("expiresAt"))
            .and_then(parse_reset_at);
        out.push(ZcodePlanBalanceRow {
            show_name,
            total_units: total,
            used_units: used,
            period_end_unix,
        });
    }
    out
}

/// `expires_at` may be a unix epoch (number) or an RFC3339-ish string; keep the
/// number form and ignore the string form (no chrono dependency in pure code).
fn parse_reset_at(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_f64().map(|f| f as i64))
}

use mahoquot_registry::{embedded_snapshot, ProviderContribution, ProviderId, RegistrySnapshot};
use serde_json::Value;

pub fn provider_id() -> ProviderId {
    ProviderId::zcode()
}

pub fn contribution(snapshot: &RegistrySnapshot) -> ProviderContribution {
    snapshot.contribution_for_provider(&provider_id())
}

pub fn default_contribution() -> ProviderContribution {
    contribution(embedded_snapshot())
}

pub fn supported_models(snapshot: &RegistrySnapshot) -> Vec<String> {
    contribution(snapshot).supported_model_ids()
}

pub fn is_zcode_model_in_snapshot(snapshot: &RegistrySnapshot, model: &str) -> bool {
    contribution(snapshot).supports_model(model)
}

pub fn is_zcode_model(model: &str) -> bool {
    is_zcode_model_in_snapshot(embedded_snapshot(), model)
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
    match parse_zcode_input(callback_url, expected_state)? {
        ZcodeInput::AuthorizationCode(code) => Ok(code),
        ZcodeInput::AuthorizationUrl(_) => {
            Err("ZCode authorization is waiting for browser approval".to_string())
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ZcodeInput {
    AuthorizationCode(String),
    AuthorizationUrl(String),
}

pub fn parse_zcode_input(input: &str, expected_state: &str) -> Result<ZcodeInput, String> {
    let input = input.trim();
    if input.is_empty() || expected_state.is_empty() {
        return Err("ZCode callback and login state are required".to_string());
    }
    let normalized = if input.starts_with("code=") || input.starts_with("?code=") {
        format!("zcode://oauth/callback?{}", input.trim_start_matches('?'))
    } else {
        input.to_string()
    };
    let mut url =
        match reqwest::Url::parse(&normalized) {
            Ok(url) => url,
            Err(_)
                if input.len() >= 4
                    && input
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.+/=~".contains(&byte)) =>
            {
                return Ok(ZcodeInput::AuthorizationCode(input.to_string()));
            }
            Err(_) => return Err(
                "ZCode callback must be a URL, code=...&state=... query, or an authorization code"
                    .to_string(),
            ),
        };
    if url.scheme() == "zcode" {
        if !url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("oauth"))
            || !url.path().eq_ignore_ascii_case("/callback")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
        {
            return Err("ZCode callback must use zcode://oauth/callback".to_string());
        }
        if url.query().is_none() {
            let fragment = url.fragment().map(str::to_string);
            url.set_query(fragment.as_deref());
            url.set_fragment(None);
        }
        return callback_code_from_url(&url, expected_state).map(ZcodeInput::AuthorizationCode);
    }
    if url.scheme() != "https"
        || url.host_str() != Some("chat.z.ai")
        || !matches!(url.path(), "/auth/oauth/authorize" | "/api/oauth/authorize")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err("ZCode sign-in URL must be the chat.z.ai authorization page".to_string());
    }
    if url.query_pairs().any(|(key, _)| key == "code") {
        return callback_code_from_url(&url, expected_state).map(ZcodeInput::AuthorizationCode);
    }
    let states: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key == "state")
        .map(|(_, value)| value)
        .collect();
    if states.len() > 1 {
        return Err("GLM ZCode callback state did not match".to_string());
    }
    let redirects: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key == "redirect_uri")
        .map(|(_, value)| value)
        .collect();
    if redirects.len() == 1 {
        if let Ok(inner) = reqwest::Url::parse(&redirects[0]) {
            if inner.scheme() == "zcode" && (inner.query().is_some() || inner.fragment().is_some())
            {
                if states.first().is_some_and(|state| state != expected_state) {
                    return Err("GLM ZCode callback state did not match".to_string());
                }
                return parse_zcode_input(&redirects[0], expected_state);
            }
        }
    }
    for (key, expected) in [
        ("client_id", ZCODE_OAUTH_CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", ZCODE_OAUTH_REDIRECT_URI),
    ] {
        let mut values = url.query_pairs().filter(|(name, _)| name == key);
        if values.next().is_none_or(|(_, value)| value != expected) || values.next().is_some() {
            return Err(format!("ZCode authorization URL has invalid {key}"));
        }
    }
    Ok(ZcodeInput::AuthorizationUrl(zcode_authorize_url(
        expected_state,
    )))
}

fn callback_code_from_url(callback: &reqwest::Url, expected_state: &str) -> Result<String, String> {
    let mut code = None;
    let mut state = None;
    for (key, value) in callback.query_pairs() {
        match key.as_ref() {
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

/// Pastes and CLI logins both write the same far-future expiry: the plan JWT
/// has no `exp` claim, so staleness is detected by upstream 401/3012 instead.
fn default_zcode_expiry() -> String {
    "2099-12-31T00:00:00Z".to_string()
}

#[derive(Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct ZcodeAccount {
    #[serde(default)]
    pub identity_slug: String,
    /// The plan-gateway JWT issued by the OAuth CLI flow (rejection is terminal:
    /// re-login, never a silent refresh).
    pub access_token: String,
    /// Upstream Z.AI OAuth token from the same login (kept for diagnosability).
    /// Optional: the JWT paste flow has no OAuth login, and the CLI flow
    /// defaults it to empty when Z.AI omits it.
    #[serde(default)]
    pub refresh_token: String,
    pub email: String,
    /// The plan JWT carries no `exp` claim; pasted tokens get the same
    /// far-future expiry the CLI flow writes (staleness is detected upstream).
    #[serde(default = "default_zcode_expiry")]
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
            "https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages"
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

    #[test]
    fn plan_model_normalizes_catalog_aliases() {
        assert_eq!(normalize_plan_model("glm-5.3-flash"), "GLM-5.3-Flash");
        assert_eq!(normalize_plan_model("GLM-5.3"), "GLM-5.3");
        assert_eq!(normalize_plan_model("glm-5-turbo"), "GLM-5-Turbo");
        assert_eq!(normalize_plan_model("glm-4.6"), "glm-4.6");
        assert_eq!(normalize_plan_model("  glm-5.2 "), "GLM-5.2");
    }

    #[test]
    fn plan_jwt_yields_the_user_id_claim() {
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"user_id":"usr-42"}"#);
        let jwt = format!("eyJhbGciOiJIUzI1NiJ9.{payload}.sig");
        assert_eq!(plan_user_id_from_jwt(&jwt).as_deref(), Some("usr-42"));
        assert_eq!(plan_user_id_from_jwt("not-a-jwt"), None);
        let empty = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"user_id":""}"#);
        assert_eq!(plan_user_id_from_jwt(&format!("h.{empty}.s")), None);
    }

    #[test]
    fn cli_init_parses_flow_and_interval() {
        let init = parse_cli_init(&serde_json::json!({
            "code": 0,
            "data": {
                "flow_id": "fl-1",
                "authorize_url": "https://zcode.z.ai/authorize?x=1",
                "poll_interval_sec": 3
            }
        }))
        .unwrap();
        assert_eq!(init.flow_id, "fl-1");
        assert_eq!(init.authorize_url, "https://zcode.z.ai/authorize?x=1");
        assert_eq!(init.poll_interval_sec, Some(3));
        assert!(parse_cli_init(&serde_json::json!({"data": {}})).is_err());
    }

    #[test]
    fn cli_poll_maps_pending_ready_and_failed() {
        assert_eq!(
            parse_cli_poll(&serde_json::json!({"data": {"status": "pending"}})),
            ZcodeCliPoll::Pending
        );
        assert_eq!(
            parse_cli_poll(&serde_json::json!({"data": {}})),
            ZcodeCliPoll::Pending
        );
        match parse_cli_poll(&serde_json::json!({
            "data": {
                "status": "ready",
                "token": "eyJ.plan.jwt",
                "user": {"user_id": "u1", "email": "a@b.c"},
                "zai": {"access_token": "zai-tok"}
            }
        })) {
            ZcodeCliPoll::Ready {
                token,
                email,
                user_id,
                zai_access_token,
            } => {
                assert_eq!(token, "eyJ.plan.jwt");
                assert_eq!(email.as_deref(), Some("a@b.c"));
                assert_eq!(user_id.as_deref(), Some("u1"));
                assert_eq!(zai_access_token.as_deref(), Some("zai-tok"));
            }
            other => panic!("expected ready, got {other:?}"),
        }
        assert_eq!(
            parse_cli_poll(&serde_json::json!({"data": {"status": "failed"}, "msg": "denied"})),
            ZcodeCliPoll::Failed("denied".to_string())
        );
    }

    #[test]
    fn ready_poll_without_a_token_is_a_failure_not_a_panic() {
        // given an upstream that reports ready but omits or blanks the token
        for body in [
            serde_json::json!({"data": {"status": "ready"}}),
            serde_json::json!({"data": {"status": "ready", "token": "   "}}),
        ] {
            // when the poll is parsed
            let parsed = parse_cli_poll(&body);
            // then it surfaces as a failure the caller can report
            assert_eq!(
                parsed,
                ZcodeCliPoll::Failed("ready poll without token".to_string())
            );
        }
    }

    #[test]
    fn captcha_challenge_detected_from_header_or_body() {
        assert!(is_plan_captcha_challenge(403, Some("param"), ""));
        assert!(is_plan_captcha_challenge(
            403,
            None,
            "{\"code\":3007,\"msg\":\"waf\"}"
        ));
        assert!(is_plan_captcha_challenge(502, None, "{\"code\": 3007}"));
        assert!(!is_plan_captcha_challenge(200, Some("param"), ""));
        assert!(!is_plan_captcha_challenge(
            403,
            Some("  "),
            "{\"code\":3006}"
        ));
        assert!(!is_plan_captcha_challenge(401, None, "unauthorized"));
    }

    #[test]
    fn biz_errors_map_to_real_statuses() {
        assert_eq!(plan_biz_error(1005), (429, "rate_limit_error"));
        assert_eq!(plan_biz_error(3012), (502, "upstream_error"));
        assert_eq!(plan_biz_error(1113), (502, "upstream_error"));
    }

    #[test]
    fn billing_headers_carry_identity_and_device_mid() {
        let headers = plan_billing_headers(
            "jwt",
            "mid-1",
            "darwin-arm64",
            "macos",
            "ko-KR",
            "Asia/Seoul",
        );
        let get = |name: &str| {
            headers
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("Authorization").as_deref(), Some("Bearer jwt"));
        assert_eq!(get("X-Device-Mid").as_deref(), Some("mid-1"));
        assert_eq!(get("X-Title").as_deref(), Some("Z Code@cli"));
        assert_eq!(get("X-Platform").as_deref(), Some("darwin-arm64"));
        assert_eq!(get("X-ZCode-Agent"), None);
        assert_eq!(get("anthropic-version"), None);
    }

    #[test]
    fn plan_balances_parse_rows_and_reset() {
        let rows = parse_plan_balances(&serde_json::json!({
            "balances": [
                {"show_name": "GLM-5.3", "total_units": 3000000.0,
                 "used_units": 300000.0, "expires_at": 1900000000},
                {"show_name": "", "totalUnits": 100, "remainingUnits": 40},
                {"total_units": 0}
            ]
        }));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].show_name, "GLM-5.3");
        assert_eq!(rows[0].used_units, 300000.0);
        assert_eq!(rows[0].period_end_unix, Some(1_900_000_000));
        assert_eq!(rows[1].show_name, "balance");
        assert_eq!(rows[1].used_units, 60.0);
    }
}
