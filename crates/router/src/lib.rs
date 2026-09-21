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
    /// member id -> last assigned sequence number
    last_served: HashMap<String, u64>,
    /// monotonically increasing global service counter
    seq: u64,
    /// affinity key -> member id, so a conversation keeps hitting one account
    /// instead of hopping every turn. Entries carry the sequence number at which
    /// they were last used so idle sessions can be evicted.
    affinity: HashMap<String, (String, u64)>,
}

/// Sessions idle for this many selections are dropped, bounding the map on a
/// long-lived proxy without needing a timer.
const AFFINITY_MAX_IDLE: u64 = 10_000;

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
        self.strategy.store(strategy_to_u8(strategy), Ordering::SeqCst);
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
                if let Some(idx) = members.iter().position(|m| {
                    m.id() == bound_id && m.health().is_available(now_unix_ms)
                }) {
                    let seq = state.seq.wrapping_add(1);
                    state.seq = seq;
                    state.last_served.insert(bound_id.clone(), seq);
                    state.affinity.insert(key.to_string(), (bound_id, seq));
                    return Some(idx);
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
                state.last_served.insert(chosen_id.clone(), seq);

                if let Some(key) = hint.affinity_key.as_deref() {
                    state
                        .affinity
                        .insert(key.to_string(), (chosen_id, seq));
                    if state.affinity.len() > 1024 {
                        let cutoff = seq.saturating_sub(AFFINITY_MAX_IDLE);
                        state.affinity.retain(|_, (_, s)| *s >= cutoff);
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
                state.last_served.insert(chosen_id.clone(), next_seq);

                if let Some(key) = hint.affinity_key.as_deref() {
                    state
                        .affinity
                        .insert(key.to_string(), (chosen_id, next_seq));
                    if state.affinity.len() > 1024 {
                        let cutoff = next_seq.saturating_sub(AFFINITY_MAX_IDLE);
                        state.affinity.retain(|_, (_, seq)| *seq >= cutoff);
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

        assert_ne!(first, second, "must alternate between a and b under RoundRobin");

        let third = p[r.select(&p, &hint).expect("select 3")].id().to_string();
        let seq3 = r.state.lock().unwrap().seq;
        assert!(seq3 > last_seq, "sequence must increase monotonically");
        last_seq = seq3;
        assert_eq!(third, first, "must wrap around to the first candidate under RoundRobin");

        let fourth = p[r.select(&p, &hint).expect("select 4")].id().to_string();
        let seq4 = r.state.lock().unwrap().seq;
        assert!(seq4 > last_seq, "sequence must increase monotonically");
        last_seq = seq4;
        assert_eq!(fourth, second, "must alternate back to the second candidate under RoundRobin");

        // 2. Call router.set_strategy(Strategy::FillFirst).
        r.set_strategy(Strategy::FillFirst);

        // Stale state probe: immediately returns the newly set Strategy without cached delay.
        assert_eq!(r.strategy(), Strategy::FillFirst, "strategy getter must immediately reflect FillFirst");

        // 3. Send subsequent requests: observe immediate switch to pinning to the first candidate (FillFirst behavior).
        for i in 0..6 {
            let picked = p[r.select(&p, &hint).expect("select under FillFirst")].id().to_string();
            let curr_seq = r.state.lock().unwrap().seq;
            assert!(curr_seq > last_seq, "sequence must remain monotonic across strategy switch (iteration {i})");
            last_seq = curr_seq;
            assert_eq!(picked, "a", "FillFirst must always pin to the first available member (iteration {i})");
        }

        // 4. Switch back to StrictRoundRobin: ensure immediate effect and sequence monotonicity.
        r.set_strategy(Strategy::StrictRoundRobin);
        assert_eq!(r.strategy(), Strategy::StrictRoundRobin, "strategy getter must immediately reflect StrictRoundRobin");

        // Under StrictRoundRobin, 'b' was served less recently than 'a' (which was picked 6 times),
        // so 'b' must be picked next.
        let next = p[r.select(&p, &hint).expect("select under resumed RoundRobin")].id().to_string();
        let curr_seq = r.state.lock().unwrap().seq;
        assert!(curr_seq > last_seq, "sequence must remain monotonic when switching back to RoundRobin");
        assert_eq!(next, "b", "resumed RoundRobin must pick candidate b because candidate a was repeatedly served");
    }
}
