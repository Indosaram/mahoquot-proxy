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
pub const DAY_SECS: i64 = 86_400;

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
        // A window that has already lapsed must not be resurrected: seeding an
        // expired window would emit a stale display bucket until the next
        // rollover check, so it is dropped here instead.
        if window_start_unix > 0 && window_start_unix + DAY_SECS > now_unix() {
            if self.window_start_unix.load(Ordering::Relaxed) == 0 {
                self.window_start_unix
                    .store(window_start_unix, Ordering::Relaxed);
            }
            self.used_tokens.fetch_max(sum_tokens, Ordering::Relaxed);
        }
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

#[derive(Clone)]
pub struct ClineTrackers {
    glm: Arc<ClineDailyTracker>,
    deepseek: Arc<ClineDailyTracker>,
}

impl ClineTrackers {
    pub fn from_env() -> Self {
        Self {
            glm: ClineDailyTracker::with_config(budget_env("CLINE_GLM_DAILY_TOKEN_BUDGET")),
            deepseek: ClineDailyTracker::with_config(budget_env(
                "CLINE_DEEPSEEK_DAILY_TOKEN_BUDGET",
            )),
        }
    }

    pub fn with_budgets(glm_tokens: u64, deepseek_tokens: u64) -> Self {
        Self {
            glm: ClineDailyTracker::with_config(glm_tokens),
            deepseek: ClineDailyTracker::with_config(deepseek_tokens),
        }
    }

    pub fn for_model(&self, model: &str) -> Option<&ClineDailyTracker> {
        match cline_quota_lane(model)? {
            ClineQuotaLane::Glm => Some(&self.glm),
            ClineQuotaLane::Deepseek => Some(&self.deepseek),
        }
    }
}

pub enum ClineQuotaLane {
    Glm,
    Deepseek,
}

pub fn cline_quota_lane(model: &str) -> Option<ClineQuotaLane> {
    let bare = model.rsplit('/').next().unwrap_or(model);
    match bare {
        "glm-5.3-flash" => Some(ClineQuotaLane::Glm),
        "deepseek-v4.1-flash" => Some(ClineQuotaLane::Deepseek),
        _ => None,
    }
}

fn budget_env(specific: &str) -> u64 {
    std::env::var(specific)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .or_else(|| {
            std::env::var("CLINE_DAILY_TOKEN_BUDGET")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
        })
        .unwrap_or(DEFAULT_CLINE_DAILY_TOKEN_BUDGET)
}

/// Seed every Cline account's per-model trackers from the durable request history at
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
            let mut sums = [0u64; 2];
            let mut earliest = [0i64; 2];
            for row in &rows {
                let lane = match cline_quota_lane(&row.model) {
                    Some(ClineQuotaLane::Glm) => 0,
                    Some(ClineQuotaLane::Deepseek) => 1,
                    None => continue,
                };
                sums[lane] = sums[lane].saturating_add(row.total_tokens);
                let occurred = row.occurred_at_ms / 1000;
                if occurred > 0 && (earliest[lane] == 0 || occurred < earliest[lane]) {
                    earliest[lane] = occurred;
                }
            }
            let trackers = member.cline_trackers();
            trackers.glm.seed_from_history(sums[0], earliest[0]);
            trackers.deepseek.seed_from_history(sums[1], earliest[1]);
            emit_seed_buckets(&state, &member, trackers, now);
            restore_cap_buckets(&state, &member, now).await;
        }
    });
}

/// Push a seeded tracker's display estimate into the account's usage buckets so
/// the summary survives a restart without waiting for the next served request.
fn emit_seed_buckets(
    state: &std::sync::Arc<crate::state::AppState>,
    member: &Arc<crate::account::AccountMember>,
    trackers: &ClineTrackers,
    now: i64,
) {
    for (model, tracker) in [
        ("z-ai/glm-5.3-flash", &trackers.glm),
        ("cline-free/deepseek-v4.1-flash", &trackers.deepseek),
    ] {
        let reset_unix = tracker.estimated_reset_unix();
        if reset_unix <= now {
            continue;
        }
        let Some(percent) = tracker.used_percent() else {
            continue;
        };
        // 100% stays reserved for an upstream-confirmed cap, exactly like
        // the live request path.
        crate::relay::record_cline_quota_bucket(
            member,
            model,
            reset_unix - now,
            now,
            percent.min(99.9),
        );
    }
    let _ = state;
}

/// Re-apply persisted cap 429s whose reset is still in the future: the exact
/// upstream reset time is the one signal a restart must not lose.
async fn restore_cap_buckets(
    state: &std::sync::Arc<crate::state::AppState>,
    member: &Arc<crate::account::AccountMember>,
    now: i64,
) {
    let Ok(store) = state.history.store() else {
        return;
    };
    let account = member.id.clone();
    let caps = match store.cline_caps(&[account]) {
        Ok(caps) => caps,
        Err(_) => return,
    };
    for cap in caps {
        if cap.reset_at_unix <= now {
            continue;
        }
        let Some(tracker) = member.cline_trackers().for_model(&cap.model) else {
            continue;
        };
        tracker.on_cap_429(cap.reset_at_unix);
        crate::relay::record_cline_quota_bucket(
            member,
            &cap.model,
            cap.reset_at_unix - now,
            now,
            100.0,
        );
        member.set_group_cooldown(&cap.model, cap.reset_at_unix * 1000);
    }
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

    #[test]
    fn lanes_track_tokens_independently() {
        let set = ClineTrackers::with_budgets(1_000, 1_000);
        let glm = set.for_model("z-ai/glm-5.3-flash").expect("glm lane");
        let ds = set
            .for_model("cline-free/deepseek-v4.1-flash")
            .expect("deepseek lane");
        glm.observe(900, now_unix());
        assert_eq!(glm.used_percent(), Some(90.0), "glm lane absorbs only glm tokens");
        assert_eq!(ds.used_percent(), Some(0.0), "deepseek lane stays untouched");
        assert!(set.for_model("z-ai/glm-4.7").is_none(), "non-display models route nowhere");
        let capped = set
            .for_model("deepseek/deepseek-v4.1-flash")
            .expect("vendor-prefixed cap id");
        assert!(std::ptr::eq(capped, ds), "same lane under a vendor prefix");
    }

    #[test]
    fn seeded_windows_emit_display_buckets_until_reset() {
        let now = now_unix();
        let set = ClineTrackers::with_budgets(1_000, 1_000);
        let glm = set.for_model("z-ai/glm-5.3-flash").expect("glm lane");
        let ds = set
            .for_model("cline-free/deepseek-v4.1-flash")
            .expect("deepseek lane");
        glm.seed_from_history(500, now - 3_600);
        assert_eq!(glm.used_percent(), Some(50.0));
        assert_eq!(glm.estimated_reset_unix(), now - 3_600 + DAY_SECS);
        assert_eq!(ds.used_percent(), Some(0.0), "untouched lane emits nothing");
        let expired = ClineTrackers::with_budgets(1_000, 1_000);
        let old = expired.for_model("z-ai/glm-5.3-flash").expect("glm lane");
        old.seed_from_history(500, now - DAY_SECS - 3_600);
        assert_eq!(
            old.estimated_reset_unix(),
            0,
            "a lapsed window must not emit a bucket"
        );
    }
}
