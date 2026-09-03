use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::account::{AccountMember, ProviderKind};
use crate::state::AppState;
use crate::usage::{parse_cursor_usage_summary, parse_kiro_usage_summary, WhamUsage};

const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CODEX_RESET_URL: &str =
    "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume";
/// The reset endpoint is a write and rejects clients that don't look like the
/// official CLI, so requests mirror the Codex CLI user agent.
const CODEX_USER_AGENT: &str = "codex_cli_rs/0.76.0 (Debian 13.0.0; x86_64) WindowsTerminal";
const CLAUDE_API_BASE: &str = "https://api.anthropic.com";
const CLAUDE_USAGE_PATH: &str = "/api/oauth/usage";
const CURSOR_USAGE_URL: &str = "https://api2.cursor.sh/auth/usage-summary";
const KIRO_USAGE_URL: &str = "https://q.us-east-1.amazonaws.com/getUsageLimits?origin=AI_EDITOR&resourceType=AGENTIC_REQUEST";
/// Undocumented and version-dated: the endpoint is gated on this exact beta
/// header, and a new date means the payload can change without notice.
const CLAUDE_OAUTH_BETA: &str = "oauth-2025-04-20";

/// Hidden relay-usage feature: the /v1/usage/self cumulative-counter endpoint
/// only exists on the nekos/ccapi relay targets, so the poller filters on the
/// account's target API, not on the key material. Generic API-key accounts
/// never enter this path (no settings, no UI trace), and static-key chat
/// routing is unaffected.
const RELAY_USAGE_HOST_MARKERS: [&str; 2] = ["nekos", "ccapi"];

/// After a 429 from a usage endpoint, skip that account's usage polls for this
/// long. Anthropic's OAuth usage endpoint throttles hard and sends no
/// Retry-After, so re-polling every cycle keeps the throttle hot and an
/// account that has never observed a snapshot would never get one.
const USAGE_RATE_LIMIT_BACKOFF_SECS: i64 = 900;

fn poll_allowed(now_unix: i64, backoff_until_unix: Option<i64>) -> bool {
    backoff_until_unix.is_none_or(|until| now_unix >= until)
}

fn relay_usage_target(key: Option<String>, base: Option<&str>) -> Option<String> {
    let key = key?;
    let base = base?;
    RELAY_USAGE_HOST_MARKERS
        .iter()
        .any(|marker| base.contains(marker))
        .then_some(key)
}

fn relay_usage_key(member: &AccountMember) -> Option<String> {
    relay_usage_target(member.relay_api_key(), member.upstream_override.as_deref())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn poll_backoff_until(state: &AppState, account: &str) -> Option<i64> {
    state
        .usage_poll_backoff
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(account)
        .copied()
}

fn set_poll_backoff(state: &AppState, account: &str, until_unix: i64) {
    state
        .usage_poll_backoff
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(account.to_string(), until_unix);
}

fn clear_poll_backoff(state: &AppState, account: &str) {
    state
        .usage_poll_backoff
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(account);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetAttemptPolicy {
    Success,
    RefreshAndRetry,
    Unauthorized,
    NoCredit,
    Upstream,
}

/// Classify one upstream reset response without coupling retry policy to I/O.
pub fn reset_attempt_policy(
    status: reqwest::StatusCode,
    already_refreshed: bool,
    body: &str,
) -> ResetAttemptPolicy {
    if status.is_success() {
        return ResetAttemptPolicy::Success;
    }
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return if already_refreshed {
            ResetAttemptPolicy::Unauthorized
        } else {
            ResetAttemptPolicy::RefreshAndRetry
        };
    }
    let detail = body.to_ascii_lowercase();
    if detail.contains("credit")
        && (detail.contains("no ")
            || detail.contains("zero")
            || detail.contains("exhaust")
            || detail.contains("available_count\":0"))
    {
        return ResetAttemptPolicy::NoCredit;
    }
    ResetAttemptPolicy::Upstream
}

/// A reset retry must retain the first request id rather than minting another.
pub fn retain_redeem_request_id(existing: &str, generated: &str) -> String {
    if existing.is_empty() {
        generated.to_string()
    } else {
        existing.to_string()
    }
}

#[derive(Debug)]
pub enum QuotaError {
    Unsupported,
    Unauthorized,
    NoCredit,
    Network(String),
    /// The usage endpoint answered 429: a stale read, not a quota fact. The
    /// caller backs off instead of surfacing it as a poll failure.
    RateLimited,
    Upstream(String),
}

impl std::fmt::Display for QuotaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuotaError::Unsupported => write!(f, "provider does not support reset credits"),
            QuotaError::Unauthorized => write!(f, "account rejected the credentials after refresh"),
            QuotaError::NoCredit => write!(f, "no reset credits available"),
            QuotaError::Network(m) => write!(f, "reset network error: {m}"),
            QuotaError::RateLimited => write!(f, "usage endpoint rate limited"),
            QuotaError::Upstream(m) => write!(f, "{m}"),
        }
    }
}

fn codex_account_id(member: &AccountMember) -> String {
    let guard = member
        .inner
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match &*guard {
        crate::account::ProviderAccount::Codex(a) => a.account_id.clone(),
        _ => String::new(),
    }
}

/// Poll one account's quota and store it. Returns `Unsupported` only for
/// providers that genuinely publish no quota API.
pub async fn refresh_account_usage(
    state: &AppState,
    member: &Arc<AccountMember>,
) -> Result<(), QuotaError> {
    if !poll_allowed(now_unix(), poll_backoff_until(state, &member.id)) {
        return Ok(());
    }
    let outcome = refresh_account_usage_inner(state, member).await;
    match outcome {
        Ok(()) => {
            clear_poll_backoff(state, &member.id);
            Ok(())
        }
        Err(QuotaError::RateLimited) => {
            set_poll_backoff(
                state,
                &member.id,
                now_unix() + USAGE_RATE_LIMIT_BACKOFF_SECS,
            );
            tracing::debug!(account = %member.id, "usage poll rate limited; backing off");
            Ok(())
        }
        Err(error) => Err(error),
    }
}

async fn refresh_account_usage_inner(
    state: &AppState,
    member: &Arc<AccountMember>,
) -> Result<(), QuotaError> {
    match member.kind() {
        ProviderKind::Codex => refresh_codex_usage(state, member).await,
        ProviderKind::Antigravity => refresh_antigravity_usage(state, member).await,
        ProviderKind::Claude => refresh_claude_usage(state, member).await,
        ProviderKind::Cursor => {
            let url = member
                .upstream_override
                .as_deref()
                .map(|base| format!("{}/auth/usage-summary", base.trim_end_matches('/')))
                .unwrap_or_else(|| CURSOR_USAGE_URL.to_string());
            refresh_json_usage(member, &state.http_client, &url, parse_cursor_usage_summary).await
        }
        ProviderKind::Kiro => {
            let url = member
                .upstream_override
                .as_deref()
                .map(|base| format!("{}/getUsageLimits", base.trim_end_matches('/')))
                .unwrap_or_else(|| KIRO_USAGE_URL.to_string());
            refresh_json_usage(member, &state.http_client, &url, parse_kiro_usage_summary).await
        }
        ProviderKind::Zcode => refresh_zcode_usage(state, member).await,
        ProviderKind::Vertex => Err(QuotaError::Unsupported),
        ProviderKind::Generic => Err(QuotaError::Unsupported),
    }
}

/// ZCode quota comes from the ZCode desktop app itself: it polls its own
/// backend (`zcode.z.ai/api/v1/zcode-plan/billing/balance`) with a signed
/// client — replicating that auth is not feasible — and logs the full balance
/// JSON. Read the newest entry out of that log. No desktop app (no logs dir)
/// means this account cannot report quota at all: import itself requires the
/// app, so treat a missing dir as Unsupported and skip quietly.
async fn refresh_zcode_usage(
    state: &AppState,
    member: &Arc<AccountMember>,
) -> Result<(), QuotaError> {
    let _ = state;
    let now_unix = now_unix();
    let logs_dir = std::env::var("ZCODE_LOGS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join(".zcode/v2/logs")
        });
    if !logs_dir.is_dir() {
        return Err(QuotaError::Unsupported);
    }
    let scan = tokio::task::spawn_blocking(
        move || -> Result<Vec<crate::usage::ZcodeBalanceEntry>, QuotaError> {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&logs_dir)
            .map_err(|error| QuotaError::Upstream(format!(
                "ZCode desktop logs not readable at {}: {error} (is the ZCode desktop app installed?)",
                logs_dir.display()
            )))?
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "log"))
            .map(|entry| entry.path())
            .collect();
        entries.sort();
        let newest = entries
            .pop()
            .ok_or_else(|| QuotaError::Upstream("no ZCode desktop logs found".to_string()))?;
        let text = std::fs::read_to_string(&newest)
            .map_err(|error| QuotaError::Upstream(error.to_string()))?;
        crate::usage::extract_zcode_balances(&text)
            .ok_or_else(|| QuotaError::Upstream(
                "no plan balance entry found in ZCode desktop logs - open the ZCode desktop app once".to_string(),
            ))
    })
    .await
    .map_err(|error| QuotaError::Upstream(error.to_string()))?;
    scan.map(|balances| {
        let usage = crate::usage::zcode_balances_usage(&balances, now_unix);
        member.set_usage(usage);
    })
}

async fn refresh_json_usage(
    member: &Arc<AccountMember>,
    client: &reqwest::Client,
    url: &str,
    parser: fn(&serde_json::Value, i64) -> crate::usage::AccountUsage,
) -> Result<(), QuotaError> {
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let mut request = client.get(url);
    for (name, value) in member.build_upstream_headers() {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .map_err(|error| QuotaError::Upstream(error.to_string()))?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(QuotaError::Unauthorized);
    }
    if !status.is_success() {
        return Err(QuotaError::Upstream(format!(
            "quota endpoint returned {status}"
        )));
    }
    let body = response
        .json::<serde_json::Value>()
        .await
        .map_err(|error| QuotaError::Upstream(error.to_string()))?;
    let usage = parser(&body, now_unix);
    member.set_usage(usage.clone());
    Ok(())
}

/// Claude subscription quota, polled rather than scraped off relayed responses.
///
/// The relay only sees `anthropic-ratelimit-unified-*` when traffic actually
/// flows through this gateway, which leaves an idle account permanently blank;
/// this endpoint is what Claude Code's own usage display reads.
async fn refresh_claude_usage(
    state: &AppState,
    member: &Arc<AccountMember>,
) -> Result<(), QuotaError> {
    if state.auth_refresh_enabled && member.is_expired(now_unix()) {
        let _ = state.refresh_member(member, None).await;
    }
    let used = member.access_token();
    match try_claude_usage(state, member).await {
        Err(QuotaError::Unauthorized) if state.auth_refresh_enabled => {
            state
                .refresh_member(member, Some(&used))
                .await
                .map_err(|e| QuotaError::Upstream(format!("refresh failed: {e}")))?;
            try_claude_usage(state, member).await
        }
        other => other,
    }
}

async fn try_claude_usage(state: &AppState, member: &Arc<AccountMember>) -> Result<(), QuotaError> {
    // Relay deployments authenticate with a static key and publish cumulative
    // counters from /v1/usage/self instead of subscription windows. Gated on
    // the account's target API so generic API keys stay untouched.
    if let Some(key) = relay_usage_key(member) {
        // Poll the usage front door when one is pinned; chat still uses the
        // upstream_override target.
        let base = member
            .usage_override
            .as_deref()
            .or(member.upstream_override.as_deref())
            .unwrap_or_default()
            .trim_end_matches('/');
        if base.is_empty() {
            return Err(QuotaError::Upstream(
                "relay account has no upstream_override".into(),
            ));
        }
        let resp = state
            .http_client
            .get(format!("{base}/v1/usage/self"))
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .header("Accept", "application/json")
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| QuotaError::Upstream(e.to_string()))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(QuotaError::Unauthorized);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(QuotaError::RateLimited);
        }
        if !status.is_success() {
            return Err(QuotaError::Upstream(format!("usage http {status}")));
        }
        let payload: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| QuotaError::Upstream(e.to_string()))?;
        let totals = crate::usage::parse_relay_usage(&payload).ok_or_else(|| {
            QuotaError::Upstream("usage payload had no cumulative counters".into())
        })?;
        let now = now_unix();
        let samples = state.usage_samples.push(
            &member.id,
            crate::usage::UsageSample {
                unix: now,
                requests: totals.requests,
                tokens: totals.tokens,
                cost_usd: totals.total_cost_usd,
            },
        );
        member.set_usage(crate::usage::AccountUsage {
            plan_type: Some("relay".into()),
            totals: Some(totals),
            windows: crate::usage::window_deltas(&samples, now),
            ..crate::usage::AccountUsage::default()
        });
        return Ok(());
    }
    let token = member.access_token();
    if token.is_empty() {
        return Err(QuotaError::Unauthorized);
    }
    let base = member
        .upstream_override
        .as_deref()
        .unwrap_or(CLAUDE_API_BASE)
        .trim_end_matches('/');

    let resp = state
        .http_client
        .get(format!("{base}{CLAUDE_USAGE_PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", CLAUDE_OAUTH_BETA)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| QuotaError::Upstream(e.to_string()))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(QuotaError::Unauthorized);
    }
    // Anthropic throttles this endpoint hard and gives no Retry-After. A 429 is
    // a stale read, not a quota fact, so the previous snapshot is left alone
    // and the account backs off instead of keeping the throttle hot.
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(QuotaError::RateLimited);
    }
    if !status.is_success() {
        return Err(QuotaError::Upstream(format!("usage http {status}")));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| QuotaError::Upstream(e.to_string()))?;
    let parsed = crate::usage::parse_claude_usage_summary(&body, now_unix());
    if parsed.observed_at_unix.is_none() {
        return Err(QuotaError::Upstream("usage payload had no windows".into()));
    }
    member.set_usage(parsed);
    Ok(())
}

/// Antigravity's per-model-group quota.
///
/// Body is `{"project": <project_id>}`; the response carries a
/// `remainingFraction` per bucket which `parse_antigravity_quota_summary`
/// converts to the consumed orientation used everywhere else.
async fn refresh_antigravity_usage(
    state: &AppState,
    member: &Arc<AccountMember>,
) -> Result<(), QuotaError> {
    // Mirrors the relay's auth handling: refresh a known-expired token up
    // front, then retry once if the quota verb still answers 401.
    if state.auth_refresh_enabled && member.is_expired(now_unix()) {
        let _ = state.refresh_member(member, None).await;
    }
    // Capture the token actually used, so a 401 retry can present it and force
    // a refresh; `refresh_member(_, None)` is a no-op unless the clock already
    // says expired, which is exactly the case a 401 contradicts.
    let used = member.access_token();
    match try_antigravity_quota(state, member).await {
        Err(QuotaError::Unauthorized) if state.auth_refresh_enabled => {
            let refreshed = state
                .refresh_member(member, Some(&used))
                .await
                .map_err(|e| QuotaError::Upstream(format!("refresh failed: {e}")))?;
            tracing::debug!(account = %member.id, refreshed, "quota 401 retry");
            try_antigravity_quota(state, member).await
        }
        other => other,
    }
}

async fn try_antigravity_quota(
    state: &AppState,
    member: &Arc<AccountMember>,
) -> Result<(), QuotaError> {
    let token = member.access_token();
    if token.is_empty() {
        return Err(QuotaError::Unauthorized);
    }
    let project = {
        let guard = member
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match &*guard {
            crate::account::ProviderAccount::Antigravity(a) => a.project_id.clone(),
            _ => return Err(QuotaError::Unsupported),
        }
    };

    let url = mahoquot_providers::antigravity_quota_summary_url(
        mahoquot_providers::ANTIGRAVITY_UPSTREAM_BASE,
    );
    let resp = state
        .http_client
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", mahoquot_providers::ANTIGRAVITY_USER_AGENT)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({ "project": project }))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| QuotaError::Upstream(e.to_string()))?;

    let status = resp.status();
    if !status.is_success() {
        // Carry the upstream message: Antigravity distinguishes a throttle from
        // an unlicensed project only in the body, and both arrive as 4xx.
        let detail = resp.text().await.unwrap_or_default();
        let detail = detail.replace('\n', " ");
        let detail = detail.trim();
        // cloudcode-pa gates this verb on the Antigravity client User-Agent and
        // answers 403 "You do not have a valid license of this product" when it
        // is absent. That is a client-identification failure, not a credential
        // or licensing one, so it is surfaced separately from Unauthorized.
        if status == reqwest::StatusCode::FORBIDDEN && detail.contains("valid license") {
            return Err(QuotaError::Upstream("quota rejected client (403)".into()));
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            tracing::debug!(%status, detail = %&detail[..detail.len().min(240)], "antigravity quota rejected");
            return Err(QuotaError::Unauthorized);
        }
        return Err(QuotaError::Upstream(format!(
            "quota http {status}: {}",
            &detail[..detail.len().min(160)]
        )));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| QuotaError::Upstream(e.to_string()))?;
    let mut usage = crate::usage::parse_antigravity_quota_summary(
        &body,
        now_unix(),
    );

    // Fetch plan tier from loadCodeAssist using the same token
    let load_url = format!("{}/v1internal:loadCodeAssist", mahoquot_providers::ANTIGRAVITY_LOAD_BASE);
    if let Ok(load_resp) = state
        .http_client
        .post(&load_url)
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", mahoquot_providers::ANTIGRAVITY_USER_AGENT)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({}))
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        if load_resp.status().is_success() {
            if let Ok(load_data) = load_resp.json::<serde_json::Value>().await {
                let paid = load_data.get("paidTier");
                let tid = paid.and_then(|p| p.get("id")).and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
                let tname = paid.and_then(|p| p.get("name")).and_then(|v| v.as_str()).unwrap_or("");
                if tid.contains("ultra") || tname.to_lowercase().contains("ultra") {
                    usage.plan_type = Some("Ultra".to_string());
                } else if tid.contains("pro") || tname.to_lowercase().contains("pro") {
                    usage.plan_type = Some("Pro".to_string());
                } else if !tname.is_empty() {
                    usage.plan_type = Some(tname.to_string());
                } else {
                    let curr = load_data.get("currentTier");
                    let cid = curr.and_then(|c| c.get("id")).and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
                    let cname = curr.and_then(|c| c.get("name")).and_then(|v| v.as_str()).unwrap_or("");
                    if cid.contains("ultra") || cname.to_lowercase().contains("ultra") {
                        usage.plan_type = Some("Ultra".to_string());
                    } else if cid.contains("pro") || cname.to_lowercase().contains("pro") {
                        usage.plan_type = Some("Pro".to_string());
                    } else if cid == "free-tier" {
                        usage.plan_type = Some("Free".to_string());
                    }
                }
            }
        }
    }

    member.set_usage(usage);
    Ok(())
}

async fn refresh_codex_usage(
    state: &AppState,
    member: &Arc<AccountMember>,
) -> Result<(), QuotaError> {
    let token = member.access_token();
    if token.is_empty() {
        return Err(QuotaError::Unauthorized);
    }
    let account_id = codex_account_id(member);

    let usage_url = member
        .upstream_override
        .as_deref()
        .map(|base| format!("{}/backend-api/wham/usage", base.trim_end_matches('/')))
        .unwrap_or_else(|| CODEX_USAGE_URL.to_string());
    let mut req = state
        .http_client
        .get(usage_url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .header("User-Agent", CODEX_USER_AGENT)
        .timeout(Duration::from_secs(20));
    if !account_id.is_empty() {
        req = req.header("ChatGPT-Account-Id", &account_id);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| QuotaError::Upstream(e.to_string()))?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(QuotaError::Unauthorized);
    }
    if !status.is_success() {
        return Err(QuotaError::Upstream(format!("usage http {status}")));
    }
    let parsed: WhamUsage = resp
        .json()
        .await
        .map_err(|e| QuotaError::Upstream(e.to_string()))?;
    member.set_usage(parsed.into_account_usage(now_unix()));
    Ok(())
}

async fn try_consume_reset_credit(
    state: &AppState,
    member: &Arc<AccountMember>,
    redeem_id: &str,
) -> Result<(reqwest::StatusCode, String), QuotaError> {
    let token = member.access_token();
    if token.is_empty() {
        return Err(QuotaError::Unauthorized);
    }
    let account_id = codex_account_id(member);
    let reset_url = member
        .upstream_override
        .as_deref()
        .map(|base| {
            format!(
                "{}/backend-api/wham/rate-limit-reset-credits/consume",
                base.trim_end_matches('/')
            )
        })
        .unwrap_or_else(|| CODEX_RESET_URL.to_string());
    let mut req = state
        .http_client
        .post(reset_url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .header("User-Agent", CODEX_USER_AGENT)
        .timeout(Duration::from_secs(20));
    if !account_id.is_empty() {
        req = req.header("Chatgpt-Account-Id", &account_id);
    }
    let resp = req
        .json(&serde_json::json!({ "redeem_request_id": redeem_id }))
        .send()
        .await
        .map_err(|e| QuotaError::Network(e.to_string()))?;
    let status = resp.status();
    let body = if status.is_success() {
        String::new()
    } else {
        resp.text().await.unwrap_or_default()
    };
    Ok((status, body))
}

fn reset_upstream_error(status: reqwest::StatusCode, body: &str) -> QuotaError {
    let snippet: String = body.trim().chars().take(200).collect();
    QuotaError::Upstream(if snippet.is_empty() {
        format!("reset http {status}")
    } else {
        format!("reset http {status}: {snippet}")
    })
}

/// Spend one reset credit to force-clear the account's 5h window.
pub async fn consume_reset_credit(
    state: &AppState,
    member: &Arc<AccountMember>,
) -> Result<(), QuotaError> {
    if member.kind() != ProviderKind::Codex {
        return Err(QuotaError::Unsupported);
    }
    if member.usage_snapshot().reset_credits_available == Some(0) {
        return Err(QuotaError::NoCredit);
    }
    if member.access_token().is_empty() {
        return Err(QuotaError::Unauthorized);
    }
    if state.auth_refresh_enabled && member.is_expired(now_unix()) {
        state
            .refresh_member(member, None)
            .await
            .map_err(|e| QuotaError::Upstream(format!("reset token refresh failed: {e}")))?;
    }

    let redeem_id = uuid_v4();
    let used_token = member.access_token();
    let (status, body) = try_consume_reset_credit(state, member, &redeem_id).await?;
    match reset_attempt_policy(status, false, &body) {
        ResetAttemptPolicy::Success => {}
        ResetAttemptPolicy::RefreshAndRetry if state.auth_refresh_enabled => {
            state
                .refresh_member(member, Some(&used_token))
                .await
                .map_err(|e| QuotaError::Upstream(format!("reset token refresh failed: {e}")))?;
            let retry_id = retain_redeem_request_id(&redeem_id, &uuid_v4());
            let (retry_status, retry_body) =
                try_consume_reset_credit(state, member, &retry_id).await?;
            match reset_attempt_policy(retry_status, true, &retry_body) {
                ResetAttemptPolicy::Success => {}
                ResetAttemptPolicy::Unauthorized | ResetAttemptPolicy::RefreshAndRetry => {
                    return Err(QuotaError::Unauthorized)
                }
                ResetAttemptPolicy::NoCredit => return Err(QuotaError::NoCredit),
                ResetAttemptPolicy::Upstream => {
                    return Err(reset_upstream_error(retry_status, &retry_body))
                }
            }
        }
        ResetAttemptPolicy::RefreshAndRetry | ResetAttemptPolicy::Unauthorized => {
            return Err(QuotaError::Unauthorized)
        }
        ResetAttemptPolicy::NoCredit => return Err(QuotaError::NoCredit),
        ResetAttemptPolicy::Upstream => return Err(reset_upstream_error(status, &body)),
    }
    let _ = refresh_account_usage(state, member).await;
    Ok(())
}

pub fn reset_error_status(error: &QuotaError) -> reqwest::StatusCode {
    match error {
        QuotaError::Unsupported => reqwest::StatusCode::UNPROCESSABLE_ENTITY,
        QuotaError::Unauthorized => reqwest::StatusCode::UNAUTHORIZED,
        QuotaError::NoCredit => reqwest::StatusCode::CONFLICT,
        QuotaError::RateLimited => reqwest::StatusCode::TOO_MANY_REQUESTS,
        QuotaError::Network(_) | QuotaError::Upstream(_) => reqwest::StatusCode::BAD_GATEWAY,
    }
}

fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    getrandom(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

fn getrandom(buf: &mut [u8]) {
    // Nanosecond-seeded xorshift: the redeem id only needs to be unique per
    // request, not cryptographically strong.
    let mut s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E3779B97F4A7C15)
        | 1;
    for byte in buf.iter_mut() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        *byte = (s >> 24) as u8;
    }
}

pub async fn refresh_all_usage(state: &Arc<AppState>) {
    let mut tasks = Vec::new();
    for m in state.pool.load().members.clone() {
        let state = Arc::clone(state);
        tasks.push(tokio::spawn(async move {
            // Logged rather than discarded: a silently failing quota poll is
            // indistinguishable from a provider that reports no quota.
            match refresh_account_usage(&state, &m).await {
                Ok(()) | Err(QuotaError::Unsupported) => {}
                Err(e) => tracing::warn!(
                    account = %m.id,
                    provider = ?m.kind(),
                    error = %e,
                    "quota refresh failed"
                ),
            }
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
    // Persist the settled snapshots so the next gateway restart restores the
    // console cards immediately instead of waiting for the first poll cycle.
    let mut snapshots = std::collections::BTreeMap::new();
    for m in state.pool.load().members.iter() {
        snapshots.insert(m.id.clone(), m.usage_snapshot());
    }
    state.usage_state.save(&snapshots, now_unix());
}

pub fn spawn_usage_poller(state: Arc<AppState>, every: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(every);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            refresh_all_usage(&state).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redeem_ids_are_v4_shaped_and_unique() {
        let a = uuid_v4();
        let b = uuid_v4();
        assert_eq!(a.len(), 36);
        assert_eq!(a.as_bytes()[14], b'4');
        assert!(matches!(a.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
        assert_ne!(a, b);
    }

    #[test]
    fn usage_poll_backoff_gates_by_unix_time() {
        assert!(poll_allowed(100, None));
        assert!(poll_allowed(900, Some(900)));
        assert!(!poll_allowed(899, Some(900)));
    }

    #[test]
    fn relay_usage_polling_filters_on_the_target_api_not_the_key() {
        // then only nekos/ccapi relay targets unlock the usage/self path,
        // regardless of what the key material looks like
        assert_eq!(
            relay_usage_target(
                Some("sk-clb-anything".into()),
                Some("https://claude.nekos.me")
            )
            .as_deref(),
            Some("sk-clb-anything")
        );
        assert!(
            relay_usage_target(Some("k1".into()), Some("https://api.ccapi.example.com")).is_some()
        );
        // a generic API target never unlocks, whatever the key prefix
        assert_eq!(
            relay_usage_target(
                Some("nekos-prefixed-key".into()),
                Some("https://api.anthropic.com")
            ),
            None
        );
        // no target API or no static key means no polling either
        assert_eq!(relay_usage_target(Some("k".into()), None), None);
        assert_eq!(
            relay_usage_target(None, Some("https://claude.nekos.me")),
            None
        );
    }
}
