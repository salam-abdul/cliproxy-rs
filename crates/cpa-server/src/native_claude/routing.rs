//! Routing policy, affinity retention, and cooldowns share the existing flat disk format.
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
};

pub(super) const PIN_CAPACITY: usize = 4096;
const PIN_TTL: u64 = 30 * 24 * 3600 * 1000;

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct RoutingState {
    #[serde(flatten)]
    pub policy: BillingPolicy,
    #[serde(flatten)]
    pub affinity: Affinity,
    #[serde(flatten)]
    pub cooldowns: Cooldowns,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct BillingPolicy {
    #[serde(default)]
    pub allow_extra_usage: bool,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Affinity {
    #[serde(default, rename = "pins")]
    bindings: BTreeMap<String, String>,
    #[serde(default)]
    last_used: BTreeMap<String, u64>,
    #[serde(default)]
    checkpoint_at: u64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Cooldowns {
    #[serde(default, rename = "cooldowns")]
    accounts: BTreeMap<String, u64>,
    #[serde(default, rename = "model_cooldowns")]
    models: BTreeMap<String, BTreeMap<String, u64>>,
}

impl RoutingState {
    pub fn load(path: &Path, at: u64) -> anyhow::Result<Self> {
        let mut state: Self = if path.is_file() {
            serde_json::from_slice(&super::private(path)?)?
        } else {
            Self::default()
        };
        state.prune(at, &HashSet::new());
        if state.affinity.len() > PIN_CAPACITY {
            state.affinity.make_room(&HashSet::new());
        }
        Ok(state)
    }

    pub fn persist(&self, path: &Path) -> anyhow::Result<()> {
        let temp = path.with_extension("json.tmp");
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&temp)?;
        f.write_all(&serde_json::to_vec(self)?)?;
        f.sync_all()?;
        std::fs::rename(temp, path)?;
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    }

    pub fn prune(&mut self, at: u64, active: &HashSet<String>) {
        self.affinity.prune(at, active);
        self.cooldowns.prune(at);
    }
}

impl Affinity {
    pub fn get(&self, session: &str) -> Option<&String> {
        self.bindings.get(session)
    }

    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    pub fn accounts(&self) -> impl Iterator<Item = &String> {
        self.bindings.values()
    }

    pub fn bind(
        &mut self,
        session: String,
        account: String,
        at: u64,
        active: &HashSet<String>,
    ) -> anyhow::Result<bool> {
        if !self.bindings.contains_key(&session) && !self.make_room(active) {
            anyhow::bail!("Native affinity capacity exhausted");
        }
        let checkpoint = self.get(&session) != Some(&account) || at.saturating_sub(self.checkpoint_at) >= 3600000;
        self.bindings.insert(session.clone(), account);
        self.last_used.insert(session, at);
        if checkpoint {
            self.checkpoint_at = at;
        }
        Ok(checkpoint)
    }

    fn prune(&mut self, at: u64, active: &HashSet<String>) {
        for sid in self.bindings.keys() {
            self.last_used.entry(sid.clone()).or_insert(at);
        }
        self.bindings.retain(|sid, _| {
            active.contains(sid) || at.saturating_sub(*self.last_used.get(sid).unwrap_or(&at)) < PIN_TTL
        });
        self.last_used.retain(|sid, _| self.bindings.contains_key(sid));
    }

    fn make_room(&mut self, active: &HashSet<String>) -> bool {
        while self.bindings.len() >= PIN_CAPACITY {
            let oldest = self
                .last_used
                .iter()
                .filter(|(sid, _)| !active.contains(*sid))
                .min_by_key(|(_, at)| *at)
                .map(|(sid, _)| sid.clone());
            let Some(oldest) = oldest else {
                return false;
            };
            self.bindings.remove(&oldest);
            self.last_used.remove(&oldest);
        }
        true
    }
}

impl Cooldowns {
    pub fn account_until(&self, account: &str) -> u64 {
        self.accounts.get(account).copied().unwrap_or(0)
    }

    pub fn model_until(&self, account: &str, model: &str) -> u64 {
        self.models
            .get(account)
            .and_then(|models| models.get(model))
            .copied()
            .unwrap_or(0)
    }

    pub fn until(&self, account: &str, model: &str) -> u64 {
        self.account_until(account).max(self.model_until(account, model))
    }

    fn prune(&mut self, at: u64) {
        self.accounts.retain(|_, until| *until > at);
        self.models.retain(|_, models| {
            models.retain(|_, until| *until > at);
            !models.is_empty()
        });
    }

    pub fn cool(&mut self, account: &str, model: &str, status: u16, delay: Option<u64>, at: u64) {
        let until = at.saturating_add(delay.unwrap_or(if status == 429 { 60000 } else { 15000 }));
        if matches!(status, 503 | 529) {
            let models = self.models.entry(account.into()).or_default();
            models.retain(|_, until| *until > at);
            // ponytail: cap live model cooldowns at 128 per account; use a bounded per-model store if needed.
            if models.len() >= 128 && !models.contains_key(model) {
                let until = models.values().copied().max().unwrap_or(until).max(until);
                let entry = self.accounts.entry(account.into()).or_default();
                *entry = (*entry).max(until);
                return;
            }
            let entry = models.entry(model.into()).or_default();
            *entry = (*entry).max(until);
        } else {
            let entry = self.accounts.entry(account.into()).or_default();
            *entry = (*entry).max(until);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn routing_state_keeps_legacy_disk_format_and_private_atomic_writes() {
        let legacy = json!({
            "allow_extra_usage": true,
            "pins": {"session-a": "account-b"},
            "cooldowns": {"account-b": 2000},
            "model_cooldowns": {"account-c": {"opus": 3000}},
            "last_used": {"session-a": 1000},
            "checkpoint_at": 1000
        });
        let state: RoutingState = serde_json::from_value(legacy.clone()).unwrap();
        assert!(state.policy.allow_extra_usage);
        assert_eq!(state.affinity.get("session-a").map(String::as_str), Some("account-b"));
        assert_eq!(state.cooldowns.until("account-c", "opus"), 3000);
        assert_eq!(serde_json::to_value(&state).unwrap(), legacy);
        let dir = std::env::temp_dir().join(format!("native-routing-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("routing.json");
        state.persist(&path).unwrap();
        let restored = RoutingState::load(&path, 1500).unwrap();
        assert_eq!(serde_json::to_value(restored).unwrap(), legacy);
        assert!(!path.with_extension("json.tmp").exists());
        let legacy_minimal: RoutingState = serde_json::from_value(json!({"pins":{"session":"account"}})).unwrap();
        assert!(!legacy_minimal.policy.allow_extra_usage);
        assert_eq!(legacy_minimal.affinity.len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn binding_checkpoints_changes_and_periodic_activity() {
        let mut affinity = Affinity::default();
        let active = HashSet::new();
        assert!(affinity.bind("s".into(), "a".into(), 1000, &active).unwrap());
        assert!(!affinity.bind("s".into(), "a".into(), 2000, &active).unwrap());
        assert!(affinity.bind("s".into(), "b".into(), 3000, &active).unwrap());
        assert!(affinity.bind("s".into(), "b".into(), 3603000, &active).unwrap());
    }
    #[test]
    fn affinity_capacity_evicts_idle_bindings_and_protects_active_ones() {
        let mut affinity = Affinity::default();
        for i in 0..PIN_CAPACITY {
            affinity
                .bind(i.to_string(), "a".into(), i as u64, &HashSet::new())
                .unwrap();
        }
        let all: HashSet<_> = (0..PIN_CAPACITY).map(|i| i.to_string()).collect();
        assert!(affinity.bind("new".into(), "b".into(), 10000, &all).is_err());
        assert_eq!(affinity.len(), PIN_CAPACITY);
        assert!(affinity.get("new").is_none());
        affinity
            .bind("new".into(), "b".into(), 10000, &HashSet::from(["0".into()]))
            .unwrap();
        assert_eq!(affinity.len(), PIN_CAPACITY);
        assert!(affinity.get("0").is_some());
        assert!(affinity.get("1").is_none());
        assert_eq!(affinity.get("new").map(String::as_str), Some("b"));
    }
    #[test]
    fn overload_only_cools_affected_model_and_never_shortens_window() {
        let mut p = RoutingState::default();
        p.cooldowns.cool("a", "opus", 529, Some(60000), 1000);
        p.cooldowns.cool("a", "opus", 529, Some(1000), 2000);
        assert_eq!(p.cooldowns.account_until("a"), 0);
        assert_eq!(p.cooldowns.until("a", "sonnet"), 0);
        assert_eq!(p.cooldowns.model_until("a", "opus"), 61000);
        p.cooldowns.cool("a", "opus", 429, None, 1000);
        assert_eq!(p.cooldowns.until("a", "sonnet"), 61000);
    }
    #[test]
    fn expired_pins_are_pruned_but_active_sessions_survive() {
        let mut p = RoutingState::default();
        for sid in ["active", "old", "legacy"] {
            p.affinity.bindings.insert(sid.into(), "a".into());
        }
        p.affinity.last_used.insert("old".into(), 0);
        p.affinity.last_used.insert("active".into(), 0);
        p.prune(PIN_TTL + 1, &HashSet::from(["active".into()]));
        assert!(p.affinity.bindings.contains_key("active"));
        assert!(p.affinity.bindings.contains_key("legacy"));
        assert!(!p.affinity.bindings.contains_key("old"));
    }
}
