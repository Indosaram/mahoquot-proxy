pub const EXHAUSTION_ENTER_PERCENT: u8 = 3;
pub const EXHAUSTION_RECOVER_PERCENT: u8 = 5;
pub const MINIMUM_HOLD_SECS: i64 = 10 * 60;
pub const SWITCH_MARGIN_SECS: i64 = 15 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowActivity {
    Active,
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate<'a> {
    pub key: &'a str,
    pub manually_disabled: bool,
    pub manual_priority: Option<u32>,
    pub remaining_percent: Option<u8>,
    pub reset_at_unix: Option<i64>,
    pub window_activity: WindowActivity,
    pub exhausted_since_unix: Option<i64>,
}

pub fn select_candidate<'a>(
    candidates: &'a [Candidate<'a>],
    now_unix: i64,
) -> Option<&'a Candidate<'a>> {
    let eligible = candidates.iter().filter(|candidate| {
        if candidate.manually_disabled || candidate.remaining_percent.is_none() {
            return false;
        }
        let remaining = candidate.remaining_percent.unwrap_or(0);
        if remaining <= EXHAUSTION_ENTER_PERCENT {
            return false;
        }
        if candidate.exhausted_since_unix.is_some() && remaining <= EXHAUSTION_RECOVER_PERCENT {
            return false;
        }
        true
    });

    let mut ranked: Vec<_> = eligible.collect();
    if ranked.is_empty() {
        return None;
    }
    if ranked
        .iter()
        .any(|candidate| candidate.manual_priority.is_some())
    {
        ranked.retain(|candidate| candidate.manual_priority.is_some());
    }
    ranked.sort_by_key(|candidate| {
        let window_tier = match candidate.window_activity {
            WindowActivity::Active => 0,
            WindowActivity::Idle => 1,
        };
        let reset = candidate
            .reset_at_unix
            .filter(|reset| *reset > now_unix)
            .unwrap_or(i64::MAX);
        (window_tier, reset, candidate.key)
    });

    let best = ranked[0];
    if let Some(incumbent) = ranked
        .iter()
        .copied()
        .min_by_key(|candidate| (candidate.manual_priority.unwrap_or(u32::MAX), candidate.key))
        .filter(|candidate| candidate.manual_priority < best.manual_priority)
    {
        let incumbent_reset = incumbent.reset_at_unix.unwrap_or(i64::MAX);
        let best_reset = best.reset_at_unix.unwrap_or(i64::MAX);
        if best_reset.saturating_add(SWITCH_MARGIN_SECS) >= incumbent_reset {
            return Some(incumbent);
        }
    }
    Some(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    fn candidate(key: &'static str) -> Candidate<'static> {
        Candidate {
            key,
            manually_disabled: false,
            manual_priority: None,
            remaining_percent: Some(50),
            reset_at_unix: Some(NOW + 60 * 60),
            window_activity: WindowActivity::Active,
            exhausted_since_unix: None,
        }
    }

    fn selected_key<'a>(candidates: &'a [Candidate<'a>]) -> Option<&'a str> {
        select_candidate(candidates, NOW).map(|candidate| candidate.key)
    }

    #[test]
    fn selects_soonest_active_reset() {
        let mut later = candidate("later");
        later.reset_at_unix = Some(NOW + 30 * 60);
        let mut sooner = candidate("sooner");
        sooner.reset_at_unix = Some(NOW + 10 * 60);

        assert_eq!(selected_key(&[later, sooner]), Some("sooner"));
    }

    #[test]
    fn all_exhausted_is_fail_open() {
        let mut first = candidate("first");
        first.remaining_percent = Some(EXHAUSTION_ENTER_PERCENT);
        let mut second = candidate("second");
        second.remaining_percent = Some(0);

        assert_eq!(selected_key(&[first, second]), None);
    }

    #[test]
    fn manual_disabled_is_never_candidate() {
        let mut disabled = candidate("disabled");
        disabled.manually_disabled = true;
        disabled.manual_priority = Some(0);
        disabled.reset_at_unix = Some(NOW + 1);
        let enabled = candidate("enabled");

        assert_eq!(selected_key(&[disabled, enabled]), Some("enabled"));
    }

    #[test]
    fn exhausted_candidate_is_held_for_ten_minutes() {
        let mut held = candidate("held");
        held.remaining_percent = Some(EXHAUSTION_RECOVER_PERCENT);
        held.exhausted_since_unix = Some(NOW - MINIMUM_HOLD_SECS + 1);
        held.reset_at_unix = Some(NOW + 1);
        let available = candidate("available");

        assert_eq!(selected_key(&[held, available]), Some("available"));
    }

    #[test]
    fn switch_requires_fifteen_minute_margin() {
        let mut incumbent = candidate("incumbent");
        incumbent.manual_priority = Some(0);
        incumbent.reset_at_unix = Some(NOW + 30 * 60);
        let mut challenger = candidate("challenger");
        challenger.manual_priority = Some(1);
        challenger.reset_at_unix = Some(NOW + 16 * 60);

        assert_eq!(
            selected_key(&[incumbent.clone(), challenger.clone()]),
            Some("incumbent")
        );

        challenger.reset_at_unix = Some(NOW + 14 * 60);
        assert_eq!(selected_key(&[incumbent, challenger]), Some("challenger"));
    }

    #[test]
    fn exhaustion_uses_three_five_percent_hysteresis() {
        let mut still_exhausted = candidate("still-exhausted");
        still_exhausted.remaining_percent = Some(EXHAUSTION_RECOVER_PERCENT - 1);
        still_exhausted.exhausted_since_unix = Some(NOW - MINIMUM_HOLD_SECS);
        still_exhausted.reset_at_unix = Some(NOW + 1);
        let available = candidate("available");

        assert_eq!(
            selected_key(&[still_exhausted, available]),
            Some("available")
        );
    }

    #[test]
    fn active_window_precedes_idle_window() {
        let mut idle = candidate("idle");
        idle.window_activity = WindowActivity::Idle;
        idle.reset_at_unix = Some(NOW + 1);
        let active = candidate("active");

        assert_eq!(selected_key(&[idle, active]), Some("active"));
    }

    #[test]
    fn manual_priority_is_immediate() {
        let mut ordinary = candidate("ordinary");
        ordinary.reset_at_unix = Some(NOW + 1);
        let mut priority = candidate("priority");
        priority.manual_priority = Some(0);
        priority.reset_at_unix = Some(NOW + 60 * 60);

        assert_eq!(selected_key(&[ordinary, priority]), Some("priority"));
    }

    #[test]
    fn deterministic_key_breaks_ties() {
        let beta = candidate("beta");
        let alpha = candidate("alpha");

        assert_eq!(selected_key(&[beta, alpha]), Some("alpha"));
    }
}
