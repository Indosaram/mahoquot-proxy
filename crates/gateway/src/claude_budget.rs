//! Monthly budgets for Claude API-key accounts (W0).
//!
//! Usage is read from request history off the request path and cached; the
//! relay only reads the cached snapshot. When history cannot be read, every
//! budgeted account counts as over budget (fail closed): an unread budget must
//! never turn into unbounded API spend.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use arc_swap::ArcSwap;

pub const REFRESH_EVERY_SECS: u64 = 60;

/// input + output + cache writes count fully, cache reads at a tenth.
pub fn weighted_units(input: u64, output: u64, cache_write: u64, cache_read: u64) -> u64 {
    input
        .saturating_add(output)
        .saturating_add(cache_write)
        .saturating_add(cache_read / 10)
}

/// First instant of the calendar month (UTC) containing `now_unix`, in ms.
pub fn month_start_utc_ms(now_unix: i64) -> i64 {
    let days = now_unix.div_euclid(86_400);
    let (year, month, _) = civil_from_days(days);
    days_from_civil(year, month, 1) * 86_400 * 1000
}

// Howard Hinnant's civil-calendar conversions (proleptic Gregorian).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[derive(Debug, Clone, Default)]
struct Snapshot {
    /// None until the first successful read, and after any failed one.
    usage: Option<HashMap<String, u64>>,
}

#[derive(Debug, Default)]
pub struct BudgetTracker {
    snapshot: ArcSwap<Snapshot>,
}

impl BudgetTracker {
    /// True when `account` has a budget and must not be served: usage at or
    /// over the limit, or usage unknown.
    pub fn is_over(&self, budgets: &BTreeMap<String, u64>, account: &str) -> bool {
        let Some(limit) = budgets.get(account) else {
            return false;
        };
        let snapshot = self.snapshot.load();
        match snapshot.usage.as_ref() {
            Some(usage) => usage.get(account).copied().unwrap_or(0) >= *limit,
            None => true,
        }
    }

    pub fn usage(&self, account: &str) -> Option<u64> {
        self.snapshot
            .load()
            .usage
            .as_ref()
            .map(|usage| usage.get(account).copied().unwrap_or(0))
    }

    /// Replace the snapshot with the result of `read`, which returns weighted
    /// units for one account or an error. Any error makes the snapshot unknown.
    pub fn refresh_with<E: std::fmt::Display>(
        &self,
        budgets: &BTreeMap<String, u64>,
        mut read: impl FnMut(&str) -> Result<u64, E>,
    ) {
        let mut usage = HashMap::with_capacity(budgets.len());
        for account in budgets.keys() {
            match read(account) {
                Ok(units) => {
                    usage.insert(account.clone(), units);
                }
                Err(error) => {
                    tracing::warn!(
                        account = %account,
                        %error,
                        "claude api-key budget unreadable; account treated as over budget"
                    );
                    self.snapshot.store(Arc::new(Snapshot { usage: None }));
                    return;
                }
            }
        }
        self.snapshot.store(Arc::new(Snapshot { usage: Some(usage) }));
    }

    pub fn refresh_from_history(&self, state: &crate::state::AppState, now_unix: i64) {
        let budgets = state.settings.current().claude_api_key_budgets.clone();
        if budgets.is_empty() {
            self.snapshot.store(Arc::new(Snapshot {
                usage: Some(HashMap::new()),
            }));
            return;
        }
        let start_ms = month_start_utc_ms(now_unix);
        self.refresh_with(&budgets, |account| {
            let store = state.history.store()?;
            let query = crate::request_history::HistoryQuery {
                start_ms: Some(start_ms),
                accounts: vec![account.to_string()],
                providers: vec!["claude".to_string()],
                ..crate::request_history::HistoryQuery::default()
            };
            let totals = store.query(&query)?.totals;
            Ok::<u64, crate::request_history::HistoryError>(weighted_units(
                totals.input_tokens,
                totals.output_tokens,
                totals.cache_write_tokens,
                totals.cached_input_tokens,
            ))
        });
    }

    pub fn spawn_refresher(state: Arc<crate::state::AppState>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker =
                tokio::time::interval(std::time::Duration::from_secs(REFRESH_EVERY_SECS));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = state.shutdown.notified() => break,
                    _ = ticker.tick() => {}
                }
                let worker = Arc::clone(&state);
                let _ = tokio::task::spawn_blocking(move || {
                    let now_unix = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    worker.claude_budgets.refresh_from_history(&worker, now_unix);
                })
                .await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_start_handles_first_and_last_second() {
        // 2026-10-01T00:00:00Z and 2026-10-31T23:59:59Z
        let october = 1_790_812_800;
        assert_eq!(month_start_utc_ms(october), october * 1000);
        assert_eq!(month_start_utc_ms(october + 31 * 86_400 - 1), october * 1000);
        // 2026-11-01T00:00:00Z starts a new month.
        let november = october + 31 * 86_400;
        assert_eq!(month_start_utc_ms(november), november * 1000);
        // Leap day: 2028-02-29T12:00:00Z belongs to 2028-02-01.
        assert_eq!(month_start_utc_ms(1_835_438_400), 1_832_976_000 * 1000);
    }

    #[test]
    fn units_weight_cache_reads_at_a_tenth() {
        assert_eq!(weighted_units(2, 100, 430, 125_764), 2 + 100 + 430 + 12_576);
    }

    #[test]
    fn a_member_at_its_limit_is_over_and_one_below_is_not() {
        let tracker = BudgetTracker::default();
        let budgets: BTreeMap<String, u64> =
            [("at".to_string(), 100), ("below".to_string(), 100)].into();
        tracker.refresh_with(&budgets, |account| {
            Ok::<u64, String>(if account == "at" { 100 } else { 99 })
        });
        assert!(tracker.is_over(&budgets, "at"));
        assert!(!tracker.is_over(&budgets, "below"));
        assert!(!tracker.is_over(&budgets, "unbudgeted"));
    }

    #[test]
    fn unreadable_history_fails_closed() {
        let tracker = BudgetTracker::default();
        let budgets: BTreeMap<String, u64> = [("key".to_string(), 1_000_000)].into();
        assert!(tracker.is_over(&budgets, "key"), "unknown before the first read");
        tracker.refresh_with(&budgets, |_| Err::<u64, _>("database is locked"));
        assert!(tracker.is_over(&budgets, "key"));
        assert!(!tracker.is_over(&budgets, "unbudgeted"));
    }
}
