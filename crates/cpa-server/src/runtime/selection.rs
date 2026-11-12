use super::*;

#[derive(Debug)]
pub enum AcquireError {
    /// No candidate is ready. `retry_after` is set when candidates exist but are all
    /// blocked (Go `auth_unavailable`); unset means none is eligible (`auth_not_found`).
    Unavailable {
        retry_after: Option<Duration>,
        cause: Option<String>,
    },
    /// Every eligible candidate is quota-cooling for this model.
    Cooldown {
        wait: Duration,
        cause: Option<String>,
    },
    Prepare {
        id: String,
        error: ExecError,
    },
}

/// Route-model state is separate from the lease's resolved execution model.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    /// Provider keys that may serve the model, in preference order.
    pub providers: Vec<String>,
    /// Single-provider shorthand, used when `providers` is empty.
    pub provider: String,
    /// The route model (requested model after `auto` resolution, suffix kept).
    pub model: String,
    /// The session this request binds to (Go `extractSessionIDs` primary).
    pub session: Option<String>,
    /// The session's parent, or the conversation alias of a prompt-cache-key session
    /// (Go fallback ID).
    pub session_parent: Option<String>,
    /// The session forked from `session_parent` (inherits its binding).
    pub session_fork: bool,
    /// A request without an explicit session, for the LCP matcher (Go `pickLCP`). Set
    /// only with session affinity on; it then decides the binding instead of `session`.
    pub lcp: Option<Arc<crate::lcp::Request>>,
    /// The only credential this selection may use (Go `WithPinnedAuthID`).
    pub pinned: Option<String>,
    /// A route's own credential rule, checked during selection instead of excluding
    /// every credential it refuses.
    pub accept: Option<fn(&Credential) -> bool>,
    /// Credential IDs already tried in this request.
    pub exclude: Vec<String>,
    pub retry_round: usize,
}

impl Selection {
    pub fn new(provider: &str, model: &str) -> Self {
        Self {
            providers: vec![provider.to_owned()],
            model: model.to_owned(),
            ..Self::default()
        }
    }

    /// The provider keys this selection may use.
    pub fn provider_keys(&self) -> Vec<String> {
        if self.providers.is_empty() && !self.provider.is_empty() {
            vec![self.provider.clone()]
        } else {
            self.providers.clone()
        }
    }
}

/// Admission without a registry: provider match, prefix, config aliases and the
/// credential's exclusions (attributes first, then file metadata).
pub fn standalone_admission<'a>(
    selection: &'a Selection,
    policy: &'a Policy,
) -> impl Fn(&Credential) -> Option<String> + 'a {
    let providers = selection.provider_keys();
    move |c| {
        if !providers.contains(&crate::registry::provider_key(c)) {
            return None;
        }
        let route = selection.model.trim();
        let prefix = crate::registry::credential_prefix(c);
        let base = crate::scheduler::canonical_model(route);
        if !prefix.is_empty()
            && !base.starts_with(&format!("{prefix}/"))
            && (policy.force_model_prefix || base.contains('/'))
        {
            return None;
        }
        let (models, _) = crate::registry::execution_models(&HashMap::new(), c, route);
        let model = models.into_iter().next().unwrap_or_default();
        let key = crate::scheduler::canonical_model(&model).to_lowercase();
        let excluded: Vec<String> = match c.attributes.get("excluded_models").filter(|v| !v.trim().is_empty()) {
            Some(list) => list.split(',').map(|p| p.trim().to_lowercase()).collect(),
            None => c
                .metadata
                .get("excluded_models")
                .and_then(serde_json::Value::as_array)
                .map(|l| {
                    l.iter()
                        .filter_map(|v| v.as_str())
                        .map(|p| p.trim().to_lowercase())
                        .collect()
                })
                .unwrap_or_default(),
        };
        if excluded.iter().any(|p| crate::registry::wildcard(p, &key)) {
            return None;
        }
        Some(model)
    }
}

/// Decides which credentials may serve a selection and under which model key their
/// cooldowns live. `None` rejects the credential.
pub type Admit<'a> = dyn Fn(&Credential) -> Option<String> + 'a;

/// Registry admission (Go `authSupportsRouteModel`) plus a registered executor.
pub fn admission<'a>(
    registry: &'a Registry,
    aliases: &'a HashMap<String, Vec<crate::registry::OAuthAlias>>,
    selection: &'a Selection,
    executors: &'a Executors,
) -> impl Fn(&Credential) -> Option<String> + 'a {
    let providers = selection.provider_keys();
    move |c| {
        let provider = crate::registry::provider_key(c);
        if !providers.contains(&provider) || !executors.supports(&c.provider) {
            return None;
        }
        let key = crate::registry::selection_model(aliases, c, &selection.model);
        let route = crate::scheduler::canonical_model(&selection.model);
        if route.is_empty() {
            return Some(key);
        }
        let selection_key = crate::scheduler::canonical_model(&key);
        (registry.client_supports(&c.id, route)
            || (selection_key != route && registry.client_supports(&c.id, selection_key)))
        .then_some(key)
    }
}

impl CredentialStore {
    /// Selects among the selection's providers without registry admission (tests and
    /// callers that already filtered credentials).
    pub fn select(self: &Arc<Self>, selection: Selection) -> Option<Lease> {
        let policy = self.policy.read().unwrap_or_else(PoisonError::into_inner).clone();
        let scope = selection.clone();
        let admit = standalone_admission(&scope, &policy);
        self.select_with(selection, policy.clone(), &admit).ok()
    }

    pub fn select_with(
        self: &Arc<Self>,
        selection: Selection,
        policy: Arc<Policy>,
        admit: &Admit<'_>,
    ) -> Result<Lease, AcquireError> {
        self.select_ranked(selection, policy, admit, &|_| crate::scheduler::Windows::default())
    }

    /// [`Self::select_with`] with each credential's usage windows (`soonest-reset`).
    /// Under the store and scheduler locks each credential costs a few lookups: its
    /// parsed profile, its cooldowns and its windows.
    pub fn select_ranked(
        self: &Arc<Self>,
        selection: Selection,
        policy: Arc<Policy>,
        admit: &Admit<'_>,
        ranks: &crate::scheduler::Ranks<'_>,
    ) -> Result<Lease, AcquireError> {
        let excluded: std::collections::HashSet<&str> = selection.exclude.iter().map(String::as_str).collect();
        let now = Instant::now();
        let (credential, execution_model, lcp, picked) = {
            let inner = self.read();
            let mut scheduler = self.scheduler.lock().unwrap_or_else(PoisonError::into_inner);
            let eligible: Vec<_> = inner
                .creds
                .iter()
                .filter(|c| {
                    !c.disabled
                        && selection.pinned.as_ref().is_none_or(|id| *id == c.id)
                        && selection.accept.is_none_or(|accept| accept(c))
                        && !excluded.contains(c.id.as_str())
                })
                .map(|c| (c, inner.profile(c)))
                .filter(|(_, p)| policy.retry_limit_of(p) >= selection.retry_round && policy.admits_weight(p.weight))
                .filter_map(|(c, p)| admit(c).map(|m| (c, m, p)))
                .collect();
            let candidates: Vec<Candidate<'_>> = eligible
                .iter()
                .filter(|(c, m, _)| scheduler.wait(c, m, now).is_none())
                .map(|(c, m, p)| Candidate {
                    credential: c,
                    model: m,
                    provider: &p.provider,
                    priority: p.priority,
                    weight: p.weight,
                    windows: ranks(c),
                })
                .filter(|c| !c.windows.exhausted)
                .collect();
            let Some((picked, lcp)) = scheduler.pick_candidate(&candidates, &selection, &policy, now) else {
                let cause = eligible
                    .iter()
                    .filter_map(|(c, m, _)| scheduler.last_error(c, m))
                    .max_by_key(|s| s.deadline)
                    .map(|s| s.error.clone());
                let wait = eligible.iter().filter_map(|(c, m, _)| scheduler.wait(c, m, now)).min();
                if !eligible.is_empty() && eligible.iter().all(|(c, m, _)| scheduler.quota_cooling(c, m, now)) {
                    return Err(AcquireError::Cooldown {
                        wait: wait.unwrap_or_default(),
                        cause,
                    });
                }
                return Err(AcquireError::Unavailable {
                    retry_after: if eligible.is_empty() { None } else { wait },
                    cause,
                });
            };
            let generation = scheduler.reserve_probe(picked.credential, picked.model, now);
            (Arc::clone(picked.credential), picked.model.to_owned(), lcp, generation)
        };
        Ok(Lease {
            store: self.clone(),
            credential,
            selection,
            execution_model,
            attempt: self.attempts.fetch_add(1, Ordering::Relaxed),
            policy,
            reported: false,
            picked,
            remote: None,
            lcp,
        })
    }

    /// [`Policy::error_action`] with the credential's rules as parsed when it was stored.
    pub(crate) fn error_action(
        &self,
        policy: &Policy,
        c: &Credential,
        error: &ExecError,
    ) -> crate::scheduler::ErrorAction {
        let inner = self.read();
        policy.error_action_with(inner.profile(c).rules.as_deref(), c, error)
    }

    /// The registry projection of this credential's cooldown state for `model`.
    pub fn suspension(&self, credential: &Credential, model: &str) -> crate::registry::Suspension {
        use crate::registry::Suspension;
        let scheduler = self.scheduler.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        let live = |m: &str| {
            scheduler
                .cooldown(&credential.id, crate::scheduler::canonical_model(m))
                .filter(|s| s.deadline > now)
        };
        if let Some(state) = live(model) {
            return match (state.quota, state.status) {
                (true, 429) => Suspension::Quota,
                (quota_exceeded, _) => Suspension::Other { quota_exceeded },
            };
        }
        if live("").is_some() {
            // Credential-wide quota: Go reports these models as `credential_quota`.
            return Suspension::Other { quota_exceeded: false };
        }
        Suspension::None
    }

    /// Whether `model` is cooling for this credential right now. A probe reservation does
    /// not count: the lease that holds it is the one asking.
    pub fn blocked(&self, credential: &Credential, model: &str) -> bool {
        let scheduler = self.scheduler.lock().unwrap_or_else(PoisonError::into_inner);
        scheduler.cooling(credential, model, Instant::now())
    }

    /// Clears every cooldown and affinity binding of one credential (Go `ResetQuota`).
    /// Returns the model keys that were cooling.
    pub fn reset_cooldown(&self, id: &str) -> Vec<String> {
        let models = self.scheduler.lock().unwrap_or_else(PoisonError::into_inner).reset(id);
        if !models.is_empty() {
            self.persist_cooldowns_of(id);
        }
        models
    }

    /// Returns the next round's wait, if any credential still permits that round
    /// (Go `closestCooldownWaitWithAttempted`). A wait exceeding the cap is rejected,
    /// not shortened. `status` is the round error's status; `attempted` the credentials
    /// that executed this round (they get the 10s quota floor after a 429).
    pub fn retry_wait(
        &self,
        selection: &Selection,
        policy: &Policy,
        status: u16,
        attempted: &[String],
        admit: &Admit<'_>,
    ) -> Option<Duration> {
        self.retry_wait_at(selection, policy, status, attempted, admit, Instant::now())
    }

    pub(super) fn retry_wait_at(
        &self,
        selection: &Selection,
        policy: &Policy,
        status: u16,
        attempted: &[String],
        admit: &Admit<'_>,
        now: Instant,
    ) -> Option<Duration> {
        let inner = self.read();
        let scheduler = self.scheduler.lock().unwrap_or_else(PoisonError::into_inner);
        let wait = inner
            .creds
            .iter()
            .filter(|c| !c.disabled)
            .filter(|c| {
                let profile = inner.profile(c);
                policy.retry_limit_of(&profile) > selection.retry_round && policy.admits_weight(profile.weight)
            })
            .filter_map(|c| admit(c).map(|m| (c, m)))
            .filter(|(c, m)| scheduler.retry_eligible(c, m, now))
            .map(|(c, m)| {
                let wait = scheduler.wait(c, &m, now).unwrap_or_default();
                if status == 429 && attempted.contains(&c.id) && !policy.cooling_disabled(c) {
                    wait.max(Duration::from_secs(10))
                } else {
                    wait
                }
            })
            .min()?;
        if wait > policy.max_retry_interval {
            None
        } else {
            Some(wait)
        }
    }
}
