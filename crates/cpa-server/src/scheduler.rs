//! Credential routing policy. Config parsing remains owned by cpa-core; the runtime
//! publishes this policy with its config through Runtime::publish_config.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

use cpa_core::credential::Credential;
use cpa_core::exec::ExecError;
#[cfg(test)]
use cpa_core::exec::FailureScope;
use serde::Deserialize;
use serde_json::Value;

use crate::runtime::{Outcome, Selection};

mod cooldown;
mod pick;
mod policy;
mod windows;

pub use cooldown::CooldownState;
use cooldown::Cooldowns;
pub(crate) use pick::Candidate;
use pick::Rotation;
pub(crate) use policy::Profile;
pub use policy::{ErrorAction, ErrorRule, Policy, RuleAction, Strategy, retry_status};
use windows::Probes;
pub use windows::{Observation, Ranks, Rollover, WindowId, Windows};

pub use cpa_core::registry::dynamic::canonical_model;

#[derive(Default)]
pub(crate) struct Scheduler {
    rotations: HashMap<(String, String), Rotation>,
    /// Mixed-provider round-robin cursors (Go `mixedCursors`).
    cursors: HashMap<(String, String), usize>,
    /// Cooldowns by credential ID; no entry is empty.
    cooldowns: HashMap<String, Cooldowns>,
    /// Session affinity bindings (Go `SessionAffinitySelector.cache`).
    affinity: crate::affinity::Cache,
    /// Bindings of requests without an explicit session (Go
    /// `SessionAffinitySelector.matcher`).
    lcp: crate::lcp::Matcher,
    /// `soonest-reset` probes by credential, reserved when the probe request is picked
    /// (under the scheduler lock, so concurrent picks never probe twice). See [`probed`].
    probes: HashMap<String, Probes>,
    /// The live policy turned session affinity off: results of leases picked while it
    /// was on no longer bind anything (Go's replacement selector has no affinity).
    affinity_off: bool,
    /// Bounded windows opened plus probes reserved; each lease carries the value at its
    /// pick ([`Trust::generation`]).
    generation: u32,
}

impl Scheduler {
    pub fn configure(&mut self, previous: &Policy, next: &Policy) {
        self.affinity_off = !next.session_affinity;
        if previous.strategy != next.strategy
            || previous.session_affinity != next.session_affinity
            || previous.session_affinity_ttl != next.session_affinity_ttl
            || previous.session_affinity_subagents != next.session_affinity_subagents
        {
            self.rotations.clear();
            self.cursors.clear();
            self.affinity.clear();
            // Go builds a new selector, and with it a new matcher on the new TTL. Unlike
            // Go, access generations keep counting.
            self.lcp.reset(crate::lcp::Limits {
                ttl: next.session_affinity_ttl,
                ..Default::default()
            });
            self.probes.clear();
        }
    }

    /// Session affinity under `policy`, unless the live policy has since turned it off.
    fn affinity(&self, policy: &Policy) -> bool {
        policy.session_affinity && !self.affinity_off
    }

    pub fn reconcile(&mut self, credentials: &[std::sync::Arc<Credential>]) {
        let live: std::collections::HashSet<&str> = credentials.iter().map(|c| c.id.as_str()).collect();
        self.cooldowns.retain(|id, _| live.contains(id.as_str()));
        self.affinity.retain(|id| live.contains(id));
        self.lcp.retain(|id| live.contains(id));
        self.probes.retain(|id, _| live.contains(id.as_str()));
    }
}

#[cfg(test)]
#[path = "scheduler_tests.rs"]
mod tests;
