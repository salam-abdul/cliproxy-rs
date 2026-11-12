use super::*;

/// Whether `next` would change nothing about the stored `current` (reconcile keeps
/// such a credential and its revision).
fn unchanged(current: &Credential, next: &Credential) -> bool {
    current.id == next.id
        && current.source == next.source
        && current.metadata == next.metadata
        && current.attributes == next.attributes
        && current.provider == next.provider
        && current.disabled == next.disabled
        && current.label == next.label
}

#[derive(Debug, PartialEq, Eq)]
pub enum PatchError {
    NotFound,
    /// The credential changed since the caller read it; re-read and retry.
    Stale {
        current: u64,
    },
    /// `type` decides the executor and cannot change in place.
    TypeIsImmutable,
    /// Config-sourced credentials persist through the config document, not here.
    ConfigBacked,
    Io(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AttemptStats {
    pub success: u64,
    pub failure: u64,
    pub cancelled: u64,
}

pub(super) struct Inner {
    pub(super) creds: Vec<Arc<Credential>>,
    /// Last revision handed out. Revisions are never reused.
    pub(super) generation: u64,
    pub(super) versions: HashMap<String, Version>,
    /// What selection reads from each stored revision, parsed when it is stored.
    pub(super) profiles: HashMap<String, Profile>,
    /// Bumped on every reconcile and patch, including pure removals.
    pub(super) epoch: u64,
}

impl Inner {
    /// The parsed profile of `c`'s revision; parsed now if the store holds another.
    pub(super) fn profile(&self, c: &Credential) -> std::borrow::Cow<'_, Profile> {
        match self.profiles.get(&c.id) {
            Some(profile) if profile.revision == c.revision => std::borrow::Cow::Borrowed(profile),
            _ => std::borrow::Cow::Owned(Profile::of(c)),
        }
    }
}

pub struct CredentialStore {
    pub(super) inner: RwLock<Inner>,
    // Lock order: management disk -> reload -> per-ID writer -> brief inner access.
    // Selection never takes reload or writer locks.
    pub(super) reload: RwLock<()>,
    pub(super) write_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    pub(super) policy: RwLock<Arc<Policy>>,
    pub(super) scheduler: Mutex<Scheduler>,
    pub(super) attempts: AtomicU64,
    pub(super) stats: [AtomicU64; 3],
    prepare_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// `auth-dir` while `save-cooldown-status` is on (Go `cooldownStore`).
    pub(super) cooldown_dir: RwLock<Option<std::path::PathBuf>>,
    /// Replaces the `.cds` files while set (Go's token-store cooldown provider).
    cooldown_backend: RwLock<Option<Arc<dyn crate::cooldown_store::Backend>>>,
    /// Serializes cooldown snapshots with their hand-over, so the last write is the newest.
    cooldown_write: Mutex<()>,
    cooldown_writer: Arc<super::cooldown_writer::CooldownWriter>,
    activity: Mutex<HashMap<String, CredentialActivity>>,
    /// Credential revisions whose file already holds Go's persisted form.
    persisted: Mutex<HashMap<String, u64>>,
    /// Where failed attempts publish Go's error events (set by the runtime).
    pub(super) error_events: std::sync::OnceLock<Arc<crate::usage::UsageQueue>>,
    /// Marked on every epoch bump, so background loops sleep until the set changes.
    pub(super) changed: tokio::sync::watch::Sender<()>,
}

impl CredentialStore {
    pub fn new(credentials: Vec<Credential>) -> Arc<Self> {
        let mut inner = Inner {
            creds: Vec::new(),
            generation: 0,
            versions: HashMap::new(),
            profiles: HashMap::new(),
            epoch: 0,
        };
        for mut cred in credentials {
            inner.generation += 1;
            cred.revision = inner.generation;
            inner.register(None, &cred);
            inner.creds.push(Arc::new(cred));
        }
        Arc::new(Self {
            inner: RwLock::new(inner),
            reload: RwLock::default(),
            write_locks: Mutex::default(),
            policy: RwLock::new(Arc::new(Policy::default())),
            scheduler: Mutex::default(),
            attempts: AtomicU64::new(0),
            stats: Default::default(),
            prepare_locks: Mutex::default(),
            cooldown_dir: RwLock::default(),
            cooldown_backend: RwLock::default(),
            cooldown_write: Mutex::default(),
            cooldown_writer: Arc::default(),
            activity: Mutex::default(),
            persisted: Mutex::default(),
            error_events: std::sync::OnceLock::new(),
            changed: tokio::sync::watch::Sender::new(()),
        })
    }

    /// Replaces the store's policy; selection state the change invalidates is reset.
    pub(super) fn set_policy(&self, policy: Arc<Policy>) {
        let mut current = self.policy.write().unwrap_or_else(PoisonError::into_inner);
        self.scheduler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .configure(&current, &policy);
        *current = policy;
    }

    /// Go `ApplyConfigWithCooldownStateStore` plus the restore that follows a config
    /// update: the current state goes to the old store before the swap, and a newly
    /// enabled store is restored from. `None` disables persistence.
    pub fn configure_cooldown_store(&self, dir: Option<std::path::PathBuf>) {
        let old = self.cooldown_dir.read().unwrap_or_else(PoisonError::into_inner).clone();
        if old == dir {
            return;
        }
        if old.is_some() {
            self.persist_cooldowns();
        }
        *self.cooldown_dir.write().unwrap_or_else(PoisonError::into_inner) = dir.clone();
        if let Some(dir) = dir {
            self.restore_cooldowns(&dir);
        }
    }

    /// Go's builder taking the token store's `CooldownStateStore`: cooldowns go to
    /// `backend` instead of `.cds` files, restored from it now when persistence is on.
    pub fn set_cooldown_backend(&self, backend: Arc<dyn crate::cooldown_store::Backend>) {
        *self.cooldown_backend.write().unwrap_or_else(PoisonError::into_inner) = Some(backend);
        let dir = self.cooldown_dir.read().unwrap_or_else(PoisonError::into_inner).clone();
        if let Some(dir) = dir {
            self.restore_cooldowns(&dir);
        }
    }

    fn cooldown_backend(&self) -> Option<Arc<dyn crate::cooldown_store::Backend>> {
        self.cooldown_backend
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Go `RestoreCooldownStates`: live records of live credentials whose cooling is
    /// enabled, then a rewrite that drops everything else.
    fn restore_cooldowns(&self, dir: &std::path::Path) {
        self.flush_cooldowns();
        let loaded = match self.cooldown_backend() {
            Some(backend) => backend.load(),
            None => crate::cooldown_store::load(dir),
        };
        let records = match loaded {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(%error, "failed to restore cooldown state");
                return;
            }
        };
        if !records.is_empty() {
            let policy = self.policy.read().unwrap_or_else(PoisonError::into_inner).clone();
            let inner = self.read();
            let mut scheduler = self.scheduler.lock().unwrap_or_else(PoisonError::into_inner);
            let (now, wall) = (Instant::now(), std::time::SystemTime::now());
            for record in &records {
                let Some(c) = inner.creds.iter().find(|c| c.id == record.auth_id.trim()) else {
                    continue;
                };
                if c.disabled || policy.cooling_disabled(c) {
                    continue;
                }
                let has_models = records
                    .iter()
                    .any(|r| r.auth_id.trim() == c.id && !r.model.trim().is_empty());
                scheduler.restore(c, record, has_models, policy.max_trusted_cooldown, now, wall);
            }
        }
        self.persist_cooldowns();
    }

    /// Go `persistCooldownStates`: saves the live cooldowns. The snapshot is taken here
    /// and written off this thread (see [`super::cooldown_writer`]).
    pub fn persist_cooldowns(&self) {
        self.persist_cooldowns_changed(None);
    }

    /// [`Self::persist_cooldowns`] after a change to one credential's cooldowns only:
    /// the other credentials' files are left as they are.
    pub(super) fn persist_cooldowns_of(&self, id: &str) {
        self.persist_cooldowns_changed(Some(std::collections::BTreeSet::from([id.to_owned()])));
    }

    fn persist_cooldowns_changed(&self, changed: Option<std::collections::BTreeSet<String>>) {
        let Some(dir) = self.cooldown_dir.read().unwrap_or_else(PoisonError::into_inner).clone() else {
            return;
        };
        let _write = self.cooldown_write.lock().unwrap_or_else(PoisonError::into_inner);
        let wall = std::time::SystemTime::now();
        let mut records = {
            let policy = self.policy.read().unwrap_or_else(PoisonError::into_inner).clone();
            let inner = self.read();
            let scheduler = self.scheduler.lock().unwrap_or_else(PoisonError::into_inner);
            let now = Instant::now();
            inner
                .creds
                .iter()
                .filter(|c| !c.disabled && !policy.cooling_disabled(c))
                .flat_map(|c| scheduler.records(c, now, wall))
                .collect::<Vec<_>>()
        };
        records.sort_by(|a, b| (&a.provider, &a.auth_id, &a.model).cmp(&(&b.provider, &b.auth_id, &b.model)));
        let target = match self.cooldown_backend() {
            Some(backend) => super::cooldown_writer::Target::Backend(backend),
            None => super::cooldown_writer::Target::Files(dir),
        };
        self.cooldown_writer.submit(super::cooldown_writer::Snapshot {
            target,
            records,
            now: wall,
            changed,
        });
    }

    /// Blocks until every cooldown change so far is saved.
    pub fn flush_cooldowns(&self) {
        self.cooldown_writer.flush();
    }

    /// Go `Manager.persist` after a result (`FileTokenStore.Save`, metadata branch): the
    /// file gets `"disabled"` and Go's `json.Marshal` form unless it already holds the
    /// same JSON. Checked once per revision, so a file is not read on every request.
    // ponytail: a removed file is not recreated (Go recreates one deleted between watcher
    // reloads); recreate it if a deleted credential must come back. The write is an
    // atomic replace instead of Go's in-place truncate, so a crash never leaves a
    // truncated file.
    pub(super) fn persist_credential(&self, credential: &Arc<Credential>) {
        let Source::File(path) = &credential.source else {
            return;
        };
        if credential.disabled
            || credential
                .attributes
                .get("runtime_only")
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
        {
            return;
        }
        {
            let persisted = self.persisted.lock().unwrap_or_else(PoisonError::into_inner);
            if persisted.get(&credential.id) == Some(&credential.revision) {
                return;
            }
        }
        let _gate = self.reload.read().unwrap_or_else(PoisonError::into_inner);
        let lock = self.write_lock(&credential.id);
        let _own = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(slot) = self.get(&credential.id).filter(|c| c.revision == credential.revision) else {
            return;
        };
        let mut metadata = slot.metadata.clone();
        metadata.insert("disabled".into(), Value::Bool(slot.disabled));
        let go = |bytes: &[u8]| cpa_common::json::GoValue::parse_f64(bytes);
        let Some(target) = serde_json::to_vec(&metadata).ok().and_then(|b| go(&b)) else {
            return;
        };
        let Ok(existing) = std::fs::read(path) else {
            return;
        };
        if go(&existing).as_ref() != Some(&target) {
            let bytes = target.marshal();
            if let Err(error) = write_bytes_atomic(path, &bytes) {
                tracing::warn!(id = %credential.id, %error, "failed to persist credential");
                return;
            }
            // Same revision: the file now says what memory already meant (absent
            // `disabled` is false, numbers are float64), so memory takes the written
            // form and the watcher's reload finds nothing changed.
            if let Ok(written) = serde_json::from_slice::<Map<String, Value>>(&bytes) {
                let mut next = Credential::clone(&slot);
                next.metadata = written;
                let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
                let index = inner
                    .creds
                    .iter()
                    .position(|c| c.id == credential.id)
                    .expect("writer owns credential");
                inner.creds[index] = Arc::new(next);
            }
        }
        self.persisted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(credential.id.clone(), credential.revision);
    }

    /// Go `clearDisabledCooldownStates` after a policy change.
    pub(super) fn clear_disabled_cooldowns(&self) {
        let policy = self.policy.read().unwrap_or_else(PoisonError::into_inner).clone();
        let cleared = {
            let inner = self.read();
            self.scheduler
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clear_disabled(&inner.creds, &policy)
        };
        if cleared {
            self.persist_cooldowns();
        }
    }

    pub fn snapshot(&self) -> Vec<Arc<Credential>> {
        self.read().creds.clone()
    }

    pub fn get(&self, id: &str) -> Option<Arc<Credential>> {
        self.read().creds.iter().find(|c| c.id == id).cloned()
    }

    /// Resolves after the next change to the credential set or any credential.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<()> {
        self.changed.subscribe()
    }

    /// Changes whenever the credential set or any credential changes.
    pub fn epoch(&self) -> u64 {
        self.read().epoch
    }

    /// Attempts finished so far, by outcome.
    pub fn stats(&self) -> AttemptStats {
        let get = |i: usize| self.stats[i].load(Ordering::Relaxed);
        AttemptStats {
            success: get(0),
            failure: get(1),
            cancelled: get(2),
        }
    }

    /// Serializes preparation of one credential across concurrent requests.
    pub fn prepare_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.prepare_locks.lock().unwrap_or_else(PoisonError::into_inner);
        locks.entry(id.to_owned()).or_default().clone()
    }

    /// Replaces a config-backed or runtime-only credential in memory if it is still at
    /// `expected_revision`: Go's `Manager.Update` never persists config API keys (the
    /// next config publish re-synthesizes them) or `runtime_only` auths. `NotFound`
    /// unless `next.id` names such a credential.
    pub fn replace_config_backed(
        &self,
        next: Credential,
        expected_revision: u64,
    ) -> Result<Arc<Credential>, PatchError> {
        let _gate = self.reload_guard();
        let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let generation = inner.generation + 1;
        let slot = inner
            .creds
            .iter_mut()
            .find(|c| c.id == next.id && matches!(c.source, Source::Config { .. } | Source::Runtime))
            .ok_or(PatchError::NotFound)?;
        if slot.revision != expected_revision {
            return Err(PatchError::Stale { current: slot.revision });
        }
        let mut next = next;
        next.source = slot.source.clone();
        next.revision = generation;
        let previous = slot.clone();
        *slot = Arc::new(next);
        let committed = slot.clone();
        inner.register(Some(&previous), &committed);
        inner.generation = generation;
        // Registrations depend on the credential (prefix, models): invalidate the
        // registry cache like `apply_patch` does.
        inner.epoch += 1;
        self.changed.send_replace(());
        drop(inner);
        // Go `Manager.Update` clears the cooldowns of an auth it stores disabled (or
        // with cooling off), so a later re-enable routes to it at once.
        self.clear_disabled_cooldowns();
        Ok(committed)
    }

    /// Replaces the credential set (watcher reload, management import/delete). Unchanged
    /// credentials keep their revision; new and changed ones get fresh revisions.
    pub fn reconcile(&self, credentials: Vec<Credential>) {
        let _gate = self.reload_guard();
        self.reconcile_locked(credentials);
    }

    /// Caller holds the exclusive reload gate, including any scan supplying this set.
    pub(crate) fn reconcile_locked(&self, credentials: Vec<Credential>) {
        let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let mut next = Vec::with_capacity(credentials.len());
        for mut cred in credentials {
            match inner.creds.iter().find(|c| unchanged(c, &cred)) {
                Some(existing) => next.push(existing.clone()),
                None => {
                    inner.generation += 1;
                    cred.revision = inner.generation;
                    let previous = inner.creds.iter().find(|c| c.id == cred.id).cloned();
                    inner.register(previous.as_deref(), &cred);
                    next.push(Arc::new(cred));
                }
            }
        }
        // Runtime-only credentials (relay sessions) have no file or config entry; they
        // stay until their session ends.
        let runtime: Vec<_> = inner
            .creds
            .iter()
            .filter(|c| c.source == Source::Runtime && !next.iter().any(|n| n.id == c.id))
            .cloned()
            .collect();
        next.extend(runtime);
        inner.versions.retain(|id, _| next.iter().any(|c| &c.id == id));
        inner.profiles.retain(|id, _| next.iter().any(|c| &c.id == id));
        inner.creds = next;
        inner.epoch += 1;
        self.changed.send_replace(());
        self.scheduler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reconcile(&inner.creds);
        drop(inner);
        self.clear_disabled_cooldowns();
        self.persist_cooldowns();
    }

    /// Adds a runtime-only credential (Go's auth add for a `/v1/ws` relay session)
    /// unless one with its ID is already active. Additive API for the relay route.
    pub fn add_runtime(&self, mut credential: Credential) -> bool {
        let _gate = self.reload_guard();
        let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        if inner.creds.iter().any(|c| c.id == credential.id && !c.disabled) {
            return false;
        }
        inner.creds.retain(|c| c.id != credential.id);
        inner.generation += 1;
        credential.revision = inner.generation;
        inner.register(None, &credential);
        inner.creds.push(Arc::new(credential));
        inner.epoch += 1;
        self.changed.send_replace(());
        self.scheduler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reconcile(&inner.creds);
        true
    }

    /// Removes a credential when its relay session ends (Go's auth delete). Additive
    /// API for the relay route.
    pub fn remove_runtime(&self, id: &str) {
        let _gate = self.reload_guard();
        let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let before = inner.creds.len();
        inner.creds.retain(|c| !(c.id == id && c.source == Source::Runtime));
        if inner.creds.len() == before {
            return;
        }
        inner.versions.remove(id);
        inner.profiles.remove(id);
        inner.epoch += 1;
        self.changed.send_replace(());
        self.scheduler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reconcile(&inner.creds);
    }

    pub(super) fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Per-credential outcome counters for management views (Go `Auth.Success`/`Failed`
/// and its 20 x 10-minute recent-request ring). Additive read API.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CredentialActivity {
    pub success: u64,
    pub failed: u64,
    /// `(bucket, success, failed)` for the most recent buckets, where `bucket` is
    /// Unix seconds / 600; at most 20 entries, oldest first.
    pub recent: Vec<(i64, u64, u64)>,
}

pub const RECENT_BUCKET_SECONDS: i64 = 600;
const RECENT_BUCKETS: usize = 20;

impl CredentialStore {
    pub(crate) fn note_activity(&self, id: &str, success: bool) {
        let bucket = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64 / RECENT_BUCKET_SECONDS);
        let mut all = self.activity.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = all.entry(id.to_owned()).or_default();
        if success {
            entry.success += 1;
        } else {
            entry.failed += 1;
        }
        match entry.recent.last_mut() {
            Some(last) if last.0 == bucket => {
                if success {
                    last.1 += 1;
                } else {
                    last.2 += 1;
                }
            }
            _ => entry.recent.push((bucket, u64::from(success), u64::from(!success))),
        }
        entry.recent.retain(|(b, _, _)| bucket - b < RECENT_BUCKETS as i64);
    }

    /// Counters for one credential; zero when it has served nothing yet.
    pub fn activity(&self, id: &str) -> CredentialActivity {
        self.activity
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    /// Active cooldowns of one credential.
    pub fn cooldowns(&self, id: &str) -> Vec<crate::scheduler::CooldownState> {
        self.scheduler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cooldowns_of(id, Instant::now())
    }

    /// Clears the cooldowns of one credential (Go `Manager.ResetQuota`); returns the
    /// model keys that were cooling.
    pub fn reset_cooldowns(&self, id: &str) -> Vec<String> {
        let had = !self.cooldowns(id).is_empty();
        let models = self
            .scheduler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reset_cooldowns(id);
        if had {
            // Go `ResetQuota` persists the cleared state.
            self.persist_cooldowns_of(id);
        }
        models
    }
}

/// Writes `metadata` the way Go's `json.NewEncoder(f).Encode` does (compact JSON and a
/// newline) via an exclusively created, uniquely named 0600 sibling and a rename. The
/// temp name does not end in `.json`, so a crash never leaves a loadable credential.
pub(super) fn write_atomic(path: &Path, metadata: &Map<String, Value>) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(metadata)?;
    bytes.push(b'\n');
    write_bytes_atomic(path, &bytes)
}

/// Replaces `path` with `bytes` through an exclusively created 0600 sibling.
pub(crate) fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(test)]
    credential_updates::tests::stall_write(path);
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let (tmp, mut file) = loop {
        let tmp = dir.join(format!(
            ".{name}.{}.{}.tmp",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        // create_new is O_CREAT|O_EXCL: it never follows or reuses an existing path.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        // Go's 0600; on Windows Go ignores mode bits and creates the file plainly.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(&tmp) {
            Ok(file) => break (tmp, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    let result = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}
