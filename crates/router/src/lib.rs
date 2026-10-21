//! Routing strategies over a dynamic pool.
//!
//! CONTRACT (docs/CONTRACTS.md §selection):
//! - StrictRoundRobin: member-id-keyed service sequence. Each select() picks the
//!   AVAILABLE member with the smallest (last_served_seq, tiebreak: first index),
//!   then stamps it with a monotonically increasing global sequence. This makes
//!   rotation perfectly even and immune to membership churn (no positional cursor).
//! - FillFirst: first AVAILABLE member in list order every time.
//! - select() MUST NOT mutate member objects; bookkeeping lives inside Router.
//! - Router is pure selection: health transitions are owned by the caller.
//! - Session affinity keys are OPAQUE SCOPES composed by the relay (caller,
//!   route providers, compatible model group, session). The router never
//!   parses, merges or recombines them, so pinning one scope cannot overwrite
//!   another and a binding is only ever reused inside its own scope.

pub mod lcp;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use mahoquot_types::{PoolMember, SessionHint, Strategy};

const STRATEGY_STRICT_ROUND_ROBIN: u8 = 0;
const STRATEGY_FILL_FIRST: u8 = 1;

fn strategy_to_u8(strategy: Strategy) -> u8 {
    match strategy {
        Strategy::StrictRoundRobin => STRATEGY_STRICT_ROUND_ROBIN,
        Strategy::FillFirst => STRATEGY_FILL_FIRST,
    }
}

fn u8_to_strategy(val: u8) -> Strategy {
    match val {
        STRATEGY_FILL_FIRST => Strategy::FillFirst,
        _ => Strategy::StrictRoundRobin,
    }
}

#[derive(Default, Debug)]
struct RouterState {
    /// Live pool member ids; `None` until `retain_members` activates the
    /// allowlist, so a standalone router keeps recording every selection.
    active: Option<std::collections::BTreeSet<String>>,
    /// member id -> last assigned sequence number
    last_served: HashMap<String, u64>,
    /// monotonically increasing global service counter
    seq: u64,
    /// Scoped affinity key -> member id, so one caller's conversation on one
    /// route keeps hitting one account instead of hopping every turn. Keys are
    /// composed by the relay and treated as opaque strings here. Entries carry
    /// the sequence number at which they were last used so idle scopes can be
    /// evicted.
    affinity: HashMap<String, (String, u64)>,
}

impl RouterState {
    fn admits(&self, id: &str) -> bool {
        self.active
            .as_ref()
            .is_none_or(|active| active.contains(id))
    }
}

/// Sessions idle for this many selections are dropped, bounding the map on a
/// long-lived proxy without needing a timer.
const AFFINITY_MAX_IDLE: u64 = 10_000;

const AFFINITY_MAX_ENTRIES: usize = 1024;

#[derive(Default, Debug)]
pub struct Router {
    strategy: AtomicU8,
    state: Mutex<RouterState>,
}

impl Router {
    pub fn new(strategy: Strategy) -> Self {
        Self {
            strategy: AtomicU8::new(strategy_to_u8(strategy)),
            state: Mutex::new(RouterState::default()),
        }
    }

    pub fn set_strategy(&self, strategy: Strategy) {
        self.strategy
            .store(strategy_to_u8(strategy), Ordering::SeqCst);
    }

    /// Index into `members` of the next member to serve, or None if none available.
    pub fn select(&self, members: &[Arc<dyn PoolMember>], hint: &SessionHint) -> Option<usize> {
        let now_unix_ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => duration.as_millis().min(i64::MAX as u128) as i64,
            Err(_) => 0,
        };

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // Stick to the account this conversation already used, as long as
        // it is still healthy; only fall through when it is not, so a session
        // never hops accounts unnecessarily regardless of strategy.
        if let Some(key) = hint.affinity_key.as_deref() {
            if let Some((bound_id, _)) = state.affinity.get(key).cloned() {
                // A binding to a member outside the activated pool is stale and
                // must not be refreshed, so fall through to normal selection.
                if state.admits(&bound_id) {
                    if let Some(idx) = members
                        .iter()
                        .position(|m| m.id() == bound_id && m.health().is_available(now_unix_ms))
                    {
                        let seq = state.seq.wrapping_add(1);
                        state.seq = seq;
                        state.last_served.insert(bound_id.clone(), seq);
                        state.affinity.insert(key.to_string(), (bound_id, seq));
                        return Some(idx);
                    }
                }
            }
        }

        match self.strategy() {
            Strategy::FillFirst => {
                let chosen_idx = members
                    .iter()
                    .position(|m| m.health().is_available(now_unix_ms))?;
                let seq = state.seq.wrapping_add(1);
                state.seq = seq;
                let chosen_id = members[chosen_idx].id().to_string();

                if state.admits(&chosen_id) {
                    state.last_served.insert(chosen_id.clone(), seq);

                    if let Some(key) = hint.affinity_key.as_deref() {
                        state.affinity.insert(key.to_string(), (chosen_id, seq));
                        if state.affinity.len() > AFFINITY_MAX_ENTRIES {
                            let cutoff = seq.saturating_sub(AFFINITY_MAX_IDLE);
                            state.affinity.retain(|_, (_, s)| *s >= cutoff);
                        }
                    }
                }

                Some(chosen_idx)
            }
            Strategy::StrictRoundRobin => {
                let mut best: Option<(usize, u64)> = None;

                for (idx, member) in members.iter().enumerate() {
                    if !member.health().is_available(now_unix_ms) {
                        continue;
                    }

                    let last_seq = state.last_served.get(member.id()).copied().unwrap_or(0);

                    match best {
                        None => {
                            best = Some((idx, last_seq));
                        }
                        Some((_, min_seq)) if last_seq < min_seq => {
                            best = Some((idx, last_seq));
                        }
                        _ => {}
                    }
                }

                let (chosen_idx, _) = best?;
                state.seq = state.seq.wrapping_add(1);
                let next_seq = state.seq;
                let chosen_id = members[chosen_idx].id().to_string();

                if state.admits(&chosen_id) {
                    state.last_served.insert(chosen_id.clone(), next_seq);

                    if let Some(key) = hint.affinity_key.as_deref() {
                        state
                            .affinity
                            .insert(key.to_string(), (chosen_id, next_seq));
                        if state.affinity.len() > AFFINITY_MAX_ENTRIES {
                            let cutoff = next_seq.saturating_sub(AFFINITY_MAX_IDLE);
                            state.affinity.retain(|_, (_, seq)| *seq >= cutoff);
                        }
                    }
                }

                Some(chosen_idx)
            }
        }
    }

    /// Report the result of serving member `id`. Reserved for future weighting.
    pub fn feedback(&self, _id: &str, _outcome: mahoquot_types::Outcome) {}

    pub fn strategy(&self) -> Strategy {
        u8_to_strategy(self.strategy.load(Ordering::SeqCst))
    }

    /// Retrieve the member ID bound to an affinity key, if any.
    pub fn bound_affinity_member(&self, key: &str) -> Option<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.affinity.get(key).map(|(id, _)| id.clone())
    }

    /// Prunes rotation/affinity bookkeeping for member ids no longer present and
    /// activates the live-id allowlist, so a later `select` can never record a
    /// member that has already left the pool.
    pub fn retain_members(&self, active: &std::collections::BTreeSet<String>) -> usize {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.active = Some(active.clone());
        let before = state.last_served.len();
        state.last_served.retain(|id, _| active.contains(id));
        state.affinity.retain(|_, (id, _)| active.contains(id));
        before - state.last_served.len()
    }

    /// Member ids that currently hold rotation bookkeeping (diagnostic).
    pub fn tracked_members(&self) -> Vec<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.last_served.keys().cloned().collect()
    }
}

#[cfg(test)]
#[allow(clippy::new_ret_no_self)]
mod red_tests {
    //! INTENTIONALLY FAILING at scaffold (documented RED baseline).
    use super::*;
    use mahoquot_types::{Health, PoolMember};

    struct M {
        id: String,
        h: Health,
        reset: Option<i64>,
    }
    impl M {
        fn new(id: &str, h: Health) -> Arc<dyn PoolMember> {
            Arc::new(M {
                id: id.into(),
                h,
                reset: None,
            })
        }
    }
    impl PoolMember for M {
        fn id(&self) -> &str {
            &self.id
        }
        fn health(&self) -> Health {
            self.h
        }
        fn reset_at_unix(&self) -> Option<i64> {
            self.reset
        }
    }

    fn pool(ids: &[(&str, Health)]) -> Vec<Arc<dyn PoolMember>> {
        ids.iter().map(|(i, h)| M::new(i, *h)).collect()
    }

    #[test]
    fn strict_rr_distributes_evenly_over_four() {
        let p = pool(&[
            ("a", Health::Available),
            ("b", Health::Available),
            ("c", Health::Available),
            ("d", Health::Available),
        ]);
        let r = Router::new(Strategy::StrictRoundRobin);
        let hint = SessionHint::default();
        let mut counts = std::collections::HashMap::new();
        for _ in 0..40 {
            let i = r.select(&p, &hint).expect("pick");
            *counts.entry(p[i].id().to_string()).or_insert(0) += 1;
        }
        for id in ["a", "b", "c", "d"] {
            assert_eq!(
                counts[id], 10,
                "member {id} must be served exactly 10 times"
            );
        }
    }

    #[test]
    fn strict_rr_survives_membership_churn_without_cursor_poisoning() {
        let mut members: Vec<Arc<dyn PoolMember>> = vec![
            ("a", Health::Available),
            ("b", Health::Available),
            ("c", Health::Available),
        ]
        .into_iter()
        .map(|(i, h)| M::new(i, h))
        .collect();
        let r = Router::new(Strategy::StrictRoundRobin);
        let hint = SessionHint::default();
        // burn some rotations
        for i in 0..3 {
            r.select(&members, &hint);
            let _ = i;
        }
        // b cools down: all traffic goes to a,c evenly; b must not be double-skipped
        members[1] = M::new(
            "b",
            Health::Cooldown {
                until_unix_ms: i64::MAX,
            },
        );
        let mut a_c = std::collections::HashMap::new();
        for _ in 0..8 {
            let i = r.select(&members, &hint).unwrap();
            assert_ne!(members[i].id(), "b");
            *a_c.entry(members[i].id().to_string()).or_insert(0) += 1;
        }
        assert_eq!(a_c["a"], 4);
        assert_eq!(a_c["c"], 4);
        // b recovers: next 4 picks cover a,b,c,b-cycle evenly (each served per-seq rule)
        members[1] = M::new("b", Health::Available);
        let got = (0..3).map(|_| members[r.select(&members, &hint).unwrap()].id().to_string());
        let mut v: Vec<_> = got.collect();
        v.sort();
        assert_eq!(v, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    }

    #[test]
    fn fill_first_pins_to_first_available_then_moves_on_cooldown() {
        let mut members = pool(&[("a", Health::Available), ("b", Health::Available)]);
        let r = Router::new(Strategy::FillFirst);
        let hint = SessionHint::default();
        for _ in 0..5 {
            assert_eq!(
                r.select(&members, &hint).map(|i| members[i].id()),
                Some("a")
            );
        }
        members[0] = M::new(
            "a",
            Health::Cooldown {
                until_unix_ms: i64::MAX,
            },
        );
        for _ in 0..5 {
            assert_eq!(
                r.select(&members, &hint).map(|i| members[i].id()),
                Some("b")
            );
        }
    }

    #[test]
    fn empty_or_all_unavailable_pool_returns_none() {
        let p = pool(&[("a", Health::Disabled)]);
        let r = Router::new(Strategy::StrictRoundRobin);
        assert_eq!(r.select(&p, &SessionHint::default()), None);
        assert_eq!(r.select(&[], &SessionHint::default()), None);
    }

    fn keyed(k: &str) -> SessionHint {
        SessionHint {
            affinity_key: Some(k.to_string()),
        }
    }

    fn avail3() -> Vec<Arc<dyn PoolMember>> {
        pool(&[
            ("a", Health::Available),
            ("b", Health::Available),
            ("c", Health::Available),
        ])
    }

    #[test]
    fn session_sticks_to_one_account_across_turns() {
        let r = Router::new(Strategy::StrictRoundRobin);
        let p = avail3();
        let first = r.select(&p, &keyed("conv-1")).expect("first");
        for _ in 0..12 {
            assert_eq!(r.select(&p, &keyed("conv-1")), Some(first));
        }
    }

    #[test]
    fn fill_first_session_sticks_to_bound_account_even_if_prior_account_recovers() {
        let r = Router::new(Strategy::FillFirst);
        // Initially 'a' is cooled down, 'b' is available.
        let mut p = pool(&[
            (
                "a",
                Health::Cooldown {
                    until_unix_ms: i64::MAX,
                },
            ),
            ("b", Health::Available),
        ]);
        let first = r.select(&p, &keyed("conv-fill")).expect("select b");
        assert_eq!(p[first].id(), "b");

        // Now 'a' recovers and becomes Available.
        p[0] = M::new("a", Health::Available);

        // Under FillFirst without affinity, it would jump to 'a'.
        // BUT with affinity, it must stay on 'b' to keep prompt cache!
        let second = r.select(&p, &keyed("conv-fill")).expect("still b");
        assert_eq!(p[second].id(), "b");
    }

    #[test]
    fn distinct_sessions_spread_across_accounts() {
        let r = Router::new(Strategy::StrictRoundRobin);
        let p = avail3();
        let picked: Vec<usize> = (0..3)
            .filter_map(|i| r.select(&p, &keyed(&format!("conv-{i}"))))
            .collect();
        let mut uniq = picked.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(
            uniq.len(),
            3,
            "each new session should take a fresh account"
        );
    }

    #[test]
    fn session_moves_off_an_unhealthy_account() {
        let r = Router::new(Strategy::StrictRoundRobin);
        let p = pool(&[("a", Health::Available), ("b", Health::Available)]);
        let first = r.select(&p, &keyed("conv-1")).expect("first");
        let healthy = pool(&[
            (
                "a",
                if first == 0 {
                    Health::AuthFailed
                } else {
                    Health::Available
                },
            ),
            (
                "b",
                if first == 1 {
                    Health::AuthFailed
                } else {
                    Health::Available
                },
            ),
        ]);
        let next = r.select(&healthy, &keyed("conv-1")).expect("failover");
        assert_ne!(next, first);
    }

    #[test]
    fn test_dynamic_strategy_switch() {
        let p = pool(&[("a", Health::Available), ("b", Health::Available)]);
        let r = Router::new(Strategy::StrictRoundRobin);
        let hint = SessionHint::default();

        assert_eq!(r.strategy(), Strategy::StrictRoundRobin);

        // 1. Send requests across 2 candidates under StrictRoundRobin: observe round-robin rotation.
        let mut last_seq = 0u64;
        let first = p[r.select(&p, &hint).expect("select 1")].id().to_string();
        let seq1 = r.state.lock().unwrap().seq;
        assert!(seq1 > last_seq, "sequence must increase monotonically");
        last_seq = seq1;

        let second = p[r.select(&p, &hint).expect("select 2")].id().to_string();
        let seq2 = r.state.lock().unwrap().seq;
        assert!(seq2 > last_seq, "sequence must increase monotonically");
        last_seq = seq2;

        assert_ne!(
            first, second,
            "must alternate between a and b under RoundRobin"
        );

        let third = p[r.select(&p, &hint).expect("select 3")].id().to_string();
        let seq3 = r.state.lock().unwrap().seq;
        assert!(seq3 > last_seq, "sequence must increase monotonically");
        last_seq = seq3;
        assert_eq!(
            third, first,
            "must wrap around to the first candidate under RoundRobin"
        );

        let fourth = p[r.select(&p, &hint).expect("select 4")].id().to_string();
        let seq4 = r.state.lock().unwrap().seq;
        assert!(seq4 > last_seq, "sequence must increase monotonically");
        last_seq = seq4;
        assert_eq!(
            fourth, second,
            "must alternate back to the second candidate under RoundRobin"
        );

        // 2. Call router.set_strategy(Strategy::FillFirst).
        r.set_strategy(Strategy::FillFirst);

        // Stale state probe: immediately returns the newly set Strategy without cached delay.
        assert_eq!(
            r.strategy(),
            Strategy::FillFirst,
            "strategy getter must immediately reflect FillFirst"
        );

        // 3. Send subsequent requests: observe immediate switch to pinning to the first candidate (FillFirst behavior).
        for i in 0..6 {
            let picked = p[r.select(&p, &hint).expect("select under FillFirst")]
                .id()
                .to_string();
            let curr_seq = r.state.lock().unwrap().seq;
            assert!(
                curr_seq > last_seq,
                "sequence must remain monotonic across strategy switch (iteration {i})"
            );
            last_seq = curr_seq;
            assert_eq!(
                picked, "a",
                "FillFirst must always pin to the first available member (iteration {i})"
            );
        }

        // 4. Switch back to StrictRoundRobin: ensure immediate effect and sequence monotonicity.
        r.set_strategy(Strategy::StrictRoundRobin);
        assert_eq!(
            r.strategy(),
            Strategy::StrictRoundRobin,
            "strategy getter must immediately reflect StrictRoundRobin"
        );

        // Under StrictRoundRobin, 'b' was served less recently than 'a' (which was picked 6 times),
        // so 'b' must be picked next.
        let next = p[r
            .select(&p, &hint)
            .expect("select under resumed RoundRobin")]
        .id()
        .to_string();
        let curr_seq = r.state.lock().unwrap().seq;
        assert!(
            curr_seq > last_seq,
            "sequence must remain monotonic when switching back to RoundRobin"
        );
        assert_eq!(
            next, "b",
            "resumed RoundRobin must pick candidate b because candidate a was repeatedly served"
        );
    }

    fn active_set(ids: &[&str]) -> std::collections::BTreeSet<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    #[test]
    fn retain_members_drops_last_served_and_affinity_for_removed_ids() {
        let p = avail3();
        let r = Router::new(Strategy::StrictRoundRobin);
        assert_eq!(p[r.select(&p, &keyed("conv-a")).expect("conv-a")].id(), "a");
        assert_eq!(p[r.select(&p, &keyed("conv-b")).expect("conv-b")].id(), "b");
        assert_eq!(p[r.select(&p, &keyed("conv-c")).expect("conv-c")].id(), "c");
        assert_eq!(r.bound_affinity_member("conv-b").as_deref(), Some("b"));

        let removed = r.retain_members(&active_set(&["a", "c"]));
        assert_eq!(
            removed, 1,
            "only the removed member's bookkeeping is pruned"
        );

        let mut tracked = r.tracked_members();
        tracked.sort();
        assert_eq!(tracked, vec!["a".to_string(), "c".to_string()]);
        assert_eq!(
            r.bound_affinity_member("conv-b"),
            None,
            "affinity bound to a removed member must be dropped"
        );
        assert_eq!(r.bound_affinity_member("conv-a").as_deref(), Some("a"));
        assert_eq!(r.bound_affinity_member("conv-c").as_deref(), Some("c"));
    }

    #[test]
    fn retain_members_preserves_seq_monotonicity_and_rotation() {
        let p = avail3();
        let r = Router::new(Strategy::StrictRoundRobin);
        let hint = SessionHint::default();
        for _ in 0..3 {
            r.select(&p, &hint).expect("seed rotation");
        }
        let seq_before = r.state.lock().unwrap().seq;

        let removed = r.retain_members(&active_set(&["b", "c"]));
        assert_eq!(removed, 1);
        assert_eq!(
            r.state.lock().unwrap().seq,
            seq_before,
            "retain must not touch the fairness sequence"
        );

        let live = pool(&[("b", Health::Available), ("c", Health::Available)]);
        let next = r.select(&live, &hint).expect("rotate after retain");
        assert_eq!(
            live[next].id(),
            "b",
            "smallest last_served_seq is served first"
        );
        assert_eq!(r.state.lock().unwrap().seq, seq_before + 1);
        let next2 = r.select(&live, &hint).expect("rotate after retain");
        assert_eq!(live[next2].id(), "c");
    }

    #[test]
    fn retain_members_keeps_live_ids() {
        let p = avail3();
        let r = Router::new(Strategy::StrictRoundRobin);
        r.select(&p, &keyed("keep-1"));
        r.select(&p, &keyed("keep-2"));
        r.select(&p, &keyed("keep-3"));

        let removed = r.retain_members(&active_set(&["a", "b", "c"]));
        assert_eq!(removed, 0, "nothing is pruned when every id is live");

        let mut tracked = r.tracked_members();
        tracked.sort();
        assert_eq!(
            tracked,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert_eq!(r.bound_affinity_member("keep-1").as_deref(), Some("a"));
        assert_eq!(r.bound_affinity_member("keep-3").as_deref(), Some("c"));
    }

    #[test]
    fn scoped_keys_keep_one_sessions_route_pins_independent() {
        // One session id, two route scopes: pinning the provider-A scope must
        // never clobber the provider-B scope for the same conversation.
        let r = Router::new(Strategy::FillFirst);
        let a = pool(&[("a1", Health::Disabled), ("a2", Health::Available)]);
        let b = pool(&[("b1", Health::Available)]);
        let scope_a = keyed("caller-1|codex|gpt-5.6-sol|session-S");
        let scope_b = keyed("caller-1|claude|claude-sonnet-4-6|session-S");

        assert_eq!(r.select(&a, &scope_a).map(|i| a[i].id()), Some("a2"));
        assert_eq!(r.select(&b, &scope_b).map(|i| b[i].id()), Some("b1"));

        // provider-A comes back: its own scope still points at a2, so the
        // recovery is A2 rather than the fill-first first member a1.
        let a_restored = pool(&[("a1", Health::Available), ("a2", Health::Available)]);
        assert_eq!(
            r.select(&a_restored, &scope_a).map(|i| a_restored[i].id()),
            Some("a2")
        );
        // and the untouched scope still points at b1.
        assert_eq!(r.select(&b, &scope_b).map(|i| b[i].id()), Some("b1"));
        assert_eq!(
            r.bound_affinity_member("caller-1|codex|gpt-5.6-sol|session-S")
                .as_deref(),
            Some("a2")
        );
        assert_eq!(
            r.bound_affinity_member("caller-1|claude|claude-sonnet-4-6|session-S")
                .as_deref(),
            Some("b1")
        );
    }

    #[test]
    fn new_scope_still_follows_fill_first_order_then_pins() {
        let r = Router::new(Strategy::FillFirst);
        let p = pool(&[("a1", Health::Available), ("a2", Health::Available)]);
        let scope = keyed("caller-1|codex|gpt-5.6-sol|session-S");
        // A scope with no binding keeps the configured fill-first order.
        assert_eq!(r.select(&p, &scope).map(|i| p[i].id()), Some("a1"));
        assert_eq!(r.select(&p, &scope).map(|i| p[i].id()), Some("a1"));

        // Another scope over the same pool is independent, not a second look.
        let other = keyed("caller-2|codex|gpt-5.6-sol|session-S");
        assert_eq!(r.select(&p, &other).map(|i| p[i].id()), Some("a1"));
        assert_eq!(
            r.bound_affinity_member("caller-1|codex|gpt-5.6-sol|session-S")
                .as_deref(),
            Some("a1")
        );
    }

    #[test]
    fn unavailable_binding_rebinds_only_its_own_scope() {
        let r = Router::new(Strategy::FillFirst);
        let p = pool(&[("a1", Health::Available), ("a2", Health::Available)]);
        let scope = keyed("caller-1|codex|gpt-5.6-sol|session-S");
        assert_eq!(r.select(&p, &scope).map(|i| p[i].id()), Some("a1"));

        // a1 goes unavailable: the scope falls back to the next eligible
        // member and is rebound there, leaving every other scope alone.
        let degraded = pool(&[("a1", Health::Disabled), ("a2", Health::Available)]);
        assert_eq!(
            r.select(&degraded, &scope).map(|i| degraded[i].id()),
            Some("a2")
        );
        assert_eq!(
            r.bound_affinity_member("caller-1|codex|gpt-5.6-sol|session-S")
                .as_deref(),
            Some("a2")
        );
        assert_eq!(
            r.bound_affinity_member("caller-9|codex|gpt-5.6-sol|session-S"),
            None,
            "a fallback must not invent a binding for another scope"
        );
    }

    #[test]
    fn scoped_affinity_storage_stays_bounded() {
        // Scope keys multiply per caller/route/model, so prove the idle window
        // still caps the map instead of letting it grow with every new scope.
        let r = Router::new(Strategy::FillFirst);
        let p = pool(&[("a1", Health::Available), ("a2", Health::Available)]);
        for i in 0..(AFFINITY_MAX_IDLE + 2_000) {
            let _ = r.select(
                &p,
                &keyed(&format!("caller-{i}|codex|gpt-5.6-sol|session-S")),
            );
        }
        let live = r.state.lock().unwrap().affinity.len();
        assert!(
            live <= AFFINITY_MAX_IDLE as usize + 2,
            "affinity map must stay inside the idle window, got {live} entries"
        );
    }

    #[test]
    fn router_after_retain_never_records_a_removed_member() {
        let r = Router::new(Strategy::FillFirst);
        r.retain_members(&active_set(&["b"]));

        // A stale caller still holds the removed member's entry.
        let stale = pool(&[("a", Health::Available)]);
        let idx = r
            .select(&stale, &SessionHint::default())
            .expect("selection still returns the healthy stale member");
        assert_eq!(stale[idx].id(), "a");
        assert!(
            r.tracked_members().is_empty(),
            "a member outside the activated pool must never be recorded"
        );
    }
}
