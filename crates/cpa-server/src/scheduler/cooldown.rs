#[cfg(test)]
use super::policy::file_rules;
use super::*;

pub(crate) struct Cooldown {
    pub deadline: Instant,
    pub level: u32,
    pub status: u16,
    /// Quota-style cooldown (429 or Cloudflare): reported as a model cooldown.
    pub quota: bool,
    /// The failure that set it (Go `ModelState.LastError`), for error summaries.
    pub error: String,
    /// When it was set (Go `ModelState.UpdatedAt`), for `save-cooldown-status`.
    pub since: SystemTime,
    /// A model quota inherited from a credential-wide quota (Go reason
    /// `credential_quota` on sibling model states).
    pub credential: bool,
    /// Bounded trust of a stated quota reset (cliproxy-rs only); `level` stays Go's.
    pub trust: Trust,
}

/// Bounded trust of one cooldown (`max-trusted-cooldown`).
/// Costs nothing until a stated reset is cut: then one comparison per 429 and, per
/// recorded result, a probe check on the two keys the result touches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Trust {
    /// Exponent of the latest bounded window, which lasted `cap << window`. Kept after
    /// the window ends, so the next cut window doubles; a success clears it.
    pub window: Option<u32>,
    /// The scheduler's generation when this bounded window opened or its probe was
    /// reserved ([`Scheduler::reserve_probe`]). A lease picked before it holds an older
    /// answer, which neither ends the window nor frees the reservation. Cost: four
    /// bytes here and on each lease, one comparison per key a result touches.
    pub generation: u32,
    /// The deadline is the bound's: a concurrent 429 inside it keeps it.
    pub bounded: bool,
    /// A probe the upstream accepted (a stream started) answered this ended bounded
    /// window: it no longer reserves probes, but keeps its generation, so an older
    /// lease's late answer still cannot replace it. Fits in existing padding.
    pub accepted: bool,
    /// The upstream's stated reset behind a bounded deadline (Go's recovery time).
    pub stated: Option<Instant>,
    /// An unanswered probe request holds the ended window until this instant.
    pub probe: Option<Instant>,
}

impl Trust {
    /// The window's probe was accepted ([`Scheduler::accept_probe_picked`]).
    fn accept(&mut self) {
        self.bounded = false;
        self.accepted = true;
        self.probe = None;
    }
}

/// Longest a probe reserves an ended bounded window. A 429 arrives within seconds; a
/// probe still unanswered after this is being served, and its result clears it anyway.
pub(super) const PROBE_HOLD: Duration = Duration::from_secs(30);
/// Largest trust exponent: `cap << 20` is over a century for any cap of 1 h or more.
pub(super) const MAX_TRUST_WINDOW: u32 = 20;

impl Cooldown {
    /// When this entry stops blocking picks: its deadline, or an unanswered probe.
    fn until(&self, now: Instant) -> Option<Instant> {
        let live = (self.deadline > now).then_some(self.deadline);
        live.max(self.trust.probe.filter(|p| *p > now))
    }

    /// A bounded window (still armed, or answered by an accepted probe) that opened, or
    /// had its probe reserved, after the lease picked at generation `picked`: that
    /// lease's answer does not answer it.
    fn newer_than(&self, picked: u32) -> bool {
        // Serial-number order, so the wrapping counter compares right while a lease is
        // fewer than 2^31 generations old.
        (self.trust.bounded || self.trust.accepted) && (self.trust.generation.wrapping_sub(picked) as i32) > 0
    }
}

/// One credential's cooldowns: the credential-wide one and one per model.
#[derive(Default)]
pub(crate) struct Cooldowns {
    own: Option<Cooldown>,
    models: HashMap<String, Cooldown>,
}

impl Cooldowns {
    /// The cooldown of `model`; the empty model is the credential-wide one.
    fn get(&self, model: &str) -> Option<&Cooldown> {
        if model.is_empty() {
            self.own.as_ref()
        } else {
            self.models.get(model)
        }
    }

    fn get_mut(&mut self, model: &str) -> Option<&mut Cooldown> {
        if model.is_empty() {
            self.own.as_mut()
        } else {
            self.models.get_mut(model)
        }
    }

    fn insert(&mut self, model: &str, state: Cooldown) {
        match self.get_mut(model) {
            Some(slot) => *slot = state,
            None if model.is_empty() => self.own = Some(state),
            None => {
                self.models.insert(model.to_owned(), state);
            }
        }
    }

    fn remove(&mut self, model: &str) {
        if model.is_empty() {
            self.own = None;
        } else {
            self.models.remove(model);
        }
    }

    fn is_empty(&self) -> bool {
        self.own.is_none() && self.models.is_empty()
    }

    /// Every cooldown by model, the credential-wide one under the empty model.
    fn iter(&self) -> impl Iterator<Item = (&str, &Cooldown)> {
        self.own
            .iter()
            .map(|s| ("", s))
            .chain(self.models.iter().map(|(m, s)| (m.as_str(), s)))
    }
}

/// Go `nextQuotaCooldown`: 1s doubling to 30m.
pub(super) fn quota_backoff(level: u32) -> (Duration, u32) {
    let seconds = (1u64 << level.min(11)).min(1800);
    if seconds >= 1800 {
        (Duration::from_secs(1800), level)
    } else {
        (Duration::from_secs(seconds), level + 1)
    }
}

/// Bounded trust for a stated quota reset: a reset longer
/// than the window's bound is cut to it, so the next ordinary request after the bound
/// probes the account and an early provider reset is noticed within the bound. The
/// first window is `cap`; each cut window after one that ended without a success is
/// twice the last (`window` is the last exponent). Returns the bound and its exponent,
/// or `None` when the stated reset is trusted as it is (it fits, or `cap` is zero).
pub(super) fn bound(stated: Duration, cap: Duration, window: Option<u32>) -> Option<(Duration, u32)> {
    if cap.is_zero() {
        return None;
    }
    let step = window.map_or(0, |w| w.saturating_add(1)).min(MAX_TRUST_WINDOW);
    let bound = cap.saturating_mul(1 << step);
    (stated > bound).then_some((bound, step))
}

/// Go `recoverableFailureRetryAfterWithHint`.
pub(super) fn transient(policy: &Policy, hint: Option<Duration>, disabled: bool) -> Option<Duration> {
    if disabled || policy.transient_error_cooldown_seconds < 0 {
        return None;
    }
    if let Some(hint) = hint.filter(|d| !d.is_zero()) {
        return Some(hint);
    }
    Some(Duration::from_secs(match policy.transient_error_cooldown_seconds {
        0 => 60,
        s => s as u64,
    }))
}

impl Scheduler {
    /// The cooldown of `id` for the canonical `model`; the empty model is the
    /// credential-wide one.
    pub(crate) fn cooldown(&self, id: &str, model: &str) -> Option<&Cooldown> {
        self.cooldowns.get(id)?.get(model)
    }

    pub(crate) fn cooldown_mut(&mut self, id: &str, model: &str) -> Option<&mut Cooldown> {
        self.cooldowns.get_mut(id)?.get_mut(model)
    }

    fn set_cooldown(&mut self, id: &str, model: &str, state: Cooldown) {
        match self.cooldowns.get_mut(id) {
            Some(entry) => entry.insert(model, state),
            None => {
                let mut entry = Cooldowns::default();
                entry.insert(model, state);
                self.cooldowns.insert(id.to_owned(), entry);
            }
        }
    }

    fn remove_cooldown(&mut self, id: &str, model: &str) {
        if let Some(entry) = self.cooldowns.get_mut(id) {
            entry.remove(model);
            if entry.is_empty() {
                self.cooldowns.remove(id);
            }
        }
    }

    /// The cooldowns of `model` and of the whole credential.
    fn both(&self, c: &Credential, model: &str) -> [Option<&Cooldown>; 2] {
        let model = canonical_model(model);
        match self.cooldowns.get(c.id.as_str()) {
            Some(entry) => [entry.get(model), entry.own.as_ref()],
            None => [None, None],
        }
    }

    pub fn quota_cooling(&self, c: &Credential, model: &str, now: Instant) -> bool {
        self.both(c, model)
            .into_iter()
            .flatten()
            .any(|s| s.quota && s.until(now).is_some())
    }

    pub fn wait(&self, c: &Credential, model: &str, now: Instant) -> Option<Duration> {
        self.both(c, model)
            .into_iter()
            .flatten()
            .filter_map(|s| s.until(now)?.checked_duration_since(now).filter(|d| !d.is_zero()))
            .max()
    }

    /// Whether a cooldown deadline (not a probe reservation) is ahead for `model`.
    pub fn cooling(&self, c: &Credential, model: &str, now: Instant) -> bool {
        self.both(c, model).into_iter().flatten().any(|s| s.deadline > now)
    }

    /// The last failure recorded for this credential and model, if any.
    pub fn last_error(&self, c: &Credential, model: &str) -> Option<&Cooldown> {
        let [model, own] = self.both(c, model);
        model.or(own)
    }

    pub fn retry_eligible(&self, c: &Credential, model: &str, now: Instant) -> bool {
        // A bounded window or an unanswered probe is not waited for: the probe answers
        // for everyone, and holding requests to the window's end would release them
        // together as a burst of probes.
        self.both(c, model).into_iter().flatten().all(|s| {
            let probing = s.trust.probe.is_some_and(|p| p > now);
            !probing && (s.deadline <= now || (retry_status(s.status) && !s.trust.bounded))
        })
    }

    /// [`Self::accept_probe_picked`] for a lease picked just now.
    #[cfg(test)]
    pub fn accept_probe(&mut self, c: &Credential, model: &str, now: Instant) {
        self.accept_probe_picked(c, model, self.generation, now);
    }

    /// The upstream accepted the attempt that probes `model` (a stream's first chunk),
    /// picked at generation `picked`: the ended bounded window has its answer, so other
    /// picks may go through while the stream runs. Only expired bounded entries that
    /// are not newer than the lease change; their count stays until the stream's own
    /// outcome is recorded. When the credential-wide window ended, the model keys that
    /// share its deadline (its copies) are disarmed too; a model's own window is not.
    /// Cost: two lookups, and a scan of the credential's model cooldowns only when its
    /// credential-wide window ended.
    pub fn accept_probe_picked(&mut self, c: &Credential, model: &str, picked: u32, now: Instant) {
        let model = canonical_model(model);
        let Some(entry) = self.cooldowns.get_mut(c.id.as_str()) else {
            return;
        };
        let mut credential_window = None;
        for m in [model, ""] {
            if let Some(s) = entry.get_mut(m)
                && s.trust.bounded
                && s.deadline <= now
                && !s.newer_than(picked)
            {
                s.trust.accept();
                if m.is_empty() {
                    credential_window = Some(s.deadline);
                }
            }
        }
        if let Some(ended) = credential_window {
            for s in entry.models.values_mut() {
                if s.trust.bounded && s.deadline == ended && !s.newer_than(picked) {
                    s.trust.accept();
                }
            }
        }
    }

    /// The generation of a lease picked outside selection (the Antigravity credits
    /// fallback, which runs on credentials selection refused): one that every bounded
    /// window and probe reservation within 2^30 generations of now, before or after, is
    /// newer than. Such a lease answers none of them: a cancelled or successful credits
    /// attempt neither frees another request's probe nor ends a window, which stays with
    /// its own probe.
    pub fn outside_selection_generation(&self) -> u32 {
        self.generation.wrapping_sub(1 << 30)
    }

    /// Reserves the probe of `c` for `model` when its pick ends a bounded window: until
    /// the probe's result is recorded (or [`PROBE_HOLD`] passes), the window's keys keep
    /// cooling, so concurrent requests do not each go upstream. Returns the generation
    /// the pick's lease carries; a reservation takes a new one, so only its own lease's
    /// result frees it. Cost per pick: two lookups.
    pub fn reserve_probe(&mut self, c: &Credential, model: &str, now: Instant) -> u32 {
        let model = canonical_model(model);
        let next = self.generation.wrapping_add(1);
        for m in [model, ""] {
            if let Some(s) = self.cooldown_mut(&c.id, m)
                && s.trust.bounded
                && s.deadline <= now
            {
                s.trust.probe = now.checked_add(PROBE_HOLD);
                s.trust.generation = next;
                self.generation = next;
            }
        }
        self.generation
    }
}

impl Scheduler {
    /// [`Self::record_picked`] for an attempt picked just now.
    #[cfg(test)]
    pub fn record(&mut self, c: &Credential, model: &str, outcome: &Outcome, policy: &Policy, now: Instant) {
        self.record_picked(c, model, outcome, policy, self.generation, now);
    }

    /// [`Self::record_with`], parsing the credential's own error rules.
    #[cfg(test)]
    pub fn record_picked(
        &mut self,
        c: &Credential,
        model: &str,
        outcome: &Outcome,
        policy: &Policy,
        picked: u32,
        now: Instant,
    ) {
        self.record_with(c, file_rules(c).as_deref(), model, outcome, policy, picked, now);
    }

    /// Applies one attempt's outcome (Go `MarkResult`), with the credential's own error
    /// rules already parsed. `picked` is the generation its lease was picked at
    /// ([`Self::reserve_probe`]): a bounded window or probe reservation newer than that
    /// is not answered by it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_with(
        &mut self,
        c: &Credential,
        rules: Option<&[ErrorRule]>,
        model: &str,
        outcome: &Outcome,
        policy: &Policy,
        picked: u32,
        now: Instant,
    ) {
        let model = canonical_model(model);
        let id = c.id.as_str();
        // Any answer ends a probe reservation on the keys it touches, except one taken
        // after its lease was picked (a later probe's, once this one's hold lapsed).
        for m in [model, ""] {
            if let Some(s) = self.cooldown_mut(id, m)
                && !s.newer_than(picked)
            {
                s.trust.probe = None;
            }
        }
        match outcome {
            Outcome::Success => self.record_success(id, model, picked, now),
            Outcome::Failure(error) => self.record_failure(c, rules, model, error, policy, picked, now),
            Outcome::Cancelled | Outcome::Neutral(_) => {}
        }
    }

    /// A success clears the model's cooldown, and an ended credential-wide one it
    /// answers.
    fn record_success(&mut self, id: &str, model: &str, picked: u32, now: Instant) {
        let Some(entry) = self.cooldowns.get_mut(id) else {
            return;
        };
        // Active credential quota survives success on an in-flight sibling.
        if entry.own.as_ref().is_some_and(|s| s.quota && s.deadline > now) {
            return;
        }
        // A request that started before a bounded window opened leaves it for the
        // window's own probe.
        if let Some(ended) = entry
            .own
            .as_ref()
            .filter(|s| s.deadline <= now && !s.newer_than(picked))
            .map(|s| s.deadline)
        {
            entry.own = None;
            // The credential recovered: model keys that a credential-wide window
            // escalated forget it too (their Go state is left as Go keeps it). A
            // model's own ended bounded window still waits for its probe, and one
            // a newer probe reserved or answered keeps its state.
            for s in entry.models.values_mut() {
                if s.deadline <= now && (!s.trust.bounded || s.deadline == ended) && !s.newer_than(picked) {
                    s.trust = Trust::default();
                }
            }
        }
        if !entry.get(model).is_some_and(|s| s.newer_than(picked)) {
            entry.remove(model);
        }
        if entry.is_empty() {
            self.cooldowns.remove(id);
        }
    }

    /// A failure's cooldown, by cause.
    #[allow(clippy::too_many_arguments)]
    fn record_failure(
        &mut self,
        c: &Credential,
        rules: Option<&[ErrorRule]>,
        model: &str,
        error: &ExecError,
        policy: &Policy,
        picked: u32,
        now: Instant,
    ) {
        let id = c.id.as_str();
        let action = policy.error_action_with(rules, c, error);
        if !action.cooldown {
            return;
        }
        let credential_quota = crate::classify::credential_scoped(error);
        let key = if credential_quota { "" } else { model };
        // A forced cooldown applies the normal policy even when cooling is disabled.
        let disabled = policy.cooling_disabled(c) && !action.force_cooldown;
        let status = error.go_status();
        let text = crate::classify::error_text(error);
        let prev = self.cooldowns.get(id).and_then(|e| e.get(key));
        // An ended bounded window that a newer probe took over, or that opened after this
        // attempt was picked, waits for its own probe: this older answer neither renews
        // nor replaces it. Inside a live window the handling below applies.
        if prev.is_some_and(|s| s.deadline <= now && s.newer_than(picked)) {
            return;
        }
        let prev_deadline = prev.map(|s| s.deadline);
        let prev_live = prev.filter(|s| s.deadline > now).map(|s| s.deadline);
        let prev_trust = prev.map(|s| s.trust).unwrap_or_default();
        let mut level = prev.filter(|s| s.quota).map(|s| s.level).unwrap_or(0);
        // Causes other than a cut reset keep the trust count: nothing recovered.
        let mut trust = Trust {
            window: prev_trust.window,
            ..Trust::default()
        };
        let hint = error.retry_after.filter(|d| !d.is_zero());
        let mut quota = false;
        let duration = if crate::classify::is_model_support(status, &text) {
            (!disabled).then(|| hint.unwrap_or(Duration::from_secs(43200)))
        } else if crate::classify::is_cloudflare(status, &text) {
            quota = true;
            (!disabled).then(|| {
                let (d, next) = quota_backoff(level);
                level = next;
                d.max(Duration::from_secs(10))
            })
        } else if crate::classify::is_invalid_grant(status, &text) {
            (!disabled).then_some(Duration::from_secs(1800))
        } else {
            match status {
                401..=403 => (!disabled).then_some(Duration::from_secs(1800)),
                404 => (!disabled).then(|| hint.unwrap_or(Duration::from_secs(43200))),
                429 => {
                    quota = true;
                    if credential_quota && prev.is_none_or(|s| !s.quota) {
                        level = 0;
                    }
                    if disabled {
                        None
                    } else if let Some(hint) = error.retry_after {
                        // A present hint, even zero, gets the 10s floor (Go
                        // minQuotaCooldownFloor); only an absent one backs off.
                        let stated = hint.max(Duration::from_secs(10));
                        let cap = policy.max_trusted_cooldown;
                        if !cap.is_zero()
                            && let Some(window) = prev.filter(|s| s.trust.bounded && s.deadline > now)
                        {
                            let (old, trust) = (window.deadline, window.trust);
                            let reset = now.checked_add(stated);
                            self.quota_in_bounded_window(c, key, old, trust, reset, text, credential_quota, now);
                            return;
                        }
                        Some(match bound(stated, cap, prev_trust.window) {
                            Some((bound, window)) => {
                                self.generation = self.generation.wrapping_add(1);
                                trust = Trust {
                                    window: Some(window),
                                    generation: self.generation,
                                    bounded: true,
                                    accepted: false,
                                    stated: now.checked_add(stated),
                                    probe: None,
                                };
                                bound
                            }
                            None => stated,
                        })
                    } else if let Some(prev) = prev.filter(|s| s.quota && s.deadline > now) {
                        // Go `quotaCooldownAfterFailure`: an active quota deadline is
                        // reused, not extended.
                        let deadline = prev.deadline;
                        if let Some(slot) = self.cooldowns.get_mut(id).and_then(|e| e.get_mut(key)) {
                            slot.error = text;
                        }
                        if credential_quota {
                            self.extend_siblings(c, deadline, None, now);
                        }
                        return;
                    } else {
                        let (d, next) = quota_backoff(level);
                        level = next;
                        Some(d)
                    }
                }
                408 | 500 | 502..=504 | 520..=526 => transient(policy, hint, disabled),
                _ => transient(policy, None, disabled),
            }
        };
        // Go falls back to the one-minute transient cooldown when a forced cooldown's
        // policy produced none (for example transient cooldowns disabled).
        let duration = match duration {
            None if action.force_cooldown => Some(Duration::from_secs(60)),
            d => d,
        };
        let Some(next) = duration.and_then(|d| now.checked_add(d)) else {
            self.remove_cooldown(id, key);
            return;
        };
        let deadline = prev_live.map_or(next, |p| p.max(next));
        // An answer from a request already in flight (a 503, a 401) inside a live bounded
        // quota window that outlasts it: the window stands as it is, quota and all, so
        // its saved record keeps the stated reset and count. Only bounded windows; Go's
        // own cooldowns merge as Go does.
        if prev_live == Some(deadline)
            && let Some(slot) = self
                .cooldowns
                .get_mut(id)
                .and_then(|e| e.get_mut(key))
                .filter(|s| s.quota && s.trust.bounded)
        {
            slot.error = text;
            // Go's level still counts a Cloudflare challenge (unchanged for other causes).
            slot.level = level;
            return;
        }
        // Provenance follows the deadline: a longer earlier cooldown that stands keeps
        // its own bounded flag and stated reset.
        let provenance = |deadline: Instant| {
            if deadline == next {
                trust
            } else if prev_live == Some(deadline) {
                Trust {
                    probe: None,
                    ..prev_trust
                }
            } else {
                Trust {
                    window: trust.window,
                    ..Trust::default()
                }
            }
        };
        let state = |deadline| Cooldown {
            deadline,
            level,
            status: error.status,
            quota,
            error: text.clone(),
            since: SystemTime::now(),
            credential: false,
            trust: provenance(deadline),
        };
        self.set_cooldown(id, key, state(deadline));
        if credential_quota {
            self.extend_siblings(c, deadline, prev_deadline, now);
            // Go also records the failing model's own quota state (reason `quota`).
            if !model.is_empty() {
                // `extend_siblings` above already took a live own key to the credential's
                // deadline, unless it is longer, the model's own bounded window, or the
                // credential's window is bounded: then the own deadline stands.
                let own = self.cooldown(id, model);
                let own_deadline = own.filter(|s| s.deadline > now).map_or(deadline, |s| s.deadline);
                let own_trust = own.map(|s| s.trust).unwrap_or_default();
                let own_window = own_trust.window;
                let mut own_state = state(own_deadline);
                if own_deadline != deadline {
                    // The model's own window (longer, or bounded) wins: its provenance
                    // goes with it.
                    own_state.trust = Trust {
                        probe: None,
                        ..own_trust
                    };
                } else {
                    own_state.trust = provenance(deadline);
                }
                own_state.trust.window = own_state.trust.window.max(own_window);
                self.set_cooldown(id, model, own_state);
            }
        }
    }

    /// A concurrent 429 inside a live bounded window ending at `old`: the probe deadline
    /// and the count stand, but an earlier stated reset moves the probe up to it.
    #[allow(clippy::too_many_arguments)]
    fn quota_in_bounded_window(
        &mut self,
        c: &Credential,
        key: &str,
        old: Instant,
        mut window_trust: Trust,
        reset: Option<Instant>,
        text: String,
        credential_quota: bool,
        now: Instant,
    ) {
        let id = c.id.as_str();
        let deadline = reset.map_or(old, |r| old.min(r));
        if let Some(slot) = self.cooldown_mut(id, key) {
            slot.deadline = deadline;
            slot.error = text;
            slot.trust.stated = reset.or(slot.trust.stated);
            window_trust = slot.trust;
        }
        if credential_quota {
            // Model keys that inherited this credential window (same deadline, bounded)
            // follow it when it moves up; longer independent cooldowns stand.
            for state in self
                .cooldowns
                .get_mut(id)
                .into_iter()
                .flat_map(|e| e.models.values_mut())
            {
                if state.trust.bounded && state.deadline == old {
                    state.deadline = deadline;
                    state.trust.stated = window_trust.stated;
                    state.trust.window = state.trust.window.max(window_trust.window);
                }
            }
            self.extend_siblings(c, deadline, None, now);
        }
    }

    /// Go's credential-scoped 429: live sibling model states become quota cooldowns
    /// (`credential_quota`) lasting at least as long as the credential. Under a bounded
    /// credential window, and for a model's own bounded window under any, a model's own
    /// deadline is left as it is: the credential's key holds the model until the
    /// credential's probe anyway, and a copied deadline would follow the credential's
    /// when an earlier reset moves it up, before the model's own deadline (Go never
    /// shortens one). Only copies take the credential's deadline: the copies of the
    /// window this one `renews` (the credential's previous deadline) move onto it even
    /// when they ended, so no copy outlives the account's recovery.
    fn extend_siblings(&mut self, c: &Credential, deadline: Instant, renews: Option<Instant>, now: Instant) {
        let Some(entry) = self.cooldowns.get_mut(c.id.as_str()) else {
            return;
        };
        let (level, trust) = entry.own.as_ref().map_or((0, Trust::default()), |s| (s.level, s.trust));
        for state in entry.models.values_mut() {
            let copy = state.trust.bounded && (state.deadline == deadline || Some(state.deadline) == renews);
            if state.deadline > now || copy {
                let window = state.trust.window.max(trust.window);
                if copy || !(state.trust.bounded || trust.bounded) {
                    if deadline >= state.deadline {
                        state.trust = Trust { probe: None, ..trust };
                    }
                    state.deadline = state.deadline.max(deadline);
                }
                // A model further along its own escalation keeps its count.
                state.trust.window = window;
                state.quota = true;
                state.credential = true;
                state.level = level;
            }
        }
    }

    /// Clears every cooldown of one credential (Go `ResetQuota`), returning the models
    /// that were cooling.
    pub fn reset(&mut self, id: &str) -> Vec<String> {
        let mut models: Vec<String> = self
            .cooldowns
            .remove(id)
            .iter()
            .flat_map(|e| e.iter().map(|(m, _)| m.to_owned()))
            .collect();
        self.affinity.invalidate(id);
        self.lcp.invalidate(id);
        self.probes.remove(id);
        models.sort();
        models
    }

    /// Go `cooldownStateRecordsForAuthLocked`: one record per live cooldown of `c`, by
    /// model (the credential-wide quota is the model-less record).
    pub fn records(&self, c: &Credential, now: Instant, wall: SystemTime) -> Vec<crate::cooldown_store::Record> {
        use crate::cooldown_store::{LastError, Quota, Record};
        let mut out: Vec<Record> = self
            .cooldowns
            .get(c.id.as_str())
            .into_iter()
            .flat_map(Cooldowns::iter)
            .filter(|(_, state)| state.deadline > now)
            .map(|(model, state)| {
                let at = wall + (state.deadline - now);
                // Go's recovery time is the stated reset; the probe time is the retry.
                let recover = state
                    .trust
                    .stated
                    .filter(|r| state.trust.bounded && *r > state.deadline)
                    .map_or(at, |r| wall + (r - now));
                let reason = match (model.is_empty() || state.credential, state.quota, state.status) {
                    (true, true, _) => "credential_quota".to_owned(),
                    (false, true, 429) => "quota".to_owned(),
                    (false, true, _) => "cloudflare challenge".to_owned(),
                    (_, false, _) => state.error.clone(),
                };
                Record {
                    provider: c.provider.trim().to_owned(),
                    auth_id: c.id.clone(),
                    model: model.to_owned(),
                    status: "cooling".into(),
                    next_retry_after: Some(at),
                    quota: if state.quota {
                        Quota {
                            exceeded: true,
                            reason: reason.clone(),
                            next_recover_at: Some(recover),
                            backoff_level: state.level,
                            observed_at: None,
                            trust_windows: Some(state.trust.window.map_or(0, |w| w.saturating_add(1))),
                        }
                    } else {
                        Quota::default()
                    },
                    reason,
                    last_error: Some(LastError {
                        message: state.error.clone(),
                        http_status: u32::from(state.status),
                        ..LastError::default()
                    }),
                    updated_at: Some(state.since),
                    auth_file: match &c.source {
                        cpa_core::credential::Source::File(path) => Some(path.clone()),
                        _ => None,
                    },
                }
            })
            .collect();
        out.sort_by(|a, b| a.model.cmp(&b.model));
        out
    }

    /// Go `restoreCooldownRecordLocked` for one live record of `c`. A model-less record
    /// restores the credential-wide quota; other model-less records aggregate the model
    /// records (Go recomputes them) and apply only when `c` has no model records.
    pub fn restore(
        &mut self,
        c: &Credential,
        record: &crate::cooldown_store::Record,
        has_model_records: bool,
        cap: Duration,
        now: Instant,
        wall: SystemTime,
    ) -> bool {
        // Go blocks until the later of the retry deadline and, for a quota state, the
        // quota recovery time (`availabilityBlock`); one deadline here.
        // Go restores only records whose retry deadline is still ahead.
        let Some(retry) = record.next_retry_after.filter(|at| *at > wall) else {
            return false;
        };
        let recover = record.quota.next_recover_at.filter(|_| record.quota.exceeded);
        let saved = record.quota.trust_windows.map(|n| n.min(MAX_TRUST_WINDOW + 1));
        let model = record.model.trim();
        if model.is_empty() && has_model_records && record.quota.reason != "credential_quota" {
            return false;
        }
        let since = |at: SystemTime| at.duration_since(wall).unwrap_or_default();
        let (remaining, trust) = match (record.quota.exceeded && !cap.is_zero(), saved) {
            // Written by cliproxy-rs: the saved retry is the deadline, as it is, whatever
            // set it (a bounded window, a trusted reset, or a longer cooldown from another
            // cause that a credential-wide quota marked), so a restart never cuts or
            // escalates it. A stated reset after the retry marks a bounded window.
            (true, Some(spent)) => {
                let stated = recover.filter(|r| *r > retry).map(|r| now + since(r));
                let trust = Trust {
                    window: spent.checked_sub(1),
                    generation: 0,
                    bounded: stated.is_some(),
                    accepted: false,
                    stated,
                    probe: None,
                };
                (since(retry), trust)
            }
            // A Go record, or one from before the bound: Go's deadline gets the first
            // bound. Go's `backoff_level` keeps its meaning and does not raise it.
            (true, None) => {
                let at = recover.map_or(retry, |r| retry.max(r));
                match bound(since(at), cap, None) {
                    Some((bound, window)) => (
                        bound,
                        Trust {
                            window: Some(window),
                            generation: 0,
                            bounded: true,
                            accepted: false,
                            stated: Some(now + since(at)),
                            probe: None,
                        },
                    ),
                    None => (since(at), Trust::default()),
                }
            }
            (false, _) => (since(recover.map_or(retry, |r| retry.max(r))), Trust::default()),
        };
        if remaining.is_zero() {
            return false;
        }
        let deadline = now + remaining;
        let error = record.last_error.clone().unwrap_or_default();
        let status = match error.http_status {
            0 if record.quota.exceeded => 429,
            s => u16::try_from(s).unwrap_or(0),
        };
        let key = canonical_model(model);
        let prev = self.cooldown(&c.id, key).filter(|prev| prev.deadline > now);
        let prev_wins = prev.is_some_and(|p| p.deadline > deadline);
        let deadline = prev.map_or(deadline, |prev| prev.deadline.max(deadline));
        let mut quota = (
            record.quota.exceeded,
            status,
            !model.is_empty() && record.quota.reason == "credential_quota",
        );
        let mut trust = trust;
        // With bounded trust on either side, provenance and the quota classification come
        // from the entry that supplies the deadline, and the counts merge. Without it the
        // merge is Go's.
        if let Some(prev) = prev.filter(|p| p.trust != Trust::default() || trust != Trust::default()) {
            let window = prev.trust.window.max(trust.window);
            if prev_wins {
                trust = Trust {
                    probe: None,
                    ..prev.trust
                };
                quota = (prev.quota, prev.status, prev.credential);
            }
            trust.window = window;
        }
        // A bounded window this restore installs is newer than every lease in flight (a
        // restore also runs while serving, when `save-cooldown-status` turns on or its
        // directory moves); a live window it keeps keeps its generation.
        if trust.bounded && !prev_wins {
            self.generation = self.generation.wrapping_add(1);
            trust.generation = self.generation;
        }
        let (quota, status, credential) = quota;
        self.set_cooldown(
            &c.id,
            key,
            Cooldown {
                deadline,
                level: record.quota.backoff_level,
                status,
                quota,
                error: if error.message.is_empty() {
                    record.reason.clone()
                } else {
                    error.message
                },
                since: record.updated_at.unwrap_or(wall),
                credential,
                trust,
            },
        );
        true
    }

    /// Go `clearDisabledCooldownStates`: drops cooldowns of credentials that are disabled
    /// or whose cooling the policy disables. Returns whether anything was cleared.
    pub fn clear_disabled(&mut self, credentials: &[std::sync::Arc<Credential>], policy: &Policy) -> bool {
        let before = self.cooldowns.len();
        self.cooldowns.retain(|id, _| {
            credentials
                .iter()
                .find(|c| c.id == *id)
                .is_none_or(|c| !c.disabled && !policy.cooling_disabled(c))
        });
        self.cooldowns.len() != before
    }
}

/// Time left until a bounded cooldown's stated reset, while it is ahead.
pub(super) fn recover_after(s: &Cooldown, now: Instant) -> Option<Duration> {
    let stated = s
        .trust
        .stated
        .filter(|r| s.trust.bounded && *r > now && *r > s.deadline)?;
    Some(stated - now)
}

/// One active cooldown for management views (additive read API). `model` is empty for
/// a credential-wide cooldown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CooldownState {
    pub model: String,
    /// Until the next attempt (zero once a bounded window ended and the next request
    /// probes the account).
    pub remaining: Duration,
    /// Until the upstream's stated reset, when the bound cut it (cliproxy-rs only).
    pub recover_in: Option<Duration>,
    pub level: u32,
    pub status: u16,
    pub quota: bool,
}

impl Scheduler {
    /// Unexpired cooldowns of one credential, model keys sorted, credential-wide first.
    pub(crate) fn cooldowns_of(&self, id: &str, now: Instant) -> Vec<CooldownState> {
        let mut out: Vec<CooldownState> = self
            .cooldowns
            .get(id)
            .into_iter()
            .flat_map(Cooldowns::iter)
            .filter(|(_, s)| s.deadline > now || recover_after(s, now).is_some())
            .map(|(model, s)| CooldownState {
                model: model.to_owned(),
                remaining: s.deadline.saturating_duration_since(now),
                recover_in: recover_after(s, now),
                level: s.level,
                status: s.status,
                quota: s.quota,
            })
            .collect();
        out.sort_by(|a, b| a.model.cmp(&b.model));
        out
    }

    /// Clears every cooldown of one credential; returns the model keys that had one.
    pub(crate) fn reset_cooldowns(&mut self, id: &str) -> Vec<String> {
        let mut models: Vec<String> = self
            .cooldowns
            .remove(id)
            .map(|e| e.models.into_keys().collect())
            .unwrap_or_default();
        models.sort();
        // A reset credential is probed again under `soonest-reset`; affinity is kept.
        self.probes.remove(id);
        models
    }
}
