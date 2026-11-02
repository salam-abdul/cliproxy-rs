//! Credential identity fencing and serialized publication. Disk I/O never holds `inner`.
use super::*;

// Go RegistrationEpoch/CredentialVersion: metadata edits keep both; token changes
// advance credentials, even A→B→A; deletion/recreation changes registration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Version {
    registration: u64,
    credentials: u64,
}

impl Inner {
    pub(super) fn register(&mut self, previous: Option<&Credential>, next: &Credential) {
        let fresh = Version {
            registration: next.revision,
            credentials: next.revision,
        };
        let version = match (previous, self.versions.get(&next.id)) {
            (Some(old), Some(version)) if old.provider == next.provider && old.source == next.source => Version {
                credentials: if credentials_changed(old, next) {
                    next.revision
                } else {
                    version.credentials
                },
                ..*version
            },
            _ => fresh,
        };
        self.versions.insert(next.id.clone(), version);
        self.profiles
            .insert(next.id.clone(), crate::scheduler::Profile::of(next));
    }
}

// Include the built-in providers' credential selectors as well as their token aliases.
fn credentials_changed(a: &Credential, b: &Credential) -> bool {
    const KEYS: &[&str] = &[
        "access_token",
        "accessToken",
        "refresh_token",
        "refreshToken",
        "id_token",
        "idToken",
        "api_key",
        "account_id",
        "client_id",
        "client_secret",
        "device_id",
        "session_id",
        "dca_token",
    ];
    KEYS.iter().any(|k| a.metadata.get(*k) != b.metadata.get(*k))
        || a.attributes.get("api_key") != b.attributes.get("api_key")
        || KEYS
            .iter()
            .any(|k| a.metadata.get("token").and_then(|t| t.get(*k)) != b.metadata.get("token").and_then(|t| t.get(*k)))
}

/// Go `mergeAuthContent`: preserve unrelated user edits while retaining rotated tokens.
fn rebase(base: &Credential, latest: &Credential, patch: &MetadataPatch) -> MetadataPatch {
    let payload = |key: &str| {
        matches!(
            key,
            "access_token"
                | "refresh_token"
                | "id_token"
                | "session_id"
                | "expired"
                | "last_refresh"
                | "expires_in"
                | "timestamp"
                | "token_type"
                | "user_code"
                | "verification_uri"
                | "verification_uri_complete"
                | "token"
        ) || base.provider == "codex" && matches!(key, "account_id" | "email" | "plan_type")
    };
    MetadataPatch {
        set: patch
            .set
            .iter()
            .filter(|(key, value)| {
                base.metadata.get(*key) != Some(*value)
                    && (base.metadata.get(*key) == latest.metadata.get(*key) || payload(key))
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        remove: patch
            .remove
            .iter()
            .filter(|key| base.metadata.contains_key(*key) && base.metadata.get(*key) == latest.metadata.get(*key))
            .cloned()
            .collect(),
    }
}

impl CredentialStore {
    pub(super) fn get_versioned(&self, id: &str) -> Option<(Arc<Credential>, Version)> {
        let inner = self.read();
        Some((
            inner.creds.iter().find(|c| c.id == id)?.clone(),
            *inner.versions.get(id)?,
        ))
    }

    pub(crate) fn reload_guard(&self) -> std::sync::RwLockWriteGuard<'_, ()> {
        self.reload.write().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn write_lock(&self, id: &str) -> Arc<Mutex<()>> {
        self.write_locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(id.to_owned())
            .or_default()
            .clone()
    }

    pub(super) fn apply_refresh(
        &self,
        base: &Credential,
        version: Version,
        patch: &MetadataPatch,
    ) -> Result<Arc<Credential>, PatchError> {
        let _gate = self.reload.read().unwrap_or_else(PoisonError::into_inner);
        let lock = self.write_lock(&base.id);
        let _own = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let (current, now) = self.get_versioned(&base.id).ok_or(PatchError::NotFound)?;
        if version.registration != now.registration {
            return Err(PatchError::NotFound);
        }
        if version.credentials != now.credentials {
            return Ok(current);
        }
        let rebased;
        let patch = if current.revision == base.revision {
            patch
        } else {
            rebased = rebase(base, &current, patch);
            &rebased
        };
        if patch.set.is_empty() && patch.remove.is_empty() {
            return Ok(current);
        }
        self.persist_patch(&current, patch)
    }

    /// Revision-checked file update; its per-ID writer and shared reload gate span persistence.
    pub fn apply_patch(
        &self,
        id: &str,
        expected_revision: u64,
        patch: &MetadataPatch,
    ) -> Result<Arc<Credential>, PatchError> {
        let _gate = self.reload.read().unwrap_or_else(PoisonError::into_inner);
        let lock = self.write_lock(id);
        let _own = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let current = self.get(id).ok_or(PatchError::NotFound)?;
        if current.revision != expected_revision {
            return Err(PatchError::Stale {
                current: current.revision,
            });
        }
        self.persist_patch(&current, patch)
    }

    fn persist_patch(&self, current: &Credential, patch: &MetadataPatch) -> Result<Arc<Credential>, PatchError> {
        if patch.remove.iter().any(|k| k == "type")
            || patch
                .set
                .get("type")
                .is_some_and(|t| t.as_str() != Some(current.provider.as_str()))
        {
            return Err(PatchError::TypeIsImmutable);
        }
        let Source::File(path) = &current.source else {
            return Err(PatchError::ConfigBacked);
        };
        let mut next = current.clone();
        patch.apply(&mut next.metadata);
        next.refresh_derived();
        next.metadata.insert("disabled".into(), Value::Bool(next.disabled));
        write_atomic(path, &next.metadata).map_err(|e| PatchError::Io(e.to_string()))?;
        let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let index = inner
            .creds
            .iter()
            .position(|c| c.id == current.id)
            .ok_or(PatchError::NotFound)?;
        inner.generation += 1;
        next.revision = inner.generation;
        inner.register(Some(current), &next);
        let next = Arc::new(next);
        inner.creds[index] = next.clone();
        inner.epoch += 1;
        self.changed.send_replace(());
        drop(inner);
        self.clear_disabled_cooldowns();
        Ok(next)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::sync::{OnceLock, mpsc};
    type Stall = (mpsc::Sender<()>, mpsc::Receiver<()>);
    static STALLS: OnceLock<Mutex<HashMap<std::path::PathBuf, Stall>>> = OnceLock::new();

    pub(crate) fn stall_write(path: &Path) {
        let stall = STALLS.get_or_init(Default::default).lock().unwrap().remove(path);
        if let Some((entered, release)) = stall {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(10)).unwrap();
        }
    }

    #[test]
    fn persistence_does_not_hold_the_selection_lock_during_disk_io() {
        for patch in [false, true] {
            let dir = std::env::temp_dir().join(format!("cpa-write-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("account.json");
            let metadata = serde_json::json!({"type":"claude", "access_token":"fake"});
            std::fs::write(&path, metadata.to_string()).unwrap();
            let store = CredentialStore::new(vec![
                Credential::from_file(&dir, &path, metadata.as_object().unwrap().clone()).unwrap(),
            ]);
            let current = store.get("account.json").unwrap();
            let (entered_tx, entered) = mpsc::channel();
            let (release, release_rx) = mpsc::channel();
            STALLS
                .get_or_init(Default::default)
                .lock()
                .unwrap()
                .insert(path.clone(), (entered_tx, release_rx));
            let writer = std::thread::spawn({
                let store = store.clone();
                move || {
                    if patch {
                        store
                            .apply_patch(
                                &current.id,
                                current.revision,
                                &MetadataPatch {
                                    set: Map::from_iter([("note".into(), "edited".into())]),
                                    remove: vec![],
                                },
                            )
                            .unwrap();
                    } else {
                        store.persist_credential(&current);
                    }
                }
            });
            entered.recv_timeout(Duration::from_secs(5)).unwrap();
            let (selected_tx, selected) = mpsc::channel();
            let reader = std::thread::spawn({
                let store = store.clone();
                move || {
                    selected_tx
                        .send(
                            store
                                .select(Selection {
                                    provider: "claude".into(),
                                    ..Default::default()
                                })
                                .is_some(),
                        )
                        .unwrap();
                }
            });
            let selectable = selected.recv_timeout(Duration::from_millis(500));
            release.send(()).unwrap();
            writer.join().unwrap();
            reader.join().unwrap();
            std::fs::remove_dir_all(&dir).unwrap();
            assert_eq!(
                selectable.ok(),
                Some(true),
                "selection stalled during disk persistence (patch={patch})"
            );
        }
    }
    #[tokio::test]
    async fn reload_waits_for_persistence_while_other_credentials_remain_usable() {
        let dir = std::env::temp_dir().join(format!("cpa-reload-write-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.yaml");
        std::fs::write(&config_path, format!("auth-dir: {}\n", dir.display())).unwrap();
        for name in ["a.json", "b.json"] {
            std::fs::write(dir.join(name), r#"{"type":"claude","access_token":"old-fake"}"#).unwrap();
        }
        let cfg = Config::load(&config_path).unwrap();
        let rt = Arc::new(crate::testing::runtime(
            cfg.clone(),
            cpa_core::config::credentials::load(&cfg),
            Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        let state = crate::management::Management::new(rt.clone(), config_path);
        let a = rt.store.get("a.json").unwrap();
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        STALLS
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(dir.join("a.json"), (entered_tx, release_rx));
        let writer = std::thread::spawn({
            let store = rt.store.clone();
            move || {
                store
                    .apply_patch(
                        &a.id,
                        a.revision,
                        &MetadataPatch {
                            set: Map::from_iter([("access_token".into(), "rotated-fake".into())]),
                            remove: vec![],
                        },
                    )
                    .unwrap()
            }
        });
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let (other_tx, other) = mpsc::channel();
        let other_writer = std::thread::spawn({
            let store = rt.store.clone();
            move || {
                let b = store.get("b.json").unwrap();
                let result = store.apply_patch(
                    &b.id,
                    b.revision,
                    &MetadataPatch {
                        set: Map::from_iter([("note".into(), "changed".into())]),
                        remove: vec![],
                    },
                );
                other_tx.send(result.is_ok()).unwrap();
            }
        });
        let independent = other.recv_timeout(Duration::from_millis(500));
        let (loaded_tx, loaded) = mpsc::channel();
        let reload = std::thread::spawn(move || {
            loaded_tx.send(crate::watching::reload(&state).is_ok()).unwrap();
        });
        let early_reload = loaded.recv_timeout(Duration::from_millis(100));
        let (read_tx, read) = mpsc::channel();
        let reader = std::thread::spawn({
            let store = rt.store.clone();
            move || {
                read_tx
                    .send(
                        store
                            .select(Selection {
                                provider: "claude".into(),
                                ..Default::default()
                            })
                            .is_some(),
                    )
                    .unwrap();
            }
        });
        let readable = read.recv_timeout(Duration::from_millis(500));
        release.send(()).unwrap();
        writer.join().unwrap();
        other_writer.join().unwrap();
        reader.join().unwrap();
        reload.join().unwrap();
        assert_eq!(independent.ok(), Some(true));
        assert!(matches!(early_reload, Err(mpsc::RecvTimeoutError::Timeout)));
        assert_eq!(readable.ok(), Some(true));
        assert!(loaded.recv_timeout(Duration::from_secs(5)).unwrap());
        assert_eq!(
            rt.store.get("a.json").unwrap().str("access_token"),
            Some("rotated-fake")
        );
        assert_eq!(rt.store.get("b.json").unwrap().str("note"), Some("changed"));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn codex_refresh_keeps_its_token_partition_and_unrelated_edits() {
        let dir = std::env::temp_dir().join(format!("cpa-codex-merge-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("account.json");
        let metadata = serde_json::json!({"type":"codex", "access_token":"old-fake", "refresh_token":"old-refresh-fake", "account_id":"account-fake", "email":"old@example.invalid", "note":"old", "obsolete":true});
        std::fs::write(&path, metadata.to_string()).unwrap();
        let store = CredentialStore::new(vec![
            Credential::from_file(&dir, &path, metadata.as_object().unwrap().clone()).unwrap(),
        ]);
        let (base, version) = store.get_versioned("account.json").unwrap();
        store
            .apply_patch(
                &base.id,
                base.revision,
                &MetadataPatch {
                    set: Map::from_iter([
                        ("email".into(), "edited@example.invalid".into()),
                        ("note".into(), "edited".into()),
                    ]),
                    remove: vec![],
                },
            )
            .unwrap();
        let updated = store
            .apply_refresh(
                &base,
                version,
                &MetadataPatch {
                    set: Map::from_iter([
                        ("access_token".into(), "rotated-fake".into()),
                        ("refresh_token".into(), "rotated-refresh-fake".into()),
                        ("email".into(), "refreshed@example.invalid".into()),
                        ("note".into(), "old".into()),
                    ]),
                    remove: vec!["obsolete".into()],
                },
            )
            .unwrap();
        assert_eq!(updated.str("access_token"), Some("rotated-fake"));
        assert_eq!(updated.str("refresh_token"), Some("rotated-refresh-fake"));
        assert_eq!(updated.str("email"), Some("refreshed@example.invalid"));
        assert_eq!(updated.str("note"), Some("edited"));
        assert!(!updated.metadata.contains_key("obsolete"));
        let disk: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(disk["refresh_token"], "rotated-refresh-fake");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
