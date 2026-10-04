use serde::Serialize;
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const RING_CAPACITY: usize = 1024;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TtftSnapshot {
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
    pub samples: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LastError {
    pub unix_ms: i64,
    pub status: u16,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct PromAccount {
    pub id: String,
    pub ok: u64,
    pub fails: u64,
    pub cooldown_until_unix_ms: Option<i64>,
}

#[derive(Debug)]
struct RingBuffer {
    samples: Vec<f64>,
    head: usize,
    is_full: bool,
}

impl Default for RingBuffer {
    fn default() -> Self {
        Self {
            samples: Vec::with_capacity(RING_CAPACITY),
            head: 0,
            is_full: false,
        }
    }
}

impl RingBuffer {
    fn push(&mut self, sample: f64) {
        if !self.is_full {
            self.samples.push(sample);
            self.is_full = self.samples.len() == RING_CAPACITY;
        } else {
            self.samples[self.head] = sample;
            self.head = (self.head + 1) % RING_CAPACITY;
        }
    }

    fn snapshot(&self) -> TtftSnapshot {
        if self.samples.is_empty() {
            return TtftSnapshot {
                p50_ms: 0.0,
                p90_ms: 0.0,
                p99_ms: 0.0,
                samples: 0,
            };
        }
        let mut sorted = self.samples.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        TtftSnapshot {
            p50_ms: calc_percentile(&sorted, 0.50),
            p90_ms: calc_percentile(&sorted, 0.90),
            p99_ms: calc_percentile(&sorted, 0.99),
            samples: sorted.len(),
        }
    }
}

fn calc_percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let idx = p * (sorted.len() - 1) as f64;
    let (lower, upper) = (idx.floor() as usize, idx.ceil() as usize);
    if lower == upper {
        sorted[lower]
    } else {
        let weight = idx - lower as f64;
        sorted[lower] * (1.0 - weight) + sorted[upper] * weight
    }
}

/// Stable, non-reversible label for an account on the PUBLIC metrics surface.
///
/// Account ids are credential emails, and `/metrics` is served without the API
/// key that gates `/admin/stats` precisely because it exposes those emails.
/// Scrapers only need a stable series key, so a truncated digest keeps the
/// per-account counters usable without publishing the identity.
pub fn public_account_label(account_id: &str) -> String {
    let digest = crate::request_history::stable_key_identifier(account_id);
    format!("acct_{}", &digest[..16])
}

fn escape_label_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str(r#"\""#),
            '\n' => out.push_str(r"\n"),
            _ => out.push(c),
        }
    }
    out
}

#[derive(Debug)]
pub struct InFlightGuard {
    state: Arc<MonitorState>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.state.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The set of account ids currently present in the pool. A store keeps this
/// allowlist inside the same guarded value as its data, so a prune can never be
/// undone by an insert that raced the pool deletion.
type ActiveIds = std::collections::BTreeSet<String>;

#[derive(Debug, Default)]
struct MonitorAccounts {
    /// Live pool ids. `None` until `retain_accounts` activates the allowlist,
    /// so a standalone store accepts every writer until membership is explicit.
    active: Option<ActiveIds>,
    ttft: HashMap<String, RingBuffer>,
    last_errors: HashMap<String, LastError>,
}

impl MonitorAccounts {
    fn admits(&self, account_id: &str) -> bool {
        self.active
            .as_ref()
            .is_none_or(|active| active.contains(account_id))
    }
}

#[derive(Debug)]
pub struct MonitorState {
    started_at_unix_ms: i64,
    in_flight: AtomicU64,
    global_ttft: Mutex<RingBuffer>,
    accounts: Mutex<MonitorAccounts>,
}

impl Default for MonitorState {
    fn default() -> Self {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Self::new(now_ms)
    }
}

impl MonitorState {
    pub fn new(now_unix_ms: i64) -> Self {
        Self {
            started_at_unix_ms: now_unix_ms,
            in_flight: AtomicU64::new(0),
            global_ttft: Mutex::new(RingBuffer::default()),
            accounts: Mutex::new(MonitorAccounts::default()),
        }
    }

    pub fn uptime_secs(&self, now_unix_ms: i64) -> u64 {
        if now_unix_ms > self.started_at_unix_ms {
            ((now_unix_ms - self.started_at_unix_ms) / 1000) as u64
        } else {
            0
        }
    }

    pub fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::SeqCst)
    }

    pub fn track_in_flight(self: &Arc<Self>) -> InFlightGuard {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        InFlightGuard {
            state: Arc::clone(self),
        }
    }

    /// Records a TTFT sample. Once `retain_accounts` has activated the live-id
    /// allowlist a sample for a departed account is dropped; before activation
    /// a standalone store accepts every writer. The membership check and the
    /// insert share one lock acquisition, so a concurrent `retain_accounts` can
    /// never be interleaved between them. Returns `false` when dropped.
    pub fn record_ttft(&self, account_id: &str, ttft_ms: f64) -> bool {
        let Ok(mut accounts) = self.accounts.lock() else {
            return false;
        };
        if !accounts.admits(account_id) {
            return false;
        }
        accounts
            .ttft
            .entry(account_id.to_string())
            .or_default()
            .push(ttft_ms);
        drop(accounts);
        if let Ok(mut global) = self.global_ttft.lock() {
            global.push(ttft_ms);
        }
        true
    }

    pub fn ttft_percentiles(&self) -> TtftSnapshot {
        self.global_ttft
            .lock()
            .map(|g| g.snapshot())
            .unwrap_or_else(|_| TtftSnapshot {
                p50_ms: 0.0,
                p90_ms: 0.0,
                p99_ms: 0.0,
                samples: 0,
            })
    }

    pub fn account_ttft(&self, account_id: &str) -> Option<TtftSnapshot> {
        self.accounts
            .lock()
            .ok()
            .and_then(|accounts| accounts.ttft.get(account_id).map(|buf| buf.snapshot()))
    }

    /// Records the last error for an account. Once `retain_accounts` has
    /// activated the live-id allowlist an error for a departed account is
    /// dropped; before activation a standalone store accepts every writer. The
    /// membership check and the insert share one lock acquisition. Returns
    /// `false` when dropped.
    pub fn record_error(&self, account_id: &str, status: u16, message: &str) -> bool {
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let entry = LastError {
            unix_ms,
            status,
            message: message.to_string(),
        };
        let Ok(mut accounts) = self.accounts.lock() else {
            return false;
        };
        if !accounts.admits(account_id) {
            return false;
        }
        accounts.last_errors.insert(account_id.to_string(), entry);
        true
    }

    /// Replaces the allowlist and prunes both the TTFT rings and the last
    /// errors of ids that left the pool, all under one guard. Returns the
    /// number of distinct accounts pruned.
    pub fn retain_accounts(&self, active: &ActiveIds) -> usize {
        let mut accounts = match self.accounts.lock() {
            Ok(accounts) => accounts,
            Err(_) => return 0,
        };
        accounts.active = Some(active.clone());
        let mut pruned: std::collections::HashSet<String> = std::collections::HashSet::new();
        accounts.ttft.retain(|id, _| {
            if active.contains(id) {
                true
            } else {
                pruned.insert(id.clone());
                false
            }
        });
        accounts.last_errors.retain(|id, _| {
            if active.contains(id) {
                true
            } else {
                pruned.insert(id.clone());
                false
            }
        });
        pruned.len()
    }

    /// Every account id that still owns state in this store, in a stable order.
    pub fn tracked_accounts(&self) -> Vec<String> {
        let accounts = match self.accounts.lock() {
            Ok(accounts) => accounts,
            Err(_) => return Vec::new(),
        };
        let mut ids: std::collections::BTreeSet<&String> = std::collections::BTreeSet::new();
        ids.extend(accounts.ttft.keys());
        ids.extend(accounts.last_errors.keys());
        ids.into_iter().cloned().collect()
    }

    pub fn clear_error(&self, account_id: &str) {
        if let Ok(mut accounts) = self.accounts.lock() {
            accounts.last_errors.remove(account_id);
        }
    }

    pub fn last_error(&self, account_id: &str) -> Option<LastError> {
        self.accounts
            .lock()
            .ok()
            .and_then(|accounts| accounts.last_errors.get(account_id).cloned())
    }

    pub fn render_prometheus(&self, now_unix_ms: i64, accounts: &[PromAccount]) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "# HELP mahoquot_uptime_seconds Process uptime in seconds.\n# TYPE mahoquot_uptime_seconds gauge\nmahoquot_uptime_seconds {}", self.uptime_secs(now_unix_ms));
        let _ = writeln!(out, "# HELP mahoquot_in_flight_requests Current number of in-flight requests.\n# TYPE mahoquot_in_flight_requests gauge\nmahoquot_in_flight_requests {}", self.in_flight());

        let ttft = self.ttft_percentiles();
        let _ = writeln!(
            out,
            "# HELP mahoquot_ttft_milliseconds TTFT percentiles in milliseconds.\n# TYPE mahoquot_ttft_milliseconds gauge\nmahoquot_ttft_milliseconds{{quantile=\"0.5\"}} {}\nmahoquot_ttft_milliseconds{{quantile=\"0.9\"}} {}\nmahoquot_ttft_milliseconds{{quantile=\"0.99\"}} {}",
            ttft.p50_ms, ttft.p90_ms, ttft.p99_ms
        );

        let _ = writeln!(out, "# HELP mahoquot_account_requests_total Total request count per account.\n# TYPE mahoquot_account_requests_total counter");
        for acc in accounts {
            let id = escape_label_value(&public_account_label(&acc.id));
            let _ = writeln!(
                out,
                "mahoquot_account_requests_total{{account=\"{id}\",outcome=\"ok\"}} {}",
                acc.ok
            );
            let _ = writeln!(
                out,
                "mahoquot_account_requests_total{{account=\"{id}\",outcome=\"fail\"}} {}",
                acc.fails
            );
        }

        let _ = writeln!(out, "# HELP mahoquot_account_cooldown_until_seconds Cooldown target timestamp in seconds.\n# TYPE mahoquot_account_cooldown_until_seconds gauge");
        for acc in accounts {
            let id = escape_label_value(&public_account_label(&acc.id));
            let cooldown = match acc.cooldown_until_unix_ms {
                Some(until_ms) if until_ms > now_unix_ms => until_ms / 1000,
                _ => 0,
            };
            let _ = writeln!(
                out,
                "mahoquot_account_cooldown_until_seconds{{account=\"{id}\"}} {cooldown}"
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(ids: &[&str]) -> ActiveIds {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn monitor_record_ttft_drops_a_removed_account() {
        let monitor = MonitorState::new(0);
        monitor.retain_accounts(&live(&["live", "gone"]));

        assert!(
            monitor.record_ttft("live", 10.0),
            "a live account must accept a TTFT sample"
        );
        assert!(monitor.record_ttft("live", 20.0));
        assert!(monitor.record_ttft("gone", 11.0));

        assert_eq!(
            monitor.retain_accounts(&live(&["live"])),
            1,
            "the removed account's TTFT ring is pruned"
        );

        assert!(
            !monitor.record_ttft("gone", 12.0),
            "a late sample for a removed account must be dropped"
        );
        assert!(
            monitor.account_ttft("gone").is_none(),
            "the dropped sample must not recreate the removed account's ring"
        );
        assert_eq!(
            monitor.account_ttft("live").map(|s| s.samples),
            Some(2),
            "the surviving account keeps every sample it recorded"
        );
    }

    #[test]
    fn monitor_retain_accounts_prunes_ttft_and_last_errors_under_one_guard() {
        let monitor = MonitorState::new(0);
        monitor.retain_accounts(&live(&["keep", "ttft-only", "error-only"]));

        assert!(monitor.record_ttft("keep", 1.0));
        assert!(monitor.record_ttft("ttft-only", 2.0));
        assert!(monitor.record_error("keep", 500, "transient"));
        assert!(monitor.record_error("error-only", 503, "upstream"));

        let pruned = monitor.retain_accounts(&live(&["keep"]));

        assert_eq!(pruned, 2, "each distinct departed account is counted once");
        assert!(monitor.account_ttft("ttft-only").is_none());
        assert!(monitor.last_error("error-only").is_none());
        assert_eq!(monitor.tracked_accounts(), vec!["keep".to_string()]);
        assert!(monitor.account_ttft("keep").is_some());
        assert!(monitor.last_error("keep").is_some());

        // The rewritten allowlist stays in force: a pruned id cannot be
        // resurrected by a writer that still holds its old id.
        assert!(!monitor.record_ttft("ttft-only", 3.0));
        assert!(!monitor.record_error("error-only", 503, "late"));
    }

    #[test]
    fn monitor_standalone_default_records_before_activation() {
        // Before any `retain_accounts`, membership is not explicit: existing
        // callers (`relay.rs` TTFT/error recording) must keep working unchanged.
        let monitor = MonitorState::new(0);
        monitor.record_ttft("acc", 5.0);
        monitor.record_error("acc", 500, "boom");

        assert_eq!(
            monitor.account_ttft("acc").map(|s| s.samples),
            Some(1),
            "a store whose allowlist has not been activated must accept samples"
        );
        assert!(
            monitor.last_error("acc").is_some(),
            "a store whose allowlist has not been activated must accept errors"
        );
    }
}
