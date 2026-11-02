#[cfg(test)]
use super::policy::{file_rules, integer, weight};
use super::windows::probed;
use super::*;

/// A credential selection may pick, with what the pick reads about it.
#[derive(Clone, Copy)]
pub(crate) struct Candidate<'a> {
    pub credential: &'a std::sync::Arc<Credential>,
    /// The model key its cooldowns live under for this selection.
    pub model: &'a str,
    pub provider: &'a str,
    pub priority: i64,
    pub weight: i64,
    pub windows: Windows,
}

#[derive(Default)]
pub(super) struct Rotation {
    last: String,
    weights: HashMap<String, i64>,
    current: HashMap<String, i64>,
}

/// A selection's affinity keys: the session, and its parent or alias when distinct.
pub(super) struct SessionKeys {
    pub(super) primary: crate::affinity::Key,
    fallback: Option<crate::affinity::Key>,
    /// The raw parent/alias session, for Go `isSubagentSession`.
    parent: String,
}

pub(super) fn session_keys(selection: &Selection) -> Option<SessionKeys> {
    let primary = selection.session.as_deref().filter(|s| !s.is_empty())?;
    let scope = selection.provider_keys().join(",");
    let model = canonical_model(&selection.model).to_owned();
    let parent = selection.session_parent.clone().unwrap_or_default();
    let fallback = (!parent.is_empty() && parent != primary).then(|| (scope.clone(), model.clone(), parent.clone()));
    Some(SessionKeys {
        primary: (scope, model, primary.to_owned()),
        fallback,
        parent,
    })
}

impl Scheduler {
    /// Picks among ready candidates, each paired with its provider key (Go
    /// `SessionAffinitySelector.Pick` over the configured selector).
    ///
    /// An established binding outranks credential priority; a session's parent (or
    /// prompt-cache conversation alias) binding is inherited by forks and, when
    /// `session-affinity-subagents` allows, by subagents.
    #[cfg(test)]
    pub fn pick<'a>(
        &mut self,
        candidates: &[(&'a Credential, &str)],
        selection: &Selection,
        policy: &Policy,
        now: Instant,
    ) -> Option<&'a Credential> {
        self.pick_ranked(candidates, selection, policy, &|_| Windows::default(), now)
    }

    /// [`Self::pick`] with each credential's usage windows, for `soonest-reset`.
    #[cfg(test)]
    pub fn pick_ranked<'a>(
        &mut self,
        candidates: &[(&'a Credential, &str)],
        selection: &Selection,
        policy: &Policy,
        ranks: &Ranks<'_>,
        now: Instant,
    ) -> Option<&'a Credential> {
        self.pick_session(candidates, selection, policy, ranks, now)
            .map(|(c, _)| c)
    }

    /// [`Self::pick_candidate`] over credentials paired with their provider keys.
    #[cfg(test)]
    pub fn pick_session<'a>(
        &mut self,
        candidates: &[(&'a Credential, &str)],
        selection: &Selection,
        policy: &Policy,
        ranks: &Ranks<'_>,
        now: Instant,
    ) -> Option<(&'a Credential, Option<crate::lcp::Match>)> {
        let owned: Vec<std::sync::Arc<Credential>> = candidates
            .iter()
            .map(|(c, _)| std::sync::Arc::new((*c).clone()))
            .collect();
        let ready: Vec<Candidate<'_>> = owned
            .iter()
            .zip(candidates)
            .map(|(c, (_, provider))| Candidate {
                credential: c,
                model: &selection.model,
                provider,
                priority: integer(c, "priority").unwrap_or(0),
                weight: weight(c),
                windows: ranks(c),
            })
            .collect();
        let (picked, lcp) = self.pick_candidate(&ready, selection, policy, now)?;
        candidates
            .iter()
            .find(|(c, _)| c.id == picked.credential.id)
            .map(|(c, _)| (*c, lcp))
    }

    /// Picks one of `candidates`, with the LCP binding of a request without an explicit
    /// session (Go `pickLCP`): its session identity, lineage and access generation.
    pub(crate) fn pick_candidate<'c, 'a>(
        &mut self,
        candidates: &'c [Candidate<'a>],
        selection: &Selection,
        policy: &Policy,
        now: Instant,
    ) -> Option<(&'c Candidate<'a>, Option<crate::lcp::Match>)> {
        if candidates.is_empty() {
            return None;
        }
        if let Some(request) = selection.lcp.as_deref().filter(|_| self.affinity(policy)) {
            let namespace = request.namespace(&selection.model);
            // A known trajectory stays on its credential while that one is available.
            if let Some(found) = self.lcp.find(&namespace, request.prepared(), now)
                && let Some(c) = candidates.iter().find(|c| c.credential.id == found.auth)
            {
                return Some((c, Some(found)));
            }
            let picked = self.pick_unbound(candidates, selection, policy)?;
            let bound = self
                .lcp
                .bind(&namespace, request.prepared(), &picked.credential.id, now);
            return Some((picked, bound));
        }
        self.pick_legacy(candidates, selection, policy, now).map(|c| (c, None))
    }

    /// Go `SessionAffinitySelector.Pick` after the LCP step.
    fn pick_legacy<'c, 'a>(
        &mut self,
        candidates: &'c [Candidate<'a>],
        selection: &Selection,
        policy: &Policy,
        now: Instant,
    ) -> Option<&'c Candidate<'a>> {
        let ttl = policy.session_affinity_ttl;
        let Some(keys) = self.affinity(policy).then(|| session_keys(selection)).flatten() else {
            return self.pick_unbound(candidates, selection, policy);
        };
        self.affinity.sweep(now, ttl);
        let fork = selection.session_fork;
        let subagent = !fork && cpa_common::session::is_subagent_session(&keys.primary.2, &keys.parent);
        let bind_keys = match &keys.fallback {
            Some(fallback) if !subagent && !fork => vec![keys.primary.clone(), fallback.clone()],
            _ => vec![keys.primary.clone()],
        };
        let find = |id: &str| candidates.iter().find(|c| c.credential.id == id);
        let reuse = match self.affinity.get_and_refresh(&keys.primary, now, ttl) {
            // A bound credential that is no longer available is replaced, not inherited.
            Some(id) => find(&id),
            None => keys
                .fallback
                .as_ref()
                .and_then(|fallback| self.affinity.get(fallback, now))
                .filter(|_| !subagent || policy.session_affinity_subagents)
                .and_then(|id| find(&id)),
        };
        let picked = match reuse {
            Some(c) => c,
            None => self.pick_unbound(candidates, selection, policy)?,
        };
        self.affinity.bind(&picked.credential.id, &bind_keys, now, ttl);
        Some(picked)
    }

    /// Go `SessionAffinitySelector.OnResult`: success refreshes the session's bindings to
    /// this credential, a credential-attributed failure releases them. Request-scoped
    /// and transport failures leave them alone.
    ///
    /// A request without an explicit session refreshes its LCP sequence on success; a
    /// credential failure removes that exact sequence unless a newer request refreshed
    /// it after `lcp`'s generation. Such a request never touches the session cache.
    #[cfg(test)]
    pub fn session_result(
        &mut self,
        c: &Credential,
        selection: &Selection,
        outcome: &Outcome,
        policy: &Policy,
        lcp: Option<&crate::lcp::Match>,
        now: Instant,
    ) {
        self.session_result_with(c, file_rules(c).as_deref(), selection, outcome, policy, lcp, now);
    }

    /// [`Self::session_result`] with the credential's own error rules already parsed.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn session_result_with(
        &mut self,
        c: &Credential,
        rules: Option<&[ErrorRule]>,
        selection: &Selection,
        outcome: &Outcome,
        policy: &Policy,
        lcp: Option<&crate::lcp::Match>,
        now: Instant,
    ) {
        if !self.affinity(policy) {
            return;
        }
        let success = match outcome {
            Outcome::Success => true,
            Outcome::Failure(error) if policy.error_action_with(rules, c, error).cooldown => false,
            _ => return,
        };
        if let Some(request) = selection.lcp.as_deref() {
            let namespace = request.namespace(&selection.model);
            if success {
                self.lcp.touch(&namespace, request.prepared(), &c.id, now);
            } else {
                let generation = lcp.map_or(0, |m| m.access);
                self.lcp
                    .remove(&namespace, &request.prepared().fingerprints, &c.id, generation, now);
            }
            if lcp.is_some() {
                return;
            }
        }
        let Some(keys) = session_keys(selection) else {
            return;
        };
        let mut targets = vec![keys.primary.clone()];
        if let Some(fallback) = keys.fallback
            && !cpa_common::session::is_subagent_session(&keys.primary.2, &keys.parent)
        {
            targets.push(fallback);
        }
        for key in &targets {
            if success {
                self.affinity.touch(key, &c.id, now, policy.session_affinity_ttl);
            } else {
                self.affinity.compare_and_delete(key, &c.id);
            }
        }
    }

    fn pick_unbound<'c, 'a>(
        &mut self,
        candidates: &'c [Candidate<'a>],
        selection: &Selection,
        policy: &Policy,
    ) -> Option<&'c Candidate<'a>> {
        let model = canonical_model(&selection.model).to_owned();
        let provider_keys = selection.provider_keys();
        let providers_key = provider_keys.join(",");
        let tier = candidates.iter().map(|c| c.priority).max()?;
        let mut ready: Vec<&Candidate<'a>> = candidates.iter().filter(|c| c.priority == tier).collect();
        ready.sort_by(|a, b| a.credential.id.cmp(&b.credential.id));
        // Providers that have ready candidates, in the request's provider order.
        let mut groups: Vec<(&str, Vec<&Candidate<'a>>)> = Vec::new();
        for provider in provider_keys
            .iter()
            .map(String::as_str)
            .chain(ready.iter().map(|c| c.provider))
        {
            if groups.iter().any(|(p, _)| *p == provider) {
                continue;
            }
            let members: Vec<&Candidate<'a>> = ready.iter().copied().filter(|c| c.provider == provider).collect();
            if !members.is_empty() {
                groups.push((provider, members));
            }
        }
        if self.rotations.len() >= 4096 {
            self.rotations.clear();
            self.cursors.clear();
        }
        let (provider, members) = match groups.as_slice() {
            [] => return None,
            [only] => return self.pick_within(only.0, &model, &only.1, policy.strategy),
            [first, ..] => first,
        };
        match policy.strategy {
            Strategy::FillFirst => members.first().copied(),
            // Like fill-first: the first provider group, then its soonest reset.
            Strategy::SoonestReset => self.pick_within(provider, &model, members, Strategy::SoonestReset),
            Strategy::WeightedRoundRobin => {
                self.pick_within(&providers_key, &model, &ready, Strategy::WeightedRoundRobin)
            }
            Strategy::RoundRobin => {
                // Go `pickMixed`: a cursor over provider segments sized by their ready
                // counts, then that provider's own round-robin.
                let total: usize = groups.iter().map(|(_, m)| m.len()).sum();
                let cursor = self.cursors.entry((providers_key.clone(), model.clone())).or_default();
                let slot = *cursor % total;
                *cursor = slot + 1;
                let mut start = 0;
                let (provider, members) = groups
                    .iter()
                    .find(|(_, members)| {
                        start += members.len();
                        slot < start
                    })
                    .or(groups.first())?;
                self.pick_within(provider, &model, members, Strategy::RoundRobin)
            }
        }
    }

    fn pick_within<'c, 'a>(
        &mut self,
        scope: &str,
        model: &str,
        members: &[&'c Candidate<'a>],
        strategy: Strategy,
    ) -> Option<&'c Candidate<'a>> {
        let state = self.rotations.entry((scope.to_owned(), model.to_owned())).or_default();
        let picked = match strategy {
            Strategy::FillFirst => return members.first().copied(),
            Strategy::SoonestReset => {
                let probes = &mut self.probes;
                let ranked: Vec<_> = members
                    .iter()
                    .map(|c| {
                        let windows = c.windows;
                        let order = windows.order(probed(probes.get(&c.credential.id), windows.rolled_over));
                        (order, windows.rolled_over, *c)
                    })
                    .collect();
                let best = ranked.iter().map(|(order, ..)| *order).min()?;
                // Equal ranks take turns.
                let tied: Vec<_> = ranked.iter().filter(|(o, ..)| *o == best).collect();
                let (order, rolled_over, picked) = *tied
                    .iter()
                    .find(|(.., c)| c.credential.id > state.last)
                    .or(tied.first())
                    .copied()?;
                if order.1 == 0 {
                    // Its one probe, spent whatever the answer: the next pick ranks it
                    // by what the answer reports, if anything.
                    let markers = probes.entry(picked.credential.id.clone()).or_default();
                    if let Some(r) = rolled_over {
                        markers.insert(r.window, r.at);
                    }
                }
                picked
            }
            Strategy::RoundRobin => members
                .iter()
                .find(|c| c.credential.id > state.last)
                .or(members.first())
                .copied()?,
            Strategy::WeightedRoundRobin => {
                if members
                    .iter()
                    .any(|c| state.weights.get(&c.credential.id).is_some_and(|w| *w != c.weight))
                {
                    state.current.clear();
                }
                if state.weights.len() > 1024 || state.current.len() > 1024 {
                    state
                        .weights
                        .retain(|id, _| members.iter().any(|c| c.credential.id == *id));
                    state
                        .current
                        .retain(|id, _| members.iter().any(|c| c.credential.id == *id));
                }
                let mut total = 0i64;
                let mut best: Option<(i64, &'c Candidate<'a>)> = None;
                for c in members {
                    let id = &c.credential.id;
                    match state.weights.get_mut(id) {
                        Some(w) => *w = c.weight,
                        None => {
                            state.weights.insert(id.clone(), c.weight);
                        }
                    }
                    if !state.current.contains_key(id) {
                        state.current.insert(id.clone(), 0);
                    }
                    let current = state.current.get_mut(id).map_or(0, |current| {
                        *current = current.saturating_add(c.weight);
                        *current
                    });
                    total = total.saturating_add(c.weight);
                    if best.is_none_or(|(b, _)| current > b) {
                        best = Some((current, c));
                    }
                }
                let (_, picked) = best?;
                if let Some(current) = state.current.get_mut(&picked.credential.id) {
                    *current = current.saturating_sub(total);
                }
                return Some(picked);
            }
        };
        state.last.clone_from(&picked.credential.id);
        Some(picked)
    }

    #[cfg(test)]
    pub fn admits(&self, c: &Credential, policy: &Policy) -> bool {
        policy.admits_weight(weight(c))
    }
}
