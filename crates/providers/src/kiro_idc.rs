use serde_json::{json, Value};

pub const KIRO_IDC_DEFAULT_REGION: &str = "us-east-1";

pub const KIRO_IDC_DEFAULT_START_URL: &str = "https://view.awsapps.com/start";

pub const KIRO_IDC_SCOPES: [&str; 3] = [
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations",
];

/// Only these two commercial hosts publish `ListAvailableProfiles`, and their
/// names are not a region pattern: us-east-1 is `codewhisperer.` while
/// eu-central-1 is `q.`. A tenant's Identity Center region has no host of its
/// own, so both are asked in order.
pub const KIRO_PROFILE_ENDPOINTS: [(&str, &str); 2] = [
    ("us-east-1", "https://codewhisperer.us-east-1.amazonaws.com/"),
    ("eu-central-1", "https://q.eu-central-1.amazonaws.com/"),
];

pub const KIRO_LIST_PROFILES_TARGET: &str = "AmazonCodeWhispererService.ListAvailableProfiles";

/// A Builder ID tenant has no account-scoped profile, and `ListAvailableProfiles`
/// answers 403 for it ("AWS Builder ID is not supported for this operation").
/// Kiro CLI still sends a request-scoped service profile on the runtime host, so
/// an account that cannot list its own profile is not a failed login: it is a
/// Builder ID account and routes with this fixed ARN.
pub const KIRO_BUILDER_ID_SERVICE_PROFILE_ARN: &str =
    "arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdcClient {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdcDeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub interval_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdcPollOutcome {
    Pending,
    SlowDown,
    Approved {
        access_token: String,
        refresh_token: String,
        expires_in: i64,
    },
}

/// Rejects anything outside the AWS region alphabet: the value is interpolated
/// into the request host, so a `../` or `/` would reshape the URL.
pub fn validate_region(region: &str) -> Result<&str, String> {
    let valid = !region.is_empty()
        && region.len() <= 32
        && region
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !region.starts_with('-')
        && !region.ends_with('-');
    if valid {
        Ok(region)
    } else {
        Err(format!("invalid AWS region: {region}"))
    }
}

/// Accepts only `https` on an `awsapps.com` host: the operator is shown this
/// URL and follows it by hand, so an attacker-supplied value would phish them.
pub fn validate_start_url(start_url: &str) -> Result<&str, String> {
    let rest = start_url
        .strip_prefix("https://")
        .ok_or_else(|| "start URL must be https".to_string())?;
    let host_end = rest.find('/').unwrap_or(rest.len());
    let host = &rest[..host_end];
    let host_ok = host.ends_with(".awsapps.com")
        || host == "awsapps.com"
        || host == "view.awsapps.com";
    if !host_ok || host.contains('@') {
        return Err("start URL must be an awsapps.com portal".to_string());
    }
    Ok(start_url)
}

pub fn oidc_url(region: &str, path: &str) -> Result<String, String> {
    let region = validate_region(region)?;
    Ok(format!("https://oidc.{region}.amazonaws.com{path}"))
}

fn error_reason(body: &Value, status: u16) -> String {
    let code = body
        .get("error")
        .and_then(|error| {
            error
                .as_str()
                .or_else(|| error.get("code").and_then(Value::as_str))
        })
        .or_else(|| body.get("errorCode").and_then(Value::as_str))
        .unwrap_or("");
    let message = body
        .get("error_description")
        .or_else(|| body.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if code.is_empty() && message.is_empty() {
        return format!("HTTP {status}");
    }
    format!("{code}: {message}").trim_end_matches(": ").to_string()
}

fn text_field(body: &Value, key: &str) -> Result<String, String> {
    body.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("AWS SSO OIDC response missing {key}"))
}

pub async fn register_client(client: &reqwest::Client, region: &str) -> Result<IdcClient, String> {
    let url = oidc_url(region, "/client/register")?;
    let response = client
        .post(&url)
        .json(&json!({
            "clientName": "kiro-cli",
            "clientType": "public",
            "scopes": KIRO_IDC_SCOPES,
        }))
        .send()
        .await
        .map_err(|error| format!("SSO OIDC register failed: {error}"))?;
    let status = response.status().as_u16();
    let body: Value = response
        .json()
        .await
        .map_err(|error| format!("SSO OIDC register returned non-JSON: {error}"))?;
    if status >= 400 {
        return Err(format!(
            "SSO OIDC register rejected ({status}): {}",
            error_reason(&body, status)
        ));
    }
    Ok(IdcClient {
        client_id: text_field(&body, "clientId")?,
        client_secret: text_field(&body, "clientSecret")?,
    })
}

pub async fn start_device_authorization(
    client: &reqwest::Client,
    region: &str,
    registration: &IdcClient,
    start_url: &str,
) -> Result<IdcDeviceAuthorization, String> {
    let url = oidc_url(region, "/device_authorization")?;
    let start_url = validate_start_url(start_url)?;
    let response = client
        .post(&url)
        .json(&json!({
            "clientId": registration.client_id,
            "clientSecret": registration.client_secret,
            "startUrl": start_url,
        }))
        .send()
        .await
        .map_err(|error| format!("SSO OIDC device authorization failed: {error}"))?;
    let status = response.status().as_u16();
    let body: Value = response
        .json()
        .await
        .map_err(|error| format!("SSO OIDC device authorization returned non-JSON: {error}"))?;
    if status >= 400 {
        return Err(format!(
            "SSO OIDC device authorization rejected ({status}): {}",
            error_reason(&body, status)
        ));
    }
    Ok(IdcDeviceAuthorization {
        device_code: text_field(&body, "deviceCode")?,
        user_code: text_field(&body, "userCode")?,
        verification_uri: text_field(&body, "verificationUri")?,
        verification_uri_complete: body
            .get("verificationUriComplete")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        interval_secs: body
            .get("interval")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .max(1),
    })
}

pub async fn poll_device_token(
    client: &reqwest::Client,
    region: &str,
    registration: &IdcClient,
    device_code: &str,
) -> Result<IdcPollOutcome, String> {
    let url = oidc_url(region, "/token")?;
    let response = client
        .post(&url)
        .json(&json!({
            "clientId": registration.client_id,
            "clientSecret": registration.client_secret,
            "deviceCode": device_code,
            "grantType": "urn:ietf:params:oauth:grant-type:device_code",
        }))
        .send()
        .await
        .map_err(|error| format!("SSO OIDC token poll failed: {error}"))?;
    let status = response.status().as_u16();
    let body: Value = response
        .json()
        .await
        .map_err(|error| format!("SSO OIDC token poll returned non-JSON: {error}"))?;
    if status >= 400 {
        let reason = error_reason(&body, status);
        if reason.contains("authorization_pending") {
            return Ok(IdcPollOutcome::Pending);
        }
        if reason.contains("slow_down") {
            return Ok(IdcPollOutcome::SlowDown);
        }
        return Err(format!("SSO OIDC token poll rejected ({status}): {reason}"));
    }
    let access_token = text_field(&body, "accessToken")?;
    let refresh_token = body
        .get("refreshToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let expires_in = body
        .get("expiresIn")
        .and_then(Value::as_i64)
        .unwrap_or(3600);
    Ok(IdcPollOutcome::Approved {
        access_token,
        refresh_token,
        expires_in,
    })
}

/// Resolves the profile an account is routed by. A tenant whose Identity Center
/// does publish a profile gets that ARN; a Builder ID tenant publishes none and
/// routes with the fixed service profile instead, so a listing failure is a
/// fallback rather than a failed login.
pub async fn resolve_profile_arn(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<String, String> {
    match list_account_profile_arn(client, access_token).await {
        Ok(arn) => Ok(arn),
        // A suspended or otherwise locked account also fails here, and routing it
        // with a service profile would only store a credential that cannot work.
        // Only the Builder ID answer is a fallback; everything else keeps its own
        // reason, because "suspended" and "no profile" need different fixes.
        Err(error) if error.contains("Builder ID") => {
            Ok(KIRO_BUILDER_ID_SERVICE_PROFILE_ARN.to_string())
        }
        Err(error) => Err(format!("cannot resolve a Kiro profile: {error}")),
    }
}

async fn list_account_profile_arn(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<String, String> {
    let mut failures: Vec<String> = Vec::new();
    for (region, url) in KIRO_PROFILE_ENDPOINTS {
        match list_profiles_at(client, access_token, url).await {
            Ok(mut profiles) => {
                if !profiles.is_empty() {
                    return Ok(profiles.remove(0));
                }
            }
            Err(error) => failures.push(format!("{region}: {error}")),
        }
    }
    if failures.len() == KIRO_PROFILE_ENDPOINTS.len() {
        return Err(failures.join("; "));
    }
    Err("no Kiro profile published for this account".to_string())
}

async fn list_profiles_at(
    client: &reqwest::Client,
    access_token: &str,
    url: &str,
) -> Result<Vec<String>, String> {
    let response = client
        .post(url)
        .header("content-type", "application/x-amz-json-1.0")
        .header("x-amz-target", KIRO_LIST_PROFILES_TARGET)
        .bearer_auth(access_token)
        .json(&json!({ "maxResults": 10 }))
        .send()
        .await
        .map_err(|error| format!("{url} unreachable: {error}"))?;
    let status = response.status().as_u16();
    // The reply is `application/x-amz-json-1.0`, which a JSON content-type
    // gate refuses, so the body is decoded by hand.
    let text = response
        .text()
        .await
        .map_err(|error| format!("{url} body read failed: {error}"))?;
    let body: Value = serde_json::from_str(&text)
        .map_err(|error| format!("{url} answered {status} with a non-JSON body: {error}"))?;
    if status != 200 {
        return Err(format!(
            "{url} answered {status}: {}",
            error_reason(&body, status)
        ));
    }
    Ok(body
        .get("profiles")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("arn").and_then(Value::as_str))
                .filter(|arn| !arn.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

/// The ARN names the region the profile lives in, which is the region its
/// management host answers on. A malformed ARN yields no region rather than a
/// guess.
pub fn region_from_profile_arn(arn: &str) -> Option<&str> {
    let mut parts = arn.split(':');
    if parts.next()? != "arn" {
        return None;
    }
    let _partition = parts.next()?;
    let _service = parts.next()?;
    let region = parts.next()?;
    if validate_region(region).is_ok() {
        Some(region)
    } else {
        None
    }
}

/// Best-effort account label. An SSO OIDC token carries no email, and accounts
/// are listed by that label, so the usage endpoint is asked once with
/// `isEmailRequired=true`. Everything about this call is optional: the
/// credential is valid without it, so a failure or an absent field yields
/// `None` and never fails the login.
pub async fn fetch_account_email(
    client: &reqwest::Client,
    access_token: &str,
    profile_arn: &str,
) -> Option<String> {
    let region = region_from_profile_arn(profile_arn)?;
    let url = format!("https://management.{region}.kiro.dev/getUsageLimits");
    let response = client
        .get(url)
        .query(&[
            ("origin", "AI_EDITOR"),
            ("profileArn", profile_arn),
            ("resourceType", "AGENTIC_REQUEST"),
            ("isEmailRequired", "true"),
        ])
        .header("user-agent", "aws-sdk-js/1.0.0 KiroIDE-1.1.70-mahoquot")
        .header("x-amz-user-agent", "aws-sdk-js/1.0.0 KiroIDE-1.1.70-mahoquot")
        .bearer_auth(access_token)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: Value = response.json().await.ok()?;
    body.pointer("/userInfo/email")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .map(str::to_string)
}

pub fn credential_document(
    label: &str,
    region: &str,
    registration: &IdcClient,
    access_token: &str,
    refresh_token: &str,
    expired: &str,
    profile_arn: &str,
) -> Value {
    json!({
        "type": "kiro",
        "identity_slug": "",
        "access_token": access_token,
        "refresh_token": refresh_token,
        "email": label,
        "expired": expired,
        "region": region,
        "auth_mode": "idc",
        "client_id": registration.client_id,
        "client_secret": registration.client_secret,
        "profileArn": profile_arn,
        "disabled": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oidc_url_rejects_regions_that_could_reshape_the_host() {
        assert_eq!(
            oidc_url("eu-central-1", "/token").unwrap(),
            "https://oidc.eu-central-1.amazonaws.com/token"
        );
        for bad in ["", "us-east-1/evil", "us east 1", "US-EAST-1", "-us"] {
            assert!(oidc_url(bad, "/token").is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn start_url_must_be_an_awsapps_portal() {
        assert!(validate_start_url("https://d-90666297b0.awsapps.com/start").is_ok());
        assert!(validate_start_url("https://view.awsapps.com/start").is_ok());
        for bad in [
            "http://d-90666297b0.awsapps.com/start",
            "https://awsapps.com.evil.test/start",
            "https://user@d-90666297b0.awsapps.com/start",
            "https://evil.test/start",
        ] {
            assert!(validate_start_url(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn pending_and_slow_down_are_classified_from_the_error_body() {
        let pending = json!({"error": "authorization_pending"});
        assert!(error_reason(&pending, 400).contains("authorization_pending"));
        let slow = json!({"error": {"code": "slow_down"}});
        assert!(error_reason(&slow, 400).contains("slow_down"));
        let expired = json!({"error": "expired_token", "error_description": "code expired"});
        assert!(error_reason(&expired, 400).contains("code expired"));
    }

    #[test]
    fn credential_document_matches_the_kiro_loader_contract() {
        let registration = IdcClient {
            client_id: "cid".into(),
            client_secret: "secret".into(),
        };
        let doc = credential_document(
            "admin@noveling.dev",
            "us-east-1",
            &registration,
            "at",
            "rt",
            "2026-11-08T00:00:00Z",
            "arn:aws:codewhisperer:us-east-1:123456789012:profile/abc",
        );
        assert_eq!(doc["type"], "kiro");
        assert_eq!(doc["auth_mode"], "idc");
        assert_eq!(doc["region"], "us-east-1");
        assert_eq!(doc["client_id"], "cid");
        assert_eq!(doc["disabled"], false);
        let parsed: crate::kiro::KiroAccount =
            serde_json::from_value(doc).expect("loader must accept the document");
        assert_eq!(parsed.auth_mode, crate::kiro::KiroAuthMode::Idc);
        assert_eq!(parsed.email, "admin@noveling.dev");
        assert_eq!(
            parsed.profile_arn,
            "arn:aws:codewhisperer:us-east-1:123456789012:profile/abc"
        );
    }

    #[test]
    fn a_builder_id_rejection_falls_back_to_the_service_profile() {
        // Measured 2026-11-08 against a Builder ID tenant, whose
        // ListAvailableProfiles answer is this 403 rather than a profile list.
        let body = json!({
            "__type": "com.amazon.aws.codewhisperer#AccessDeniedException",
            "message": "AWS Builder ID is not supported for this operation."
        });
        let reason = error_reason(&body, 403);
        assert!(reason.contains("Builder ID"), "reason was {reason}");
    }

    #[test]
    fn a_suspended_account_is_not_mistaken_for_a_builder_id_tenant() {
        // Same 403 shape, different cause: routing this with the service
        // profile would store a credential no request can use.
        let body = json!({
            "__type": "com.amazon.aws.codewhisperer#AccessDeniedException",
            "message": "Your User ID (04283458) temporarily is suspended. We've locked your account as a security precaution."
        });
        let reason = error_reason(&body, 403);
        assert!(!reason.contains("Builder ID"), "reason was {reason}");
    }

    #[test]
    fn profile_hosts_are_literal_and_not_a_region_pattern() {
        assert_eq!(
            KIRO_PROFILE_ENDPOINTS[0],
            ("us-east-1", "https://codewhisperer.us-east-1.amazonaws.com/")
        );
        assert_eq!(
            KIRO_PROFILE_ENDPOINTS[1],
            ("eu-central-1", "https://q.eu-central-1.amazonaws.com/")
        );
    }

    #[test]
    fn the_profile_region_is_read_from_the_arn_fourth_field() {
        assert_eq!(
            region_from_profile_arn("arn:aws:codewhisperer:eu-central-1:123456789012:profile/x"),
            Some("eu-central-1")
        );
        // A partition and an empty region are both possible shapes; only the
        // latter is refused, because an empty host segment cannot be asked.
        assert_eq!(
            region_from_profile_arn("arn:aws-us-gov:codewhisperer:us-gov-west-1:1:profile/x"),
            Some("us-gov-west-1")
        );
        assert_eq!(region_from_profile_arn("arn:aws:codewhisperer::1:profile/x"), None);
        assert_eq!(region_from_profile_arn(""), None);
        assert_eq!(region_from_profile_arn("not-an-arn"), None);
    }
}
