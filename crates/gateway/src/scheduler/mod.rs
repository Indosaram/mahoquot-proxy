pub mod algorithm;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use mahoquot_types::PoolMember;
use serde::{Deserialize, Serialize};

use crate::account::AccountMember;
use algorithm::{
    select_candidate, Candidate, WindowActivity, EXHAUSTION_ENTER_PERCENT,
    EXHAUSTION_RECOVER_PERCENT, MINIMUM_HOLD_SECS,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchedulerSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub priorities: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedState {
    #[serde(default = "state_version")]
    version: u8,
    #[serde(default)]
    selected: Option<String>,
    #[serde(default)]
    exhausted_since_unix: BTreeMap<String, i64>,
    #[serde(default)]
    non_auth_failures: BTreeMap<String, u8>,
}

fn state_version() -> u8 {
    1
}

#[cfg(test)]
mod reservation_tests {
    use super::*;

    #[test]
    fn runtime_reservations_exclude_even_during_fail_open() {
        let root =
            std::env::temp_dir().join(format!("mahoquot-reservation-{}", std::process::id()));
        let registry = SchedulerRegistry::load(&root.join("config.yaml"), &[]);
        assert!(registry.permits("codex-a"));
        registry.reserve("instance-a", "codex-a").unwrap();
        assert!(!registry.permits("codex-a"));
        assert!(registry.permits("codex-b"));
        assert!(registry.release("instance-a"));
        assert!(registry.permits("codex-a"));
    }

    #[test]
    fn duplicate_account_binding_is_rejected() {
        let root = std::env::temp_dir().join(format!(
            "mahoquot-reservation-conflict-{}",
            std::process::id()
        ));
        let registry = SchedulerRegistry::load(&root.join("config.yaml"), &[]);
        registry.reserve("instance-a", "codex-a").unwrap();
        let error = registry.reserve("instance-b", "codex-a").unwrap_err();
        assert!(error.contains("already reserved"));
        assert_eq!(
            registry
                .reservations()
                .get("instance-a")
                .map(String::as_str),
            Some("codex-a")
        );
    }

    #[test]
    fn retain_accounts_releases_removed_accounts() {
        let root = std::env::temp_dir().join(format!(
            "mahoquot-reservation-reconcile-{}",
            std::process::id()
        ));
        let registry = SchedulerRegistry::load(&root.join("config.yaml"), &[]);
        registry.reserve("instance-a", "codex-a").unwrap();
        registry.reserve("instance-b", "codex-b").unwrap();

        // codex-a has been deleted from the pool; codex-b is still live.
        let active: BTreeSet<String> = ["codex-b".to_string()].into_iter().collect();
        let released = registry.retain_accounts(&active);

        assert_eq!(released, vec!["instance-a".to_string()]);
        let reservations = registry.reservations();
        assert_eq!(reservations.len(), 1);
        assert_eq!(
            reservations.get("instance-b").map(String::as_str),
            Some("codex-b")
        );
        // The released account is reservable again; the live one stays bound.
        assert!(registry.permits("codex-a"));
        assert!(!registry.permits("codex-b"));
    }

    #[test]
    fn retain_accounts_keeps_live_accounts_and_other_instances() {
        let root = std::env::temp_dir().join(format!(
            "mahoquot-reservation-keep-{}",
            std::process::id()
        ));
        let registry = SchedulerRegistry::load(&root.join("config.yaml"), &[]);
        registry.reserve("instance-a", "codex-a").unwrap();
        registry.reserve("instance-b", "codex-b").unwrap();

        let active: BTreeSet<String> = ["codex-a", "codex-b", "codex-c"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let released = registry.retain_accounts(&active);

        assert!(released.is_empty());
        let reservations = registry.reservations();
        assert_eq!(reservations.len(), 2);
        assert_eq!(
            reservations.get("instance-a").map(String::as_str),
            Some("codex-a")
        );
        assert_eq!(
            reservations.get("instance-b").map(String::as_str),
            Some("codex-b")
        );
        assert!(!registry.permits("codex-a"));
        assert!(!registry.permits("codex-b"));
    }

    #[test]
    fn reserving_an_account_outside_the_activated_pool_is_rejected() {
        let root = std::env::temp_dir().join(format!(
            "mahoquot-reservation-inactive-{}",
            std::process::id()
        ));
        let registry = SchedulerRegistry::load(&root.join("config.yaml"), &[]);
        // Before activation a standalone registry still accepts reservations.
        assert!(registry.reserve("instance-a", "codex-a").is_ok());
        assert!(registry.release("instance-a"));

        let active: BTreeSet<String> = ["codex-live".to_string()].into_iter().collect();
        registry.retain_accounts(&active);

        let error = registry.reserve("instance-b", "codex-gone").unwrap_err();
        assert!(error.contains("not active"), "unexpected error: {error}");
        // A live account is still reservable after the allowlist is published.
        assert!(registry.reserve("instance-c", "codex-live").is_ok());
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountScheduleStatus {
    pub id: String,
    pub selected: bool,
    pub parked: bool,
    pub priority: Option<u32>,
    pub remaining_percent: Option<u8>,
    pub reset_at_unix: Option<i64>,
    pub consecutive_non_auth_failures: u8,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchedulerSnapshot {
    pub enabled: bool,
    pub selected: Option<String>,
    pub order: Vec<String>,
    pub fail_open: bool,
    pub reason: String,
    pub accounts: Vec<AccountScheduleStatus>,
    #[serde(skip)]
    allowed: BTreeSet<String>,
}

impl Default for SchedulerSnapshot {
    fn default() -> Self {
        Self {
            enabled: false,
            selected: None,
            order: Vec::new(),
            fail_open: true,
            reason: "disabled".to_string(),
            accounts: Vec::new(),
            allowed: BTreeSet::new(),
        }
    }
}

#[derive(Debug)]
pub struct SchedulerRegistry {
    settings: ArcSwap<SchedulerSettings>,
    snapshot: ArcSwap<SchedulerSnapshot>,
    runtime: std::sync::Mutex<PersistedState>,
    settings_path: PathBuf,
    state_path: PathBuf,
    state_parse_failed: AtomicBool,
    reservations: std::sync::Mutex<ReservationState>,
}

/// Guarded reservation table. The live-id allowlist and the reservations share
/// one mutex, so an input check and the insert it guards can never be
/// interleaved with a reconcile.
#[derive(Debug, Default)]
struct ReservationState {
    /// Live pool ids; `None` until `retain_accounts` activates the allowlist,
    /// so a standalone registry accepts reservations until membership is explicit.
    active: Option<BTreeSet<String>>,
    reservations: BTreeMap<String, String>,
}

impl SchedulerRegistry {
    pub fn load(config_path: &Path, members: &[Arc<AccountMember>]) -> Self {
        let settings_path = config_path.with_file_name("scheduler-settings.json");
        let state_path = config_path.with_file_name("scheduler-state.json");
        let (settings, settings_failed) = load_json::<SchedulerSettings>(&settings_path);
        let (runtime, state_failed) = load_json::<PersistedState>(&state_path);
        let registry = Self {
            settings: ArcSwap::from_pointee(settings.unwrap_or_default()),
            snapshot: ArcSwap::from_pointee(SchedulerSnapshot::default()),
            runtime: std::sync::Mutex::new(runtime.unwrap_or_default()),
            settings_path,
            state_path,
            state_parse_failed: AtomicBool::new(settings_failed || state_failed),
            reservations: std::sync::Mutex::new(ReservationState::default()),
        };
        registry.reconcile(members);
        registry
    }

    pub fn settings(&self) -> Arc<SchedulerSettings> {
        self.settings.load_full()
    }

    pub fn snapshot(&self) -> Arc<SchedulerSnapshot> {
        self.snapshot.load_full()
    }

    pub fn permits(&self, account: &str) -> bool {
        if self
            .reservations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .reservations
            .values()
            .any(|reserved| reserved == account)
        {
            return false;
        }
        let snapshot = self.snapshot.load();
        snapshot.fail_open || snapshot.allowed.contains(account)
    }

    pub fn reserve(&self, instance_id: &str, account_id: &str) -> Result<(), String> {
        let mut state = self
            .reservations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Once the pool membership is explicit, a reservation for an account
        // that is not live is rejected under the same lock that holds the
        // table, so a concurrent reconcile cannot land between check and write.
        if state
            .active
            .as_ref()
            .is_some_and(|active| !active.contains(account_id))
        {
            return Err(format!("account {account_id} is not active"));
        }
        if state
            .reservations
            .iter()
            .any(|(instance, account)| instance != instance_id && account == account_id)
        {
            return Err(format!("account {account_id} is already reserved"));
        }
        state
            .reservations
            .insert(instance_id.to_string(), account_id.to_string());
        Ok(())
    }

    pub fn release(&self, instance_id: &str) -> bool {
        self.reservations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .reservations
            .remove(instance_id)
            .is_some()
    }

    pub fn reservations(&self) -> BTreeMap<String, String> {
        self.reservations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .reservations
            .clone()
    }

    /// Sets the live-id allowlist and releases reservations whose account left
    /// the pool, all under one guard. Returns the released instance ids.
    pub fn retain_accounts(&self, active: &BTreeSet<String>) -> Vec<String> {
        let mut state = self
            .reservations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.active = Some(active.clone());
        let mut released = Vec::new();
        state.reservations.retain(|instance, account| {
            if active.contains(account.as_str()) {
                true
            } else {
                released.push(instance.clone());
                false
            }
        });
        released
    }

    pub fn update_settings(
        &self,
        next: SchedulerSettings,
        members: &[Arc<AccountMember>],
    ) -> std::io::Result<Arc<SchedulerSnapshot>> {
        persist_json(&self.settings_path, &next)?;
        self.settings.store(Arc::new(next));
        self.state_parse_failed.store(false, Ordering::Release);
        self.reconcile(members);
        Ok(self.snapshot())
    }

    pub fn set_order(
        &self,
        order: &[String],
        members: &[Arc<AccountMember>],
    ) -> std::io::Result<Arc<SchedulerSnapshot>> {
        let known: BTreeSet<&str> = members.iter().map(|member| member.id.as_str()).collect();
        let mut next = (*self.settings()).clone();
        next.priorities.clear();
        for (priority, id) in order.iter().enumerate() {
            if known.contains(id.as_str()) && !next.priorities.contains_key(id) {
                next.priorities.insert(id.clone(), priority as u32);
            }
        }
        self.update_settings(next, members)
    }

    pub fn reconcile(&self, members: &[Arc<AccountMember>]) {
        let settings = self.settings();
        if !settings.enabled {
            self.snapshot.store(Arc::new(SchedulerSnapshot {
                enabled: false,
                reason: "disabled".to_string(),
                ..SchedulerSnapshot::default()
            }));
            return;
        }
        if self.state_parse_failed.load(Ordering::Acquire) {
            self.snapshot.store(Arc::new(SchedulerSnapshot {
                enabled: true,
                reason: "state-parse-failure".to_string(),
                ..SchedulerSnapshot::default()
            }));
            return;
        }

        let now = now_unix();
        let mut runtime = self
            .runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = serde_json::to_vec(&*runtime).unwrap_or_default();
        let ids: BTreeSet<&str> = members.iter().map(|member| member.id.as_str()).collect();
        runtime
            .exhausted_since_unix
            .retain(|id, _| ids.contains(id.as_str()));
        runtime
            .non_auth_failures
            .retain(|id, _| ids.contains(id.as_str()));
        if runtime
            .selected
            .as_deref()
            .is_some_and(|id| !ids.contains(id))
        {
            runtime.selected = None;
        }

        let facts: Vec<_> = members
            .iter()
            .map(|member| {
                let usage = member.usage_snapshot();
                // The account is as constrained as its MOST-consumed window
                // (opencodex #3029): a freshly-reset 5-hour burst window must
                // not make a nearly-exhausted weekly window look selectable.
                let mut remaining: Option<u8> = None;
                let mut reset_at_unix: Option<i64> = None;
                for window in [&usage.primary, &usage.secondary] {
                    let Some(used) = window.used_percent else {
                        continue;
                    };
                    let left = (100.0 - used).clamp(0.0, 100.0).round() as u8;
                    if remaining.is_none_or(|best| left < best) {
                        remaining = Some(left);
                        reset_at_unix = window.reset_at_unix;
                    }
                }
                if remaining.is_some_and(|value| value <= EXHAUSTION_ENTER_PERCENT) {
                    runtime
                        .exhausted_since_unix
                        .entry(member.id.clone())
                        .or_insert(now);
                } else if remaining.is_some_and(|value| value > EXHAUSTION_RECOVER_PERCENT) {
                    let held_long_enough = runtime
                        .exhausted_since_unix
                        .get(&member.id)
                        .is_none_or(|since| now.saturating_sub(*since) >= MINIMUM_HOLD_SECS);
                    if held_long_enough {
                        runtime.exhausted_since_unix.remove(&member.id);
                    }
                }
                (
                    member.id.as_str(),
                    remaining,
                    reset_at_unix,
                    member.health(),
                    runtime.exhausted_since_unix.get(&member.id).copied(),
                    runtime
                        .non_auth_failures
                        .get(&member.id)
                        .copied()
                        .unwrap_or(0),
                )
            })
            .collect();

        let explicit_priorities = !settings.priorities.is_empty();
        let candidates: Vec<_> = facts
            .iter()
            .map(
                |(id, remaining, reset, health, exhausted_since, failures)| Candidate {
                    key: id,
                    manually_disabled: !health.is_available(now * 1000)
                        || *failures >= 3
                        || exhausted_since
                            .is_some_and(|since| now.saturating_sub(since) < MINIMUM_HOLD_SECS),
                    manual_priority: if explicit_priorities {
                        settings.priorities.get(*id).copied()
                    } else if runtime.selected.as_deref() == Some(*id) {
                        Some(0)
                    } else if runtime.selected.is_some() {
                        Some(1)
                    } else {
                        None
                    },
                    remaining_percent: *remaining,
                    reset_at_unix: *reset,
                    window_activity: if reset.is_some_and(|value| value > now) {
                        WindowActivity::Active
                    } else {
                        WindowActivity::Idle
                    },
                    exhausted_since_unix: *exhausted_since,
                },
            )
            .collect();

        let selected =
            select_candidate(&candidates, now).map(|candidate| candidate.key.to_string());
        runtime.selected = selected.clone();
        let mut order: Vec<String> = candidates
            .iter()
            .filter(|candidate| {
                !candidate.manually_disabled && candidate.remaining_percent.is_some()
            })
            .map(|candidate| candidate.key.to_string())
            .collect();
        order.sort_by_key(|id| {
            let candidate = candidates
                .iter()
                .find(|candidate| candidate.key == id)
                .unwrap();
            (
                candidate.manual_priority.unwrap_or(u32::MAX),
                candidate.reset_at_unix.unwrap_or(i64::MAX),
                id.clone(),
            )
        });
        if let Some(selected) = selected.as_ref() {
            if let Some(position) = order.iter().position(|id| id == selected) {
                order.remove(position);
            }
            order.insert(0, selected.clone());
        }
        let fail_open = selected.is_none();
        let allowed = selected.iter().cloned().collect();
        let accounts = facts
            .iter()
            .map(
                |(id, remaining, reset, _, _, failures)| AccountScheduleStatus {
                    id: (*id).to_string(),
                    selected: selected.as_deref() == Some(*id),
                    parked: selected.is_some() && selected.as_deref() != Some(*id),
                    priority: settings.priorities.get(*id).copied(),
                    remaining_percent: *remaining,
                    reset_at_unix: *reset,
                    consecutive_non_auth_failures: *failures,
                },
            )
            .collect();
        self.snapshot.store(Arc::new(SchedulerSnapshot {
            enabled: true,
            selected,
            order,
            fail_open,
            reason: if fail_open {
                "no-eligible-target"
            } else {
                "scheduled"
            }
            .to_string(),
            accounts,
            allowed,
        }));
        let after = serde_json::to_vec(&*runtime).unwrap_or_default();
        if before != after {
            persist_json_background(self.state_path.clone(), runtime.clone());
        }
    }

    pub fn record_success(&self, account: &str, members: &[Arc<AccountMember>]) {
        let changed = {
            let mut runtime = self.runtime.lock().unwrap_or_else(|p| p.into_inner());
            runtime.non_auth_failures.remove(account).is_some()
        };
        if changed {
            self.reconcile(members);
        }
    }

    pub fn record_non_auth_failure(&self, account: &str, members: &[Arc<AccountMember>]) {
        {
            let mut runtime = self.runtime.lock().unwrap_or_else(|p| p.into_inner());
            let count = runtime
                .non_auth_failures
                .entry(account.to_string())
                .or_default();
            *count = count.saturating_add(1);
        }
        self.reconcile(members);
    }

    pub fn record_auth_failure(&self, members: &[Arc<AccountMember>]) {
        self.reconcile(members);
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn load_json<T: serde::de::DeserializeOwned>(path: &Path) -> (Option<T>, bool) {
    match std::fs::read(path) {
        Ok(raw) => match serde_json::from_slice(&raw) {
            Ok(value) => (Some(value), false),
            Err(error) => {
                tracing::warn!(path = %path.display(), error = %error, "scheduler sidecar parse failed; failing open");
                (None, true)
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (None, false),
        Err(error) => {
            tracing::warn!(path = %path.display(), error = %error, "scheduler sidecar read failed; failing open");
            (None, true)
        }
    }
}

fn persist_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    let rendered = serde_json::to_vec_pretty(value)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    let mut file = std::fs::File::create(&temp)?;
    file.write_all(&rendered)?;
    file.sync_all()?;
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

fn persist_json_background(path: PathBuf, state: PersistedState) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn_blocking(move || {
            if let Err(error) = persist_json(&path, &state) {
                tracing::warn!(path = %path.display(), error = %error, "failed to persist scheduler state");
            }
        });
    } else if let Err(error) = persist_json(&path, &state) {
        tracing::warn!(path = %path.display(), error = %error, "failed to persist scheduler state");
    }
}
