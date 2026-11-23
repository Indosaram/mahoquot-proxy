use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use tracing::{debug, info, warn};

#[cfg(test)]
use mahoquot_types::Health;

use crate::account::{AccountMember, ProviderKind};
use crate::state::AppState;

#[cfg(test)]
use mahoquot_providers::refresh_exec::RefreshError;

pub const DEFAULT_LEAD: Duration = Duration::from_secs(2 * 60 * 60);
pub const DEFAULT_SCAN_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Floor on the time between two rotations of the same credential.
///
/// "Within `lead` of expiry" is only a safe trigger when the access token
/// outlives the lead. Several issuers mint shorter tokens (an hour is common,
/// and a response without `expires_in` is persisted as one hour), and then the
/// condition stays true immediately after a successful rotation. Without this
/// floor the worker would rotate on every scan forever. That is not a harmless
/// retry loop: OAuth servers rotate the refresh token on each use and treat a
/// replayed ancestor as reuse, so a storm can revoke the whole grant family.
const MIN_ROTATION_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// How many credentials may be rotated at once.
///
/// Every rotation is an outbound token request; firing one per account the
/// instant a whole pool drifts into the lead window is the pattern identity
/// providers throttle. `warmup` bounds its own fan-out the same way.
const ROTATION_CONCURRENCY: usize = 4;

fn is_refreshable(member: &AccountMember) -> bool {
    // A terminal grant failure already benched the account. Retrying it on the
    // scan interval can never succeed and only risks the identity provider
    // throttling the client, so it waits for an operator to re-authenticate.
    !member.is_auth_failed()
        // The grant is turned over; replaying it can revoke the family. The
        // account still serves inference on its unexpired access token, so
        // this suppresses the refresh only, not routing.
        && !member.is_grant_rejected()
        // An operator-disabled account is out of service on purpose; rotating
        // its credential spends a request the rotation cannot pay back.
        && !member.is_manually_disabled()
        && member.relay_api_key().is_none()
        && member.kind() != ProviderKind::Devin
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Whether enough wall time has passed since this credential was last touched.
fn rotation_is_due(last_rotation: Option<Instant>, now: Instant, min_interval: Duration) -> bool {
    match last_rotation {
        Some(at) => now.saturating_duration_since(at) >= min_interval,
        None => true,
    }
}

/// Whether a failed refresh proves the grant itself is dead.
///
/// The classification is shared with the serving path
/// (`RefreshError::is_grant_rejection`) so both agree on what evidence counts.
/// A 403 can be a WAF or proxy rejection for a perfectly valid grant, and
/// benching a healthy account from a background scan removes capacity until the
/// next restart. The observed terminal rejection for a rotated-away refresh
/// token is 400 `invalid_grant`; 401 covers a credential the issuer no longer
/// honours.
pub async fn refresh_expiring(
    state: &Arc<AppState>,
    lead: Duration,
    last_rotation: &mut HashMap<String, Instant>,
) -> usize {
    if !state.auth_refresh_enabled {
        return 0;
    }
    let horizon = now_unix() + lead.as_secs() as i64;
    let now = Instant::now();
    let mut due = Vec::new();
    for member in state.pool.load().members.clone() {
        if !is_refreshable(&member) || !member.is_expired(horizon) {
            continue;
        }
        if !rotation_is_due(
            last_rotation.get(&member.id).copied(),
            now,
            MIN_ROTATION_INTERVAL,
        ) {
            continue;
        }
        due.push(member);
    }
    let rotations = futures::stream::iter(due.into_iter().map(|member| {
        let state = Arc::clone(state);
        async move {
            let presented = member.access_token();
            // Passing the current token selects refresh()'s stale-token branch,
            // which skips its `is_expired` gate. With `None` a refresh before
            // expiry is a no-op, so this argument is what makes pre-expiry work.
            match state.refresh_member(&member, Some(&presented)).await {
                Ok(true) => {
                    info!(
                        account = %member.id,
                        provider = ?member.kind(),
                        lead_secs = lead.as_secs(),
                        "refreshed credential before expiry"
                    );
                    Some(member.id.clone())
                }
                Ok(false) => None,
                Err(error) => {
                    if error.is_grant_rejection() {
                        // Terminal for this grant, not for the request path:
                        // the access token is still valid until it expires, so
                        // only the replay stops. Benching here removed working
                        // capacity up to two hours early.
                        member.mark_grant_rejected();
                        warn!(
                            account = %member.id,
                            provider = ?member.kind(),
                            error = %error,
                            "pre-expiry refresh rejected by the identity provider"
                        );
                    } else {
                        // Transient (network, 429, upstream 5xx, WAF 403): the
                        // credential is untouched, so leave health alone and let
                        // the serving path decide if it really is unusable.
                        debug!(
                            account = %member.id,
                            provider = ?member.kind(),
                            error = %error,
                            "pre-expiry refresh failed transiently"
                        );
                    }
                    None
                }
            }
        }
    }))
    .buffer_unordered(ROTATION_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    let mut refreshed = 0;
    for id in rotations.into_iter().flatten() {
        // Recorded only on success: a failed attempt changed nothing about the
        // credential, so it must stay free to retry on the next scan.
        last_rotation.insert(id, Instant::now());
        refreshed += 1;
    }
    refreshed
}

fn env_secs(key: &str, fallback: Duration) -> Duration {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_secs)
        .unwrap_or(fallback)
}

pub fn spawn_from_env(state: Arc<AppState>) -> PreExpiryRefreshHandle {
    spawn_pre_expiry_refresher(
        state,
        env_secs("AUTH_REFRESH_SCAN_SECS", DEFAULT_SCAN_INTERVAL),
        env_secs("AUTH_REFRESH_LEAD_SECS", DEFAULT_LEAD),
    )
}

pub struct PreExpiryRefreshHandle {
    stop: Arc<tokio::sync::Notify>,
    join: tokio::task::JoinHandle<()>,
}

impl PreExpiryRefreshHandle {
    pub async fn shutdown(self) {
        self.stop.notify_one();
        self.join.abort();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.join).await;
    }
}

pub fn spawn_pre_expiry_refresher(
    state: Arc<AppState>,
    every: Duration,
    lead: Duration,
) -> PreExpiryRefreshHandle {
    let stop = Arc::new(tokio::sync::Notify::new());
    let stop_signal = Arc::clone(&stop);
    let join = tokio::spawn(async move {
        let mut last_rotation: HashMap<String, Instant> = HashMap::new();
        loop {
            tokio::select! {
                _ = refresh_expiring(&state, lead, &mut last_rotation) => {}
                _ = stop_signal.notified() => break,
            }
            tokio::select! {
                _ = tokio::time::sleep(every) => {}
                _ = stop_signal.notified() => break,
            }
        }
    });
    PreExpiryRefreshHandle { stop, join }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::ProviderAccount;
    use mahoquot_providers::CodexAccount;

    #[test]
    fn lead_leaves_usable_life_after_rotation() {
        let lead = DEFAULT_LEAD.as_secs();
        assert!(lead >= 3600, "lead under an hour cannot beat expiry jitter");
        assert!(lead < 8 * 3600, "lead must not exceed an 8h access token");
    }

    #[test]
    fn scan_interval_cannot_cross_the_whole_lead() {
        assert!(DEFAULT_SCAN_INTERVAL < DEFAULT_LEAD / 4);
    }

    #[test]
    fn a_short_lived_token_cannot_be_rotated_every_scan() {
        let now = Instant::now();
        assert!(rotation_is_due(None, now, MIN_ROTATION_INTERVAL));
        assert!(!rotation_is_due(Some(now), now, MIN_ROTATION_INTERVAL));
        // A token that expires inside the lead leaves the trigger pinned on
        // right after a rotation; only the floor stops the next scan.
        assert!(!rotation_is_due(
            Some(now - Duration::from_secs(60)),
            now,
            MIN_ROTATION_INTERVAL
        ));
        assert!(rotation_is_due(
            Some(now - MIN_ROTATION_INTERVAL),
            now,
            MIN_ROTATION_INTERVAL
        ));
    }

    #[test]
    fn only_a_rejected_grant_benches_the_account() {
        let invalid_grant = RefreshError::Status {
            code: 400,
            body: r#"{"error":"invalid_grant"}"#.to_string(),
        };
        assert!(invalid_grant.is_grant_rejection());

        let revoked = RefreshError::Status {
            code: 401,
            body: String::new(),
        };
        assert!(revoked.is_grant_rejection());

        // A WAF or egress proxy can answer 403 for a grant that is still valid;
        // a background scan must not bench the account over it.
        let waf = RefreshError::Status {
            code: 403,
            body: "blocked".to_string(),
        };
        assert!(!waf.is_grant_rejection());
        assert!(!waf.is_auth_failure());

        // A payment failure (402) or a non-grant 400 says nothing about the
        // stored credential.
        let payment = RefreshError::Status {
            code: 402,
            body: String::new(),
        };
        assert!(!payment.is_grant_rejection());
        assert!(!payment.is_auth_failure());
        let malformed = RefreshError::Status {
            code: 400,
            body: "<html>bad request</html>".to_string(),
        };
        assert!(!malformed.is_auth_failure());

        let throttled = RefreshError::Status {
            code: 429,
            body: String::new(),
        };
        assert!(!throttled.is_grant_rejection());
        assert!(!throttled.is_auth_failure());

        let transport = RefreshError::Io(std::io::Error::other("reset"));
        assert!(!transport.is_grant_rejection());
        assert!(!transport.is_auth_failure());
    }

    #[test]
    fn a_rejected_grant_keeps_the_working_access_token_routable() {
        let member = AccountMember::for_test(ProviderAccount::Codex(CodexAccount {
            email: "test@example.com".to_string(),
            ..Default::default()
        }));
        assert!(is_refreshable(&member));

        // The issuer rejected the grant, but the access token has not expired.
        member.mark_grant_rejected();
        assert!(member.is_grant_rejected());
        assert!(!is_refreshable(&member), "the dead grant is not replayed");
        assert!(
            !member.is_auth_failed(),
            "the account still serves inference"
        );

        member.clear_grant_rejection();
        assert!(is_refreshable(&member));
    }

    #[test]
    fn an_operator_disabled_account_is_not_rotated() {
        let member = AccountMember::for_test(ProviderAccount::Codex(CodexAccount {
            email: "test@example.com".to_string(),
            ..Default::default()
        }));
        assert!(is_refreshable(&member));
        member.set_health(Health::Disabled);
        assert!(!is_refreshable(&member));
    }

    #[test]
    fn a_dead_grant_is_left_for_re_authentication() {
        let member = AccountMember::for_test(ProviderAccount::Codex(CodexAccount {
            email: "test@example.com".to_string(),
            ..Default::default()
        }));
        assert!(is_refreshable(&member), "a healthy credential is scanned");

        // A re-authenticated account is published with fresh health, and a
        // terminal failure is still skipped by the scan.
        member.set_health(Health::AuthFailed);
        assert!(!is_refreshable(&member));
    }
}
