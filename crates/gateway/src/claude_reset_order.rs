use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::account::{AccountMember, ProviderKind};

pub const REFRESH_INTERVAL_SECS: i64 = 30 * 60;
/// Below this share of quota left, a new session would exhaust the account
/// mid-conversation and rewrite its whole prompt cache on the next account.
pub const NEW_SESSION_MIN_REMAINING_PERCENT: f64 = 10.0;

/// Reset-order ranks keyed by account id, stamped with the second they were computed.
type CachedRanks = (i64, Arc<HashMap<String, usize>>);

#[derive(Debug, Default)]
pub struct ClaudeResetOrder {
    cached: Mutex<Option<CachedRanks>>,
}

pub fn is_claude_subscription(member: &AccountMember) -> bool {
    member.kind() == ProviderKind::Claude && member.relay_api_key().is_none()
}

fn active_five_hour_reset(member: &AccountMember, now_unix: i64) -> Option<i64> {
    member
        .usage_snapshot()
        .primary
        .reset_at_unix
        .filter(|reset| *reset > now_unix)
}

fn is_nearly_spent(member: &AccountMember, now_unix: i64) -> bool {
    let usage = member.usage_snapshot();
    [usage.primary, usage.secondary].into_iter().any(|window| {
        let live = window.reset_at_unix.is_none_or(|reset| reset > now_unix);
        live && window
            .used_percent
            .is_some_and(|used| 100.0 - used < NEW_SESSION_MIN_REMAINING_PERCENT)
    })
}

/// Ranks Claude subscription accounts so the one whose active 5-hour window
/// resets soonest comes first; accounts without an active window follow in pool
/// order.
pub fn rank_by_five_hour_reset(
    members: &[Arc<AccountMember>],
    now_unix: i64,
) -> HashMap<String, usize> {
    let mut keyed: Vec<(i64, usize, &str)> = members
        .iter()
        .enumerate()
        .filter(|(_, member)| is_claude_subscription(member))
        .map(|(index, member)| {
            let reset = active_five_hour_reset(member, now_unix).unwrap_or(i64::MAX);
            (reset, index, member.id.as_str())
        })
        .collect();
    keyed.sort();
    keyed
        .into_iter()
        .enumerate()
        .map(|(rank, (_, _, id))| (id.to_string(), rank))
        .collect()
}

impl ClaudeResetOrder {
    /// Recomputes every 30 minutes, or earlier once a ranked window resets, so
    /// a freshly reset account never keeps the top slot.
    pub fn ranks(
        &self,
        members: &[Arc<AccountMember>],
        now_unix: i64,
    ) -> Arc<HashMap<String, usize>> {
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((valid_until, ranks)) = cached.as_ref() {
            if now_unix < *valid_until {
                return Arc::clone(ranks);
            }
        }
        let ranks = Arc::new(rank_by_five_hour_reset(members, now_unix));
        let next_check = now_unix.saturating_add(REFRESH_INTERVAL_SECS);
        let valid_until = members
            .iter()
            .filter(|member| is_claude_subscription(member))
            .filter_map(|member| active_five_hour_reset(member, now_unix))
            .min()
            .map_or(next_check, |reset| reset.min(next_check));
        *cached = Some((valid_until, Arc::clone(&ranks)));
        ranks
    }
}

/// Reorders only the slots held by Claude subscription accounts, so every other
/// provider keeps its position in the candidate list. Nearly spent accounts go
/// last for new sessions; sessions already bound keep their account through
/// router affinity.
pub fn reorder_claude_slots(
    ordered: &mut [usize],
    members: &[Arc<AccountMember>],
    ranks: &HashMap<String, usize>,
    now_unix: i64,
) {
    let slots: Vec<usize> = ordered
        .iter()
        .enumerate()
        .filter(|(_, index)| {
            members
                .get(**index)
                .is_some_and(|member| is_claude_subscription(member))
        })
        .map(|(slot, _)| slot)
        .collect();
    let mut claude: Vec<usize> = slots.iter().map(|slot| ordered[*slot]).collect();
    claude.sort_by_key(|index| {
        let member = &members[*index];
        (
            is_nearly_spent(member, now_unix),
            ranks.get(member.id.as_str()).copied().unwrap_or(usize::MAX),
        )
    });
    for (slot, index) in slots.into_iter().zip(claude) {
        ordered[slot] = index;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GatewayConfig;
    use crate::state::AppState;
    use crate::usage::{AccountUsage, QuotaWindow};

    const NOW: i64 = 1_800_000_000;

    fn credential(kind: &str, slug: &str) -> String {
        let extra = if kind == "codex" {
            r#""account_id":"acc","id_token":"id","last_refresh":"2026-01-01T00:00:00Z","#
        } else {
            ""
        };
        format!(
            r#"{{{extra}"identity_slug":"{slug}","access_token":"token","refresh_token":"refresh","email":"{slug}@test.invalid","expired":"2099-01-01T00:00:00Z","type":"{kind}"}}"#
        )
    }

    fn state(files: &[(&str, &str, &str)]) -> (AppState, std::path::PathBuf) {
        let auth_dir = std::env::temp_dir().join(format!(
            "mahoquot-claude-reset-order-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&auth_dir).expect("create auth dir");
        for (file, kind, slug) in files {
            std::fs::write(auth_dir.join(file), credential(kind, slug)).expect("write credential");
        }
        let order: Vec<&str> = files.iter().map(|(file, _, _)| *file).collect();
        std::fs::write(
            auth_dir.join(".mahoquot-account-order.json"),
            serde_json::to_vec(&order).expect("order json"),
        )
        .expect("write order");
        let config = GatewayConfig {
            auth_dir: auth_dir.clone(),
            config_path: auth_dir.join("config.yaml"),
            auth_refresh_enabled: false,
            ..GatewayConfig::default()
        };
        (AppState::new(&config).expect("state"), auth_dir)
    }

    fn set_window(member: &AccountMember, used_percent: f64, reset_at_unix: Option<i64>) {
        member.set_usage(AccountUsage {
            primary: QuotaWindow {
                used_percent: Some(used_percent),
                window_minutes: Some(300),
                reset_at_unix,
                ..QuotaWindow::default()
            },
            ..AccountUsage::default()
        });
    }

    fn member<'a>(members: &'a [Arc<AccountMember>], slug: &str) -> &'a Arc<AccountMember> {
        members
            .iter()
            .find(|member| member.id.contains(slug))
            .unwrap_or_else(|| panic!("member {slug}"))
    }

    fn reordered(members: &[Arc<AccountMember>], now: i64) -> Vec<String> {
        let ranks = rank_by_five_hour_reset(members, now);
        let mut ordered: Vec<usize> = (0..members.len()).collect();
        reorder_claude_slots(&mut ordered, members, &ranks, now);
        ordered
            .iter()
            .map(|index| members[*index].id.clone())
            .collect()
    }

    #[test]
    fn soonest_active_reset_leads_and_other_providers_keep_their_slots() {
        let (state, auth_dir) = state(&[
            ("claude-late.json", "claude", "late"),
            ("codex-mid.json", "codex", "mid"),
            ("claude-idle.json", "claude", "idle"),
            ("claude-soon.json", "claude", "soon"),
        ]);
        let pool = state.pool.load_full();
        let members = &pool.members;
        set_window(member(members, "late"), 10.0, Some(NOW + 4 * 3600));
        set_window(member(members, "soon"), 10.0, Some(NOW + 20 * 60));
        set_window(member(members, "idle"), 10.0, Some(NOW - 60));

        let before: Vec<String> = members.iter().map(|member| member.id.clone()).collect();
        let after = reordered(members, NOW);

        let codex_slot = |list: &[String]| list.iter().position(|id| id.contains("mid"));
        assert_eq!(codex_slot(&after), codex_slot(&before));
        let claude: Vec<&String> = after.iter().filter(|id| !id.contains("mid")).collect();
        assert!(claude[0].contains("soon"), "{after:?}");
        assert!(claude[1].contains("late"), "{after:?}");
        assert!(claude[2].contains("idle"), "{after:?}");

        std::fs::remove_dir_all(auth_dir).ok();
    }

    #[test]
    fn ranking_is_reused_until_the_thirty_minute_check() {
        let (state, auth_dir) = state(&[
            ("claude-a.json", "claude", "first"),
            ("claude-b.json", "claude", "second"),
        ]);
        let pool = state.pool.load_full();
        let members = &pool.members;
        let first = member(members, "first");
        let second = member(members, "second");
        set_window(first, 10.0, Some(NOW + 3600));
        set_window(second, 10.0, Some(NOW + 7200));

        let order = ClaudeResetOrder::default();
        assert_eq!(order.ranks(members, NOW).get(&first.id), Some(&0));

        set_window(second, 10.0, Some(NOW + REFRESH_INTERVAL_SECS + 60));
        let within = order.ranks(members, NOW + REFRESH_INTERVAL_SECS - 1);
        assert_eq!(
            within.get(&first.id),
            Some(&0),
            "re-sorted before the 30-minute check"
        );

        let after = order.ranks(members, NOW + REFRESH_INTERVAL_SECS);
        assert_eq!(
            after.get(&second.id),
            Some(&0),
            "30-minute check must pick up the new reset"
        );

        std::fs::remove_dir_all(auth_dir).ok();
    }

    #[test]
    fn a_reset_before_the_thirty_minute_check_demotes_the_account_at_once() {
        let (state, auth_dir) = state(&[
            ("claude-a.json", "claude", "first"),
            ("claude-b.json", "claude", "second"),
        ]);
        let pool = state.pool.load_full();
        let members = &pool.members;
        let first = member(members, "first");
        let second = member(members, "second");
        set_window(first, 70.0, Some(NOW + 600));
        set_window(second, 30.0, Some(NOW + 3600));

        let order = ClaudeResetOrder::default();
        assert_eq!(order.ranks(members, NOW).get(&first.id), Some(&0));
        assert_eq!(order.ranks(members, NOW + 599).get(&first.id), Some(&0));

        let after_reset = order.ranks(members, NOW + 600);
        assert_eq!(
            after_reset.get(&second.id),
            Some(&0),
            "a window that reset must not keep the top slot until the 30-minute check"
        );

        std::fs::remove_dir_all(auth_dir).ok();
    }

    #[test]
    fn nearly_spent_account_yields_new_sessions_to_the_next_reset() {
        let (state, auth_dir) = state(&[
            ("claude-a.json", "claude", "spent"),
            ("claude-b.json", "claude", "roomy"),
        ]);
        let pool = state.pool.load_full();
        let members = &pool.members;
        set_window(member(members, "spent"), 95.0, Some(NOW + 600));
        set_window(member(members, "roomy"), 40.0, Some(NOW + 3600));

        let after = reordered(members, NOW);
        assert!(after[0].contains("roomy"), "{after:?}");
        assert!(
            after[1].contains("spent"),
            "spent account stays reachable last: {after:?}"
        );

        set_window(member(members, "spent"), 95.0, Some(NOW - 1));
        assert!(
            !is_nearly_spent(member(members, "spent"), NOW),
            "usage from a window that already reset is stale"
        );

        std::fs::remove_dir_all(auth_dir).ok();
    }
}
