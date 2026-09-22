//! Per-account Cline daily token budget tracking.
//!
//! Cline free accounts carry a rolling 24-hour token cap (~15.2M tokens per
//! account) that upstream only reveals as a hard `INFERENCE_CAP_ERROR` 429.
//! The gateway counts served tokens per account, opens the 24-hour window at
//! the first observed use (real traffic or a warmup probe), and estimates the
//! next reset as window start + 24h — reconciled to the exact upstream reset
//! the moment a cap 429 names it.
//!
//! This tracking is **display-only**. The budget is a local estimate of an
//! allowance the gateway does not own: it is not metered per model, upstream
//! never confirms it, and a request the gateway refuses on its own estimate is
//! capacity thrown away. Only upstream decides exhaustion, via the cap 429 that
//! benches the quota group it names.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Approximate daily free-token budget of one Cline account. A display
/// reference for the usage estimate only — never a routing gate.
pub const DEFAULT_CLINE_DAILY_TOKEN_BUDGET: u64 = 15_200_000;
const DAY_SECS: i64 = 86_400;

pub struct ClineDailyTracker {
    used_tokens: AtomicU64,
    /// Unix seconds the current 24h window opened; 0 = closed.
    window_start_unix: AtomicI64,
    /// Exact upstream-named reset from a cap 429; 0 = none.
    reset_override_unix: AtomicI64,
    seeded: AtomicBool,
    budget_tokens: AtomicU64,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl ClineDailyTracker {
    pub fn from_env() -> Arc<Self> {
        let budget = std::env::var("CLINE_DAILY_TOKEN_BUDGET")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_CLINE_DAILY_TOKEN_BUDGET);
        Self::with_config(budget)
    }

    pub fn with_config(budget_tokens: u64) -> Arc<Self> {
        Arc::new(Self {
            used_tokens: AtomicU64::new(0),
            window_start_unix: AtomicI64::new(0),
            reset_override_unix: AtomicI64::new(0),
            seeded: AtomicBool::new(false),
            budget_tokens: AtomicU64::new(budget_tokens),
        })
    }

    pub fn budget_tokens(&self) -> u64 {
        self.budget_tokens.load(Ordering::Relaxed)
    }

    pub fn used_tokens(&self) -> u64 {
        self.used_tokens.load(Ordering::Relaxed)
    }

    /// Reported usage fraction against the reference budget, clamped at 100%.
    /// An estimate for display; it gates nothing. `None` when no budget is set.
    pub fn used_percent(&self) -> Option<f64> {
        let budget = self.budget_tokens.load(Ordering::Relaxed);
        if budget == 0 {
            return None;
        }
        Some((self.used_tokens() as f64 * 100.0 / budget as f64).min(100.0))
    }

    /// The best-known reset: the upstream-named deadline while it is still
    /// ahead, otherwise window start + 24h. Zero when no window is open.
    pub fn estimated_reset_unix(&self) -> i64 {
        let now = now_unix();
        let over = self.reset_override_unix.load(Ordering::Relaxed);
        if over > now {
            return over;
        }
        let start = self.window_start_unix.load(Ordering::Relaxed);
        if start > 0 {
            start + DAY_SECS
        } else {
            0
        }
    }

    /// Clear the window once its 24h span (or an upstream reset) has lapsed.
    fn rollover_if_lapsed(&self, now_unix: i64) {
        let now = now_unix;
        let start = self.window_start_unix.load(Ordering::Relaxed);
        let over = self.reset_override_unix.load(Ordering::Relaxed);
        let reset = if over > now {
            over
        } else if start > 0 {
            start + DAY_SECS
        } else {
            return;
        };
        if now >= reset {
            self.used_tokens.store(0, Ordering::Relaxed);
            self.window_start_unix.store(0, Ordering::Relaxed);
            self.reset_override_unix.store(0, Ordering::Relaxed);
        }
    }

    /// Seed once per process from the durable request history: the token sum
    /// this account already served inside its current 24h window. Live
    /// accumulation is never rolled back by the seed (`fetch_max`).
    pub fn seed_from_history(&self, sum_tokens: u64, window_start_unix: i64) {
        if self.seeded.swap(true, Ordering::Relaxed) {
            return;
        }
        if self.window_start_unix.load(Ordering::Relaxed) == 0 && window_start_unix > 0 {
            self.window_start_unix
                .store(window_start_unix, Ordering::Relaxed);
        }
        self.used_tokens.fetch_max(sum_tokens, Ordering::Relaxed);
    }

    /// Anchor the window at `now` (warmup probe success) when it is closed.
    pub fn anchor_window(&self, now_unix: i64) {
        self.rollover_if_lapsed(now_unix);
        if self.window_start_unix.load(Ordering::Relaxed) == 0 && now_unix > 0 {
            self.window_start_unix.store(now_unix, Ordering::Relaxed);
        }
    }

    /// Record a served request's tokens into the display estimate. Accounting
    /// only: crossing the reference budget never benches the account, because
    /// the gateway does not own this allowance.
    pub fn observe(&self, tokens: u64, now_unix: i64) {
        self.rollover_if_lapsed(now_unix);
        if self.window_start_unix.load(Ordering::Relaxed) == 0 {
            self.window_start_unix.store(now_unix, Ordering::Relaxed);
        }
        self.used_tokens.fetch_add(tokens, Ordering::Relaxed);
    }

    /// Reconcile with an upstream cap 429: it names the exact reset time, so
    /// the estimate becomes exact and the budget counts as consumed.
    pub fn on_cap_429(&self, reset_at_unix: i64) {
        // A past reset carries no information about the current window.
        if reset_at_unix <= now_unix() {
            return;
        }
        self.reset_override_unix
            .store(reset_at_unix, Ordering::Relaxed);
        self.used_tokens
            .fetch_max(self.budget_tokens.load(Ordering::Relaxed), Ordering::Relaxed);
        if self.window_start_unix.load(Ordering::Relaxed) == 0 {
            self.window_start_unix
                .store(reset_at_unix - DAY_SECS, Ordering::Relaxed);
        }
    }
}

/// Seed every Cline account's tracker from the durable request history at
/// gateway startup, so a restart resumes from real served tokens instead of
/// restarting the budget from zero.
pub fn seed_cline_trackers_from_history(state: &std::sync::Arc<crate::state::AppState>) {
    let state = std::sync::Arc::clone(state);
    tokio::spawn(async move {
        let now = now_unix();
        let members: Vec<Arc<crate::account::AccountMember>> = state
            .pool
            .load()
            .members
            .iter()
            .filter(|m| m.provider_name() == "cline")
            .cloned()
            .collect();
        for member in members {
            let query = crate::request_history::HistoryQuery {
                accounts: vec![member.id.clone()],
                start_ms: Some((now - DAY_SECS) * 1000),
                end_ms: Some(now * 1000),
                ..Default::default()
            };
            let rows = match state.history.store() {
                Ok(store) => store.export(&query).unwrap_or_default(),
                Err(_) => continue,
            };
            if rows.is_empty() {
                member.cline_tracker().seed_from_history(0, 0);
                continue;
            }
            let sum: u64 = rows.iter().map(|row| row.total_tokens).sum();
            let earliest = rows
                .iter()
                .map(|row| row.occurred_at_ms)
                .min()
                .unwrap_or(0)
                / 1000;
            member.cline_tracker().seed_from_history(sum, earliest);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> Arc<ClineDailyTracker> {
        ClineDailyTracker::with_config(1000)
    }

    #[test]
    fn observe_accumulates_without_capping() {
        let now = now_unix();
        let t = tracker();
        t.observe(500, now);
        t.observe(400, now + 1);
        // Past the old 95% mark and past the budget itself: accounting only,
        // the tracker never signals the caller to bench.
        t.observe(600, now + 2);
        assert_eq!(t.used_tokens(), 1500);
        assert_eq!(t.estimated_reset_unix(), now + DAY_SECS);
        assert_eq!(t.used_percent(), Some(100.0), "display clamps at 100%");
    }

    #[test]
    fn window_rollover_resets_usage() {
        let now = now_unix();
        let t = tracker();
        t.observe(900, now);
        assert_eq!(t.estimated_reset_unix(), now + DAY_SECS);
        // 24h later: rollover clears, next observe opens a fresh window.
        t.observe(10, now + DAY_SECS + 5);
        assert_eq!(t.used_tokens(), 10);
        assert_eq!(t.estimated_reset_unix(), now + DAY_SECS + 5 + DAY_SECS);
    }

    #[test]
    fn cap_429_names_exact_reset_and_marks_budget_consumed() {
        let now = now_unix();
        let t = tracker();
        t.observe(100, now);
        let upstream_reset = now + 3600 * 13;
        t.on_cap_429(upstream_reset);
        assert_eq!(t.estimated_reset_unix(), upstream_reset);
        assert_eq!(t.used_tokens(), 1000, "budget counts as consumed");
        assert_eq!(t.used_percent(), Some(100.0));
    }

    #[test]
    fn cap_429_is_ignored_for_past_resets() {
        let now = now_unix();
        let t = tracker();
        t.observe(100, now);
        t.on_cap_429(now - 10);
        assert_eq!(t.estimated_reset_unix(), now + DAY_SECS);
        assert_eq!(t.used_tokens(), 100, "no false consumption");
    }

    #[test]
    fn seed_fills_history_without_rolling_back_live_traffic() {
        let now = now_unix();
        let t = tracker();
        t.observe(800, now);
        t.seed_from_history(300, now - 50);
        assert_eq!(t.used_tokens(), 800, "live traffic wins over the seed");
        assert_eq!(t.estimated_reset_unix(), now + DAY_SECS);
        t.seed_from_history(900, now - 50);
        assert_eq!(t.used_tokens(), 800, "second seed is ignored once seeded");
    }

    #[test]
    fn anchor_window_opens_closed_window_only() {
        let now = now_unix();
        let t = tracker();
        t.anchor_window(now);
        assert_eq!(t.estimated_reset_unix(), now + DAY_SECS);
        t.anchor_window(now + 100);
        assert_eq!(t.estimated_reset_unix(), now + DAY_SECS, "first anchor wins");
    }
}
