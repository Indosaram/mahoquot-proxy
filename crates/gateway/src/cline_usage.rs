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

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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

/// The display key of a Cline model: its bare slug after the vendor prefix.
/// Bucket ids, tracker keys, and every display filter compare on this, so a
/// routed alias (`z-ai/...`) and the upstream id (`cline-free/...`) name the
/// same quota.
pub fn cline_quota_slug(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// Which reference budget a slug's tracker starts from. The two historical
/// env knobs keep their names (both already fall back to the account-wide
/// budget); every model Cline ships tomorrow shares that default. Evaluated
/// once, when the tracker is first created.
fn budget_for_slug(slug: &str) -> u64 {
    match slug {
        "gemini-3.8-flash" => budget_env("CLINE_GLM_DAILY_TOKEN_BUDGET"),
        "deepseek-v4.1-flash" => budget_env("CLINE_DEEPSEEK_DAILY_TOKEN_BUDGET"),
        _ => budget_env("CLINE_DAILY_TOKEN_BUDGET"),
    }
}

/// Rolling 24h budget trackers keyed by bare model slug, created on first
/// touch: a free model Cline ships tomorrow needs no code change here. Which
/// of these buckets a surface renders is a display setting on the frontend;
/// the gateway tracks and records every Cline model it serves.
#[derive(Clone)]
pub struct ClineTrackers {
    trackers: Arc<Mutex<HashMap<String, Arc<ClineDailyTracker>>>>,
}

impl ClineTrackers {
    pub fn from_env() -> Self {
        Self {
            trackers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Pre-seeds the two historical budgets (tests and explicit tuning).
    pub fn with_budgets(glm_tokens: u64, deepseek_tokens: u64) -> Self {
        let this = Self::from_env();
        this.insert("gemini-3.8-flash", glm_tokens);
        this.insert("deepseek-v4.1-flash", deepseek_tokens);
        this
    }

    fn insert(&self, slug: &str, budget: u64) {
        self.trackers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(slug.to_owned(), ClineDailyTracker::with_config(budget));
    }

    /// The tracker for a model, minted on first touch. Callers sit behind the
    /// Cline provider gate; pure routing reads use [`Self::existing`] instead
    /// so they never mint state.
    pub fn for_model(&self, model: &str) -> Arc<ClineDailyTracker> {
        let slug = cline_quota_slug(model).to_owned();
        let budget = budget_for_slug(&slug);
        self.trackers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(slug)
            .or_insert_with(|| ClineDailyTracker::with_config(budget))
            .clone()
    }

    /// Read-only lookup for a tracker that already exists.
    pub fn existing(&self, model: &str) -> Option<Arc<ClineDailyTracker>> {
        self.trackers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(cline_quota_slug(model))
            .cloned()
    }

    /// Snapshot for iteration, taken without holding the lock across the
    /// recording calls that follow.
    pub fn entries(&self) -> Vec<(String, Arc<ClineDailyTracker>)> {
        self.trackers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|(slug, tracker)| (slug.clone(), tracker.clone()))
            .collect()
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
            let mut sums: HashMap<String, u64> = HashMap::new();
            let mut earliest: HashMap<String, i64> = HashMap::new();
            for row in &rows {
                let slug = cline_quota_slug(&row.model).to_owned();
                *sums.entry(slug.clone()).or_default() += row.total_tokens;
                let occurred = row.occurred_at_ms / 1000;
                if occurred > 0 {
                    let entry = earliest.entry(slug).or_insert(0);
                    if *entry == 0 || occurred < *entry {
                        *entry = occurred;
                    }
                }
            }
            let trackers = member.cline_trackers();
            for (slug, sum) in &sums {
                trackers
                    .for_model(slug)
                    .seed_from_history(*sum, earliest.get(slug).copied().unwrap_or(0));
            }
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
    for (slug, tracker) in trackers.entries() {
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
            &format!("cline-free/{slug}"),
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
        // Bench first, and unconditionally. Routability must be restored for
        // every persisted cap — gating the cooldown behind tracker existence
        // once let a cline cap come back from a restart with no bench at all,
        // and the account was re-selected seconds later.
        member.set_group_cooldown(&cap.model, cap.reset_at_unix * 1000);
        // Restore the *reason* alongside the deadline. A restart otherwise
        // sees only when the account is blocked, and the exhaustion response
        // would keep calling an emptied credit balance "daily-exhausted".
        if cap.credit_driven {
            member.set_group_credit_bench(&cap.model, cap.reset_at_unix * 1000);
        }
        member.cline_trackers()
            .for_model(&cap.model)
            .on_cap_429(cap.reset_at_unix);
        crate::relay::record_cline_quota_bucket(
            member,
            &cap.model,
            cap.reset_at_unix - now,
            now,
            100.0,
        );
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

    #[tokio::test]
    async fn a_cap_on_any_model_still_benches_after_restore() {
        // The bench must not sit behind tracker existence: `deepseek-v4-flash`
        // has no traffic history at all, but its cap still has to come back
        // from a restart or the account is routable again seconds after boot.
        use std::sync::Arc;
        let now = now_unix();
        let auth_dir = std::env::temp_dir().join(format!(
            "mahoquot-restore-unlaned-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&auth_dir).expect("create auth dir");
        let cred = serde_json::json!({
            "type": "generic",
            "identity_slug": "cline-account",
            "provider": "cline",
            "label": "Cline",
            "adapter": "openai-chat",
            "base_url": "http://127.0.0.1:9",
            "api_key": "fixture-cline",
        })
        .to_string();
        std::fs::write(auth_dir.join("generic-cline.json"), cred).expect("write cline");
        let config = crate::config::GatewayConfig {
            auth_dir: auth_dir.clone(),
            config_path: auth_dir.join("config.yaml"),
            auth_refresh_enabled: false,
            ..crate::config::GatewayConfig::default()
        };
        let state = Arc::new(crate::state::AppState::new(&config).expect("state"));
        let pool = state.pool.load_full();
        let member = pool.members[0].clone();

        // Seed the durable cap exactly as the live path records it.
        let store = state.history.store().expect("history store");
        store
            .record_cline_cap(&crate::request_history::ClineCapEvent {
                account_identifier: member.id.clone(),
                model: "deepseek/deepseek-v4-flash".to_string(),
                cap_at_ms: now * 1000,
                reset_at_unix: now + 3_600,
                credit_driven: false,
            })
            .expect("record cap");

        restore_cap_buckets(&state, &member, now).await;

        assert!(
            !member.group_available("deepseek/deepseek-v4-flash", now * 1000),
            "an unlaned cap must still bench its group across a restart"
        );
        assert!(
            !member.group_credit_benched("deepseek/deepseek-v4-flash", now * 1000),
            "a timed-quota cap must not be relabelled as a credit block on restore"
        );
        // An unrelated sibling model is unaffected.
        assert!(member.group_available("z-ai/glm-5.3-flash", now * 1000));
    }

    #[tokio::test]
    async fn restore_relabels_a_cap_that_was_benched_for_credits() {
        let now = now_unix();
        let auth_dir = std::env::temp_dir().join(format!(
            "mahoquot-restore-credit-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&auth_dir).expect("create auth dir");
        let cred = serde_json::json!({
            "type": "generic",
            "identity_slug": "cline-account",
            "provider": "cline",
            "label": "Cline",
            "adapter": "openai-chat",
            "base_url": "http://127.0.0.1:9",
            "api_key": "fixture-cline",
        })
        .to_string();
        std::fs::write(auth_dir.join("generic-cline.json"), cred).expect("write cline");
        let config = crate::config::GatewayConfig {
            auth_dir: auth_dir.clone(),
            config_path: auth_dir.join("config.yaml"),
            auth_refresh_enabled: false,
            ..crate::config::GatewayConfig::default()
        };
        let state = Arc::new(crate::state::AppState::new(&config).expect("state"));
        let pool = state.pool.load_full();
        let member = pool.members[0].clone();

        let store = state.history.store().expect("history store");
        store
            .record_cline_cap(&crate::request_history::ClineCapEvent {
                account_identifier: member.id.clone(),
                model: "z-ai/glm-5.3-flash".to_string(),
                cap_at_ms: now * 1000,
                reset_at_unix: now + 300,
                credit_driven: true,
            })
            .expect("record cap");

        restore_cap_buckets(&state, &member, now).await;

        assert!(
            !member.group_available("z-ai/glm-5.3-flash", now * 1000),
            "the deadline must survive the restart"
        );
        assert!(
            member.group_credit_benched("z-ai/glm-5.3-flash", now * 1000),
            "an emptied balance must come back labelled, so the 503 can ask for credits \
             instead of claiming a daily reset that will never arrive"
        );
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
    fn per_model_trackers_stay_independently_budgeted() {
        let set = ClineTrackers::with_budgets(1_000, 1_000);
        let gemini = set.for_model("cline-free/gemini-3.8-flash");
        let ds = set.for_model("cline-free/deepseek-v4.1-flash");
        gemini.observe(900, now_unix());
        assert_eq!(
            gemini.used_percent(),
            Some(90.0),
            "gemini tracker absorbs only gemini tokens"
        );
        assert_eq!(ds.used_percent(), Some(0.0), "deepseek tracker stays untouched");
        let capped = set.for_model("deepseek/deepseek-v4.1-flash");
        assert!(
            Arc::ptr_eq(&capped, &ds),
            "same tracker under a vendor prefix"
        );
        let other = set.for_model("z-ai/glm-4.7");
        assert!(
            !Arc::ptr_eq(&other, &gemini),
            "an unlisted model tracks on its own slug, never onto a display lane"
        );
        other.observe(DEFAULT_CLINE_DAILY_TOKEN_BUDGET / 10, now_unix());
        assert_eq!(other.used_percent(), Some(10.0), "default budget applies");
    }

    #[test]
    fn seeded_windows_emit_buckets_until_reset() {
        let now = now_unix();
        let set = ClineTrackers::with_budgets(1_000, 1_000);
        let gemini = set.for_model("cline-free/gemini-3.8-flash");
        let ds = set.for_model("cline-free/deepseek-v4.1-flash");
        gemini.seed_from_history(500, now - 3_600);
        assert_eq!(gemini.used_percent(), Some(50.0));
        assert_eq!(gemini.estimated_reset_unix(), now - 3_600 + DAY_SECS);
        assert_eq!(ds.used_percent(), Some(0.0), "untouched lane emits nothing");
        let expired = ClineTrackers::with_budgets(1_000, 1_000);
        let old = expired.for_model("cline-free/gemini-3.8-flash");
        old.seed_from_history(500, now - DAY_SECS - 3_600);
        assert_eq!(
            old.estimated_reset_unix(),
            0,
            "a lapsed window must not emit a bucket"
        );
    }
}
