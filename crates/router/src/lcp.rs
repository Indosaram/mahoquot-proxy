//! Bounded, caller/provider/model-scoped conversation trajectories.
//! Fingerprinting stays at the protocol boundary; this module has no I/O.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prepared {
    pub turns: Vec<String>,
    pub tail: Vec<String>,
    pub environment: String,
    pub min_prefix: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub max_turns: usize,
    pub max_groups: usize,
    pub max_prefixes: usize,
    pub ttl_ms: i64,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            max_turns: 1024,
            max_groups: 4096,
            max_prefixes: 262144,
            ttl_ms: 3_600_000,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Binding {
    pub session: String,
    pub parent: Option<String>,
    pub member: String,
}

#[derive(Debug)]
struct Group {
    namespace: String,
    prepared: Prepared,
    binding: Binding,
    expires: i64,
    access: u64,
}

#[derive(Debug, Default)]
pub struct Matcher {
    groups: Vec<Group>,
    access: u64,
}

impl Matcher {
    pub fn resolve(
        &mut self,
        namespace: &str,
        prepared: &Prepared,
        bounds: Bounds,
        now: i64,
        session_id: impl Fn(&[String], Option<&str>) -> String,
    ) -> Option<Binding> {
        self.groups.retain(|group| now < group.expires);
        while self.groups.len() > bounds.max_groups
            || self
                .groups
                .iter()
                .map(|group| group.prepared.turns.len())
                .sum::<usize>()
                > bounds.max_prefixes
        {
            let Some(index) = self
                .groups
                .iter()
                .enumerate()
                .min_by_key(|(_, group)| group.access)
                .map(|(index, _)| index)
            else {
                break;
            };
            self.groups.remove(index);
        }
        if prepared.min_prefix == 0
            || prepared.turns.len() < prepared.min_prefix
            || prepared.turns.len() > bounds.max_turns
            || prepared.turns.len() > bounds.max_prefixes
        {
            return None;
        }
        let cold = || Binding {
            session: session_id(&prepared.turns[..prepared.min_prefix], None),
            parent: None,
            member: String::new(),
        };
        let best = self
            .groups
            .iter()
            .filter(|g| g.namespace == namespace)
            .filter_map(|g| {
                let common = g
                    .prepared
                    .turns
                    .iter()
                    .zip(&prepared.turns)
                    .take_while(|(a, b)| a == b)
                    .count();
                (common >= prepared.min_prefix.max(g.prepared.min_prefix)).then_some((g, common))
            })
            .max_by_key(|(g, common)| (*common, g.prepared.turns.len(), g.access));
        if let Some((group, common)) = best {
            let binding = group.binding.clone();
            let length = group.prepared.turns.len();
            let access = group.access;
            if let Some(group) = self.groups.iter_mut().find(|group| group.access == access) {
                group.expires = now.saturating_add(bounds.ttl_ms.max(0));
            }
            if common < length && common < prepared.turns.len() {
                let parent = session_id(&prepared.turns[..common], None);
                return Some(Binding {
                    session: session_id(&prepared.turns[..=common], None),
                    parent: Some(parent),
                    member: binding.member,
                });
            }
            return Some(binding);
        }
        // Compaction may inherit only an unambiguous trailing overlap with the
        // same environment. Repeated two-turn tails (>16 trajectories) refuse.
        let mut candidates = Vec::new();
        let mut best_overlap = 0;
        for group in self
            .groups
            .iter()
            .filter(|g| g.namespace == namespace && g.prepared.environment == prepared.environment)
        {
            let mut overlap = 0;
            for end in 1..prepared.tail.len() {
                let count = prepared.tail[..=end]
                    .iter()
                    .rev()
                    .zip(group.prepared.tail.iter().rev())
                    .take_while(|(a, b)| a == b)
                    .count();
                let candidate_start =
                    prepared.turns.len().saturating_sub(prepared.tail.len()) + end + 1 - count;
                let parent_start = group.prepared.turns.len().saturating_sub(count);
                if count >= 2
                    && candidate_start <= prepared.min_prefix
                    && (candidate_start > 0 || parent_start > 0)
                {
                    overlap = overlap.max(count);
                }
            }
            if overlap > best_overlap {
                candidates.clear();
                best_overlap = overlap;
            }
            if overlap >= 2 && overlap == best_overlap {
                candidates.push(group);
            }
        }
        if candidates.len() > 16 {
            return Some(cold());
        }
        let sessions: std::collections::BTreeSet<_> = candidates
            .iter()
            .map(|g| g.binding.session.as_str())
            .collect();
        if sessions.len() == 1 {
            let group = candidates[0];
            return Some(Binding {
                session: session_id(&prepared.turns, Some(&group.binding.session)),
                parent: Some(group.binding.session.clone()),
                member: group.binding.member.clone(),
            });
        }
        Some(cold())
    }

    pub fn bind(
        &mut self,
        namespace: &str,
        prepared: Prepared,
        binding: Binding,
        bounds: Bounds,
        now: i64,
    ) {
        if prepared.min_prefix == 0
            || prepared.turns.len() > bounds.max_turns
            || prepared.turns.len() > bounds.max_prefixes
        {
            return;
        }
        self.groups.retain(|g| {
            now < g.expires && !(g.namespace == namespace && g.prepared.turns == prepared.turns)
        });
        self.access = self.access.wrapping_add(1);
        self.groups.push(Group {
            namespace: namespace.into(),
            prepared,
            binding,
            expires: now.saturating_add(bounds.ttl_ms.max(0)),
            access: self.access,
        });
        while self.groups.len() > bounds.max_groups
            || self
                .groups
                .iter()
                .map(|g| g.prepared.turns.len())
                .sum::<usize>()
                > bounds.max_prefixes
        {
            let Some(index) = self
                .groups
                .iter()
                .enumerate()
                .min_by_key(|(_, g)| g.access)
                .map(|(i, _)| i)
            else {
                break;
            };
            self.groups.remove(index);
        }
    }

    pub fn retain_members(&mut self, active: &std::collections::BTreeSet<String>) {
        self.groups.retain(|g| active.contains(&g.binding.member));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(turns: &[&str]) -> Prepared {
        Prepared {
            turns: turns.iter().map(|v| (*v).into()).collect(),
            tail: turns.iter().map(|v| (*v).into()).collect(),
            environment: "system".into(),
            min_prefix: 2,
        }
    }
    fn id(turns: &[String], parent: Option<&str>) -> String {
        format!("{}:{}", parent.unwrap_or("root"), turns.join("/"))
    }
    fn seed(
        matcher: &mut Matcher,
        namespace: &str,
        turns: &[&str],
        now: i64,
        bounds: Bounds,
    ) -> Binding {
        let request = prepared(turns);
        let mut binding = matcher
            .resolve(namespace, &request, bounds, now, id)
            .unwrap();
        binding.member = "second".into();
        matcher.bind(namespace, request, binding.clone(), bounds, now);
        binding
    }

    #[test]
    fn growth_retains_binding_and_sibling_forks_have_distinct_sessions() {
        let mut matcher = Matcher::default();
        let bounds = Bounds::default();
        let root = seed(
            &mut matcher,
            "caller/provider/model",
            &["s", "u", "a", "left"],
            0,
            bounds,
        );
        let grown = matcher
            .resolve(
                "caller/provider/model",
                &prepared(&["s", "u", "a", "left", "next"]),
                bounds,
                1,
                id,
            )
            .unwrap();
        assert_eq!(grown.session, root.session);
        let right = matcher
            .resolve(
                "caller/provider/model",
                &prepared(&["s", "u", "a", "right"]),
                bounds,
                1,
                id,
            )
            .unwrap();
        let other = matcher
            .resolve(
                "caller/provider/model",
                &prepared(&["s", "u", "a", "other"]),
                bounds,
                1,
                id,
            )
            .unwrap();
        assert_ne!(right.session, other.session);
        assert_ne!(right.session, right.parent.clone().unwrap());
        assert_eq!(right.parent, other.parent);
        assert_eq!(right.member, "second");
    }

    #[test]
    fn namespaces_expiry_removal_and_bounds_prevent_stale_inheritance() {
        let mut matcher = Matcher::default();
        let bounds = Bounds {
            max_groups: 1,
            ttl_ms: 10,
            ..Bounds::default()
        };
        seed(&mut matcher, "a/provider/model", &["s", "u"], 0, bounds);
        let isolated = matcher
            .resolve("b/provider/model", &prepared(&["s", "u"]), bounds, 1, id)
            .unwrap();
        assert!(isolated.member.is_empty());
        let expired = matcher
            .resolve("a/provider/model", &prepared(&["s", "u"]), bounds, 10, id)
            .unwrap();
        assert!(expired.member.is_empty());
        seed(&mut matcher, "a/provider/model", &["s", "u"], 10, bounds);
        seed(&mut matcher, "b/provider/model", &["s", "v"], 10, bounds);
        assert_eq!(matcher.groups.len(), 1);
        matcher.retain_members(&std::collections::BTreeSet::new());
        assert!(matcher.groups.is_empty());
        assert!(matcher
            .resolve(
                "a",
                &prepared(&["s", "u", "a"]),
                Bounds {
                    max_turns: 2,
                    ..bounds
                },
                11,
                id
            )
            .is_none());
    }

    #[test]
    fn compaction_requires_matching_environment_and_unambiguous_tail() {
        let mut matcher = Matcher::default();
        let bounds = Bounds::default();
        let root = seed(
            &mut matcher,
            "scope",
            &["s", "old", "a", "tail1", "tail2"],
            0,
            bounds,
        );
        let request = prepared(&["s", "summary", "tail1", "tail2"]);
        let result = matcher.resolve("scope", &request, bounds, 1, id).unwrap();
        assert_eq!(result.parent.as_deref(), Some(root.session.as_str()));
        let mut different = request.clone();
        different.environment = "other".into();
        assert!(matcher
            .resolve("scope", &different, bounds, 1, id)
            .unwrap()
            .member
            .is_empty());
    }
}
