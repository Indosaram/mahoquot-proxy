//! Classification of Claude upstream denials.
//!
//! A 403 used to be retried on the next account, which spreads an abuse or
//! policy signal across the whole pool. Classification decides, per answer,
//! whether another account may see the request at all.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

/// Phrases Anthropic uses when an account or organization is restricted.
/// Matched case-insensitively on the upstream message text.
const ENFORCEMENT_SIGNATURES: &[&str] = &[
    "only authorized for use with claude code",
    "has been disabled",
    "suspended",
    "terms of service",
    "usage policy",
    "organization has been",
];

/// A second plain denial on the same account inside this window is treated
/// as enforcement.
pub const DENIED_PROMOTION_WINDOW_SECS: i64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    /// Account- or org-level enforcement: bench the account, never fail over.
    Enforcement,
    /// Any other 403 (including `permission_error` and a first
    /// `Request not allowed`): return it, never fail over.
    Denied,
    /// Not a Claude denial this module owns; existing handling applies.
    Other,
}

/// Classify a Claude upstream answer. `message` is the response body for
/// HTTP failures, or the upstream message carried by an early SSE error.
pub fn classify(status: u16, message: &str) -> Denial {
    if matches!(status, 400 | 401 | 403) && is_enforcement_text(message) {
        return Denial::Enforcement;
    }
    if status == 403 {
        return Denial::Denied;
    }
    Denial::Other
}

pub fn is_enforcement_text(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    ENFORCEMENT_SIGNATURES
        .iter()
        .any(|signature| lowered.contains(signature))
}

/// Short reason stored with a suspected account: the matched signature, so
/// the persisted record never carries request content.
pub fn enforcement_reason(status: u16, message: &str) -> String {
    let lowered = message.to_ascii_lowercase();
    let signature = ENFORCEMENT_SIGNATURES
        .iter()
        .find(|signature| lowered.contains(*signature))
        .copied()
        .unwrap_or("repeated permission denial");
    format!("{status}: {signature}")
}

/// Fail-closed quarantine, including accounts whose settings write failed.
#[derive(Debug, Default)]
pub struct QuarantineSet {
    inner: Mutex<BTreeSet<String>>,
}

impl QuarantineSet {
    pub fn insert(&self, account: &str) {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(account.to_string());
    }

    pub fn contains(&self, account: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(account)
    }

    pub fn remove(&self, account: &str) {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(account);
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Per-account timestamps of the last plain `Denied`, used to promote a
/// repeat denial to enforcement.
#[derive(Debug, Default)]
pub struct DeniedTracker {
    last: Mutex<HashMap<String, i64>>,
}

impl DeniedTracker {
    /// Record a plain denial at `now_unix`. Returns true when it is the second
    /// one inside the promotion window, i.e. it must be handled as enforcement.
    pub fn record(&self, account: &str, now_unix: i64) -> bool {
        let mut last = self.last.lock().unwrap_or_else(|p| p.into_inner());
        let repeat = last
            .get(account)
            .is_some_and(|at| now_unix - *at <= DENIED_PROMOTION_WINDOW_SECS);
        if repeat {
            last.remove(account);
        } else {
            last.insert(account.to_string(), now_unix);
        }
        repeat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarantine_tracks_and_clears_accounts() {
        let set = QuarantineSet::default();
        assert!(set.is_empty());
        set.insert("claude-a");
        assert!(set.contains("claude-a"));
        assert!(!set.contains("claude-b"));
        assert_eq!(set.len(), 1);
        set.remove("claude-a");
        assert!(set.is_empty());
    }

    #[test]
    fn quarantine_is_idempotent() {
        let set = QuarantineSet::default();
        set.insert("claude-a");
        set.insert("claude-a");
        assert_eq!(set.len(), 1);
    }


    #[test]
    fn recorded_bodies_land_in_the_expected_class() {
        let cases: &[(u16, &str, Denial)] = &[
            (
                400,
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"This credential is only authorized for use with Claude Code and cannot be used for other API requests."}}"#,
                Denial::Enforcement,
            ),
            (
                403,
                r#"{"error":{"type":"permission_error","message":"Your account has been disabled after an automatic review of your recent activities."}}"#,
                Denial::Enforcement,
            ),
            (
                401,
                r#"{"error":{"type":"authentication_error","message":"This organization has been disabled."}}"#,
                Denial::Enforcement,
            ),
            (
                403,
                r#"{"error":{"type":"permission_error","message":"Account suspended"}}"#,
                Denial::Enforcement,
            ),
            // The room's C55 case: a permission_denied that was not a ban.
            (
                403,
                r#"{"type":"error","error":{"type":"permission_error","message":"permission_denied"}}"#,
                Denial::Denied,
            ),
            (403, "Request not allowed", Denial::Denied),
            (
                401,
                r#"{"error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
                Denial::Other,
            ),
            (
                400,
                r#"{"error":{"type":"invalid_request_error","message":"max_tokens too large"}}"#,
                Denial::Other,
            ),
            (529, "Overloaded", Denial::Other),
            (500, "has been disabled", Denial::Other),
        ];
        for (status, body, expected) in cases {
            assert_eq!(classify(*status, body), *expected, "{status} {body}");
        }
    }

    #[test]
    fn a_second_denial_inside_the_window_promotes() {
        let tracker = DeniedTracker::default();
        assert!(!tracker.record("a", 1_000));
        assert!(!tracker.record("b", 1_000), "accounts are tracked separately");
        assert!(tracker.record("a", 1_000 + DENIED_PROMOTION_WINDOW_SECS));
        assert!(!tracker.record("a", 5_000), "a promotion resets the counter");
        assert!(!tracker.record("a", 5_000 + DENIED_PROMOTION_WINDOW_SECS + 1));
    }

    #[test]
    fn the_reason_names_the_signature_not_the_body() {
        let reason = enforcement_reason(
            403,
            "secret prompt text ... account has been disabled ... more text",
        );
        assert_eq!(reason, "403: has been disabled");
    }
}
