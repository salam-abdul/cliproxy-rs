//! Hot reload of `config.yaml` and the auth directory (Go internal/watcher).
//!
//! Go watches the config file and the top level of the auth directory with fsnotify.
//! A config event reloads after 150 ms without further events (`configReloadDebounce`);
//! auth events apply at once, removals after a 50 ms replace check
//! (`replaceCheckDelay`). Both compare content hashes, so self-writes and
//! unchanged rewrites are no-ops.
//!
//! The watcher sleeps until the operating system reports a change in the config file's
//! folder or the auth folder ([`crate::fs_events`]). It then stats the config file and
//! the top-level `*.json` auth files every 50 ms, hashing only files whose metadata
//! changed or that were modified in the last two seconds (git's racy timestamp rule),
//! until the change has been stable for 150 ms (config) or one tick (auth files) and is
//! applied. While nothing changes it does no work and sets no timer. As in Go, changes
//! made by another machine on a network or FUSE file system raise no event and are seen
//! with the next local change.
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use cpa_core::config::{Config, credentials};

use sha2::{Digest, Sha256};

use crate::fs_events::{Events, Targets};
use crate::management::Management;

const TICK: Duration = Duration::from_millis(50);
/// Go `configReloadDebounce`.
const CONFIG_SETTLE: Duration = Duration::from_millis(150);
/// Go `replaceCheckDelay`: one tick.
const AUTH_SETTLE: Duration = Duration::from_millis(50);
/// Files modified this recently are re-hashed even when their metadata is unchanged:
/// coarse file timestamps cannot tell two quick same-size writes apart.
const RACY: Duration = Duration::from_secs(2);

type Hash = [u8; 32];

/// Content identity of everything the watcher observes.
#[derive(Clone, Default, PartialEq, Eq)]
struct Snapshot {
    /// `None` when the config file cannot be read (missing).
    config: Option<Hash>,
    auth: BTreeMap<PathBuf, Hash>,
    directory_incomplete: bool,
}

/// File metadata that changes with any write or replacement.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stat {
    len: u64,
    ino: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Stat {
    #[cfg(unix)]
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            len: meta.len(),
            ino: meta.ino(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        }
    }

    /// ponytail: Windows exposes no stable file identity or change time here, so a
    /// same-size replacement with an unchanged modification time older than two
    /// seconds goes unnoticed until its next change. Hash small files on Windows if
    /// such replacements must be seen.
    #[cfg(not(unix))]
    fn of(meta: &std::fs::Metadata) -> Self {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or((0, 0), |d| (d.as_secs() as i64, i64::from(d.subsec_nanos())));
        Self {
            len: meta.len(),
            ino: 0,
            mtime,
            ctime: (0, 0),
        }
    }
}

/// Hashes by path, reused while a file's metadata is unchanged and not racy.
#[derive(Default)]
struct HashCache(BTreeMap<PathBuf, (Stat, Hash)>);

impl HashCache {
    /// The content hash of `path`, or `None` when it cannot be read.
    fn hash(&mut self, path: &Path, meta: &std::fs::Metadata, now: SystemTime) -> Option<Hash> {
        let stat = Stat::of(meta);
        let racy = meta
            .modified()
            .map_or(true, |m| now.duration_since(m).map_or(true, |age| age < RACY));
        if let Some((cached, hash)) = self.0.get(path)
            && *cached == stat
            && !racy
        {
            return Some(*hash);
        }
        let data = std::fs::read(path).ok()?;
        let hash: Hash = Sha256::digest(&data).into();
        self.0.insert(path.to_owned(), (stat, hash));
        Some(hash)
    }
}

fn is_auth_json(path: &Path) -> bool {
    // Go matches the extension case-insensitively when scanning.
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().to_lowercase().ends_with(".json"))
}

fn observe(state: &Management, cache: &mut HashCache) -> Snapshot {
    let now = SystemTime::now();
    let mut seen = Vec::new();
    let config = std::fs::metadata(&state.path)
        .ok()
        .and_then(|meta| cache.hash(&state.path, &meta, now));
    seen.push(state.path.clone());
    let mut auth = BTreeMap::new();
    let dir = state.rt.config().auth_dir.clone();
    let mut directory_incomplete = false;
    match std::fs::read_dir(&dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(_) => {
                        directory_incomplete = true;
                        continue;
                    }
                };
                let path = entry.path();
                if !is_auth_json(&path) {
                    continue;
                }
                let hash = match std::fs::metadata(&path) {
                    Ok(meta) if meta.is_file() => cache.hash(&path, &meta, now).unwrap_or_default(),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    _ => Hash::default(),
                };
                seen.push(path.clone());
                auth.insert(path, hash);
            }
        }
        Err(error) => directory_incomplete = error.kind() != std::io::ErrorKind::NotFound,
    }
    if directory_incomplete {
        remember_unreadable_directory(state, &dir, &mut auth);
    }
    cache.0.retain(|path, _| seen.contains(path));
    Snapshot {
        config,
        auth,
        directory_incomplete,
    }
}

// Keep known members present with an unknown hash during enumeration failure. This
// also preserves the evidence needed to report a later real deletion to remote stores.
fn remember_unreadable_directory(state: &Management, dir: &Path, auth: &mut BTreeMap<PathBuf, Hash>) {
    let incomplete = credentials::AuthScan {
        directory_incomplete: true,
        ..Default::default()
    };
    for credential in state.rt.store().snapshot() {
        if let cpa_core::credential::Source::File(path) = &credential.source
            && incomplete.preserves(dir, path)
        {
            auth.entry(dir.join(path.file_name().unwrap_or_default())).or_default();
        }
    }
}

/// Go's per-event log lines for auth files that changed between two snapshots.
fn log_auth_changes(before: &Snapshot, after: &Snapshot) {
    let name = |p: &Path| p.file_name().unwrap_or_default().to_string_lossy().into_owned();
    for (path, hash) in &after.auth {
        let op = match before.auth.get(path) {
            None => "CREATE",
            Some(old) if old != hash => "WRITE",
            Some(_) => continue,
        };
        tracing::info!("auth file changed ({op}): {}, processing incrementally", name(path));
    }
    for path in before
        .auth
        .keys()
        .filter(|p| !after.directory_incomplete && !after.auth.contains_key(*p))
    {
        tracing::info!("auth file changed (REMOVE): {}, processing incrementally", name(path));
    }
}

pub fn start(state: &Arc<Management>) -> tokio::task::JoinHandle<()> {
    // Go stores its own serialized config snapshot, independently of management
    // mutations of the published runtime config before their disk event arrives.
    let (mut previous_config, initial_hash) = {
        let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
        (
            state.rt.config(),
            std::fs::read(&state.path)
                .ok()
                .map(|data| <Hash>::from(Sha256::digest(data))),
        )
    };
    let first = targets(state, None);
    let state = Arc::downgrade(state);
    tokio::spawn(async move {
        // Off the request threads: setup can block on a slow network path, and on
        // Windows it waits for the watch thread. A panic leaves the 2-second poll.
        let mut events = tokio::task::spawn_blocking(move || Events::new(&first))
            .await
            .unwrap_or_default();
        let mut cache = HashCache::default();
        // The state last reloaded; the first stable observation reloads once, as Go's
        // watcher reloads clients when it starts.
        let mut applied: Option<Snapshot> = None;
        let mut observed: Option<Snapshot> = None;
        let mut since = Instant::now();
        loop {
            let woke = applied.is_some() && applied == observed;
            if woke {
                events.changed().await;
            } else {
                tokio::time::sleep(TICK).await;
            }
            let Some(state) = state.upgrade() else {
                return;
            };
            // After an event, retarget before looking: a file that lands before the new
            // watch exists shows up in this look, and one that lands after raises an
            // event. Cost per wake: a few watch calls (one stat per watched file on
            // macOS), on the blocking pool.
            let wake_targets = woke.then(|| targets(&state, observed.as_ref()));
            // A panic in the work is caught inside the closure, so `events` always comes
            // back and notifications stay on.
            let looked = tokio::task::spawn_blocking({
                let state = state.clone();
                move || {
                    let snapshot = catch_unwind(AssertUnwindSafe(|| {
                        if let Some(t) = wake_targets {
                            events.retarget(&t);
                        }
                        let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
                        observe(&state, &mut cache)
                    }));
                    (snapshot.ok(), cache, events)
                }
            })
            .await;
            let snapshot = match looked {
                Ok((snapshot, returned_cache, returned_events)) => {
                    cache = returned_cache;
                    events = returned_events;
                    snapshot
                }
                // Cancelled at shutdown.
                Err(_) => {
                    cache = HashCache::default();
                    events = Events::default();
                    None
                }
            };
            let Some(snapshot) = snapshot else {
                tracing::error!(
                    "config watcher: looking at the config and auth files failed; retrying on the next change"
                );
                continue;
            };
            if observed.as_ref() != Some(&snapshot) {
                observed = Some(snapshot);
                since = Instant::now();
                continue;
            }
            let Some(current) = observed.clone() else { continue };
            if applied.as_ref() == Some(&current) {
                continue;
            }
            let config_changed = applied.as_ref().map_or(initial_hash, |a| a.config) != current.config;
            // The first observation always reads the file: an edit made after the
            // runtime loaded its config but before `start` hashed the file matches
            // `initial_hash`, yet the runtime has never seen it.
            let read_config = config_changed || applied.is_none();
            if since.elapsed() < if read_config { CONFIG_SETTLE } else { AUTH_SETTLE } {
                continue;
            }
            if let Some(before) = &applied {
                log_auth_changes(before, &current);
                if config_changed && current.config.is_some() {
                    tracing::info!("config file changed, reloading: {}", state.path.display());
                }
            }
            // The reload may move auth-dir, and new auth files need their own watches on
            // macOS: retarget after it, on the same blocking thread. A change that lands
            // before a new watch exists is caught by the extra look `Events` then makes.
            let reloaded = tokio::task::spawn_blocking({
                let state = state.clone();
                let current = current.clone();
                move || {
                    let loaded = catch_unwind(AssertUnwindSafe(|| reload_snapshot(&state, read_config, &current)));
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        events.retarget(&targets(&state, Some(&current)));
                    }));
                    (loaded.map_err(|_| ()), events)
                }
            })
            .await;
            let (loaded, read) = match reloaded {
                Ok((loaded, returned)) => {
                    events = returned;
                    match loaded {
                        Ok((loaded, read)) => (Ok(loaded), Some(read)),
                        Err(()) => (Err(()), None),
                    }
                }
                Err(_) => {
                    events = Events::default();
                    (Err(()), None)
                }
            };
            if loaded.is_err() {
                tracing::error!("config watcher: reload failed unexpectedly; retrying on the next change");
            }
            if let Some(before) = &applied {
                let accepted = matches!(loaded, Ok(Ok(Some(_))));
                persist_changes(&state, before, &current, config_changed && accepted);
            }
            match loaded {
                Ok(Ok(Some(next_config))) => {
                    // The first observation's read of the file the runtime started
                    // with is quiet; Go does not reload at startup.
                    if config_changed || next_config.document != previous_config.document {
                        crate::config_diff::log(&previous_config, &next_config);
                        tracing::info!("config successfully reloaded, triggering client reload");
                    }
                    previous_config = Arc::new(next_config);
                }
                Ok(Err(error)) if config_changed || applied.is_none() => match error {
                    ReloadError::Missing(e) => tracing::error!(
                        "failed to read config file for hash check: open {}: {}",
                        state.path.display(),
                        go_errno_text(&e)
                    ),
                    ReloadError::Invalid(e) => tracing::error!("failed to reload config: {e:#}"),
                },
                _ => {}
            }
            // Applied once either way: a failed config load retries on its next change,
            // as Go retries on its next file event, rather than every tick.
            applied = Some(record(&mut cache, current, read));
        }
    })
}

// Record the read, not the earlier observation: a restored edit must trigger another reload.
fn record(cache: &mut HashCache, observed: Snapshot, read: Option<Snapshot>) -> Snapshot {
    let read = read.unwrap_or_else(|| observed.clone());
    if read != observed {
        *cache = HashCache::default();
    }
    read
}

/// What the watch should cover: the config file, the auth folder of the published
/// config, and the auth files last seen there.
fn targets(state: &Management, current: Option<&Snapshot>) -> Targets {
    Targets {
        config: state.path.clone(),
        auth_dir: state.rt.config().auth_dir.clone(),
        files: current.map(|s| s.auth.keys().cloned().collect()).unwrap_or_default(),
    }
}

/// Go `persistConfigAsync` / `persistAuthAsync`: with a remote store, what changed
/// between two applied snapshots is pushed in the background. The config goes only
/// after a reload accepted it, and never when empty or missing (Go ignores those
/// events). An auth write goes as "Sync auth <name>" unless the file is empty or not a
/// JSON object; a removal goes as "Remove auth <name>".
// ponytail: Go also skips files whose JSON does not decode into its `Auth` struct or
// whose weight is invalid; here any JSON object is pushed. Decode into the credential
// type first if invalid files must be skipped.
fn persist_changes(state: &Management, before: &Snapshot, after: &Snapshot, config_accepted: bool) {
    let Some(store) = state.store.clone() else {
        return;
    };
    let config = config_accepted && std::fs::metadata(&state.path).is_ok_and(|m| m.len() > 0);
    let mut auth = Vec::new();
    for (path, hash) in &after.auth {
        if before.auth.get(path) == Some(hash) {
            continue;
        }
        let object = std::fs::read(path)
            .ok()
            .filter(|data| !data.is_empty())
            .is_some_and(|data| serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&data).is_ok());
        if object {
            auth.push(("Sync", path.clone()));
        }
    }
    for path in before.auth.keys().filter(|p| !after.auth.contains_key(*p)) {
        auth.push(("Remove", path.clone()));
    }
    if !config && auth.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let bound = Duration::from_secs(30);
        if config {
            match tokio::time::timeout(bound, store.persist_config()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::error!("failed to persist config change: {error:#}"),
                Err(_) => tracing::error!("failed to persist config change: context deadline exceeded"),
            }
        }
        for (verb, path) in auth {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let message = format!("{verb} auth {name}");
            match tokio::time::timeout(bound, store.persist_auth_files(message, vec![path])).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::error!("failed to persist auth changes: {error:#}"),
                Err(_) => tracing::error!("failed to persist auth changes: context deadline exceeded"),
            }
        }
    });
}

/// Go's `syscall.Errno` text: strerror in lower case, without Rust's suffix.
fn go_errno_text(e: &std::io::Error) -> String {
    let text = e.to_string();
    let text = text.split(" (os error").next().unwrap_or_default();
    let mut chars = text.chars();
    chars
        .next()
        .map_or_else(String::new, |c| c.to_lowercase().chain(chars).collect())
}

/// Why [`reload`] kept the last good config.
#[derive(Debug)]
pub enum ReloadError {
    /// The config file could not be read (Go logs it and keeps running).
    Missing(std::io::Error),
    /// The config file did not load (Go `failed to reload config`).
    Invalid(anyhow::Error),
}

impl std::fmt::Display for ReloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(e) => write!(f, "{e}"),
            Self::Invalid(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for ReloadError {}

/// Blocking reload. Shared with deterministic tests; no provider calls are made.
///
/// A missing, empty or rejected config keeps the last good config, and the auth
/// directory is reconciled against it anyway: Go handles auth events independently
/// of the config file.
pub fn reload(state: &Management) -> Result<(), ReloadError> {
    reload_config(state, true).map(|_| ())
}

/// Return the config actually accepted under the disk lock, not a later runtime
/// snapshot possibly replaced by management. None is an auth-only/empty reload.
fn reload_config(state: &Management, read_config: bool) -> Result<Option<Config>, ReloadError> {
    reload_snapshot(state, read_config, &Snapshot::default()).0
}

fn reload_snapshot(
    state: &Management,
    read_config: bool,
    observed: &Snapshot,
) -> (Result<Option<Config>, ReloadError>, Snapshot) {
    let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
    let _gate = state.rt.store().reload_guard();
    let mut read = Snapshot {
        config: observed.config,
        auth: BTreeMap::new(),
        directory_incomplete: false,
    };
    let (mut config, config_error, accepted) = match read_config.then(|| std::fs::read(&state.path)) {
        None => ((*state.rt.config()).clone(), None, false),
        Some(Err(error)) => {
            read.config = None;
            ((*state.rt.config()).clone(), Some(ReloadError::Missing(error)), false)
        }
        Some(Ok(data)) => {
            read.config = Some(Sha256::digest(&data).into());
            match (!data.is_empty()).then(|| {
                std::str::from_utf8(&data)
                    .map_err(anyhow::Error::from)
                    .and_then(|text| Config::load_text(&state.path, text))
            }) {
                None => ((*state.rt.config()).clone(), None, false),
                Some(Ok(cfg)) => (cfg, None, true),
                Some(Err(error)) => ((*state.rt.config()).clone(), Some(ReloadError::Invalid(error)), false),
            }
        }
    };
    state.lock_auth_dir(&mut config);
    let scan = credentials::scan_auth_dir(&config);
    read.directory_incomplete = scan.directory_incomplete;
    let incomplete = scan.incomplete.clone();
    let result = accepted.then(|| config.clone());
    let auth_dir = config.auth_dir.clone();
    read.auth = state.publish_locked(config, Some(scan));
    for path in incomplete {
        read.auth.entry(path).or_default();
    }
    if read.directory_incomplete {
        remember_unreadable_directory(state, &auth_dir, &mut read.auth);
    }
    (config_error.map_or(Ok(result), Err), read)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (Arc<Management>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("cpa-watch-outcome-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        std::fs::write(&path, format!("oauth: {{auth-dir: {}}}\n", dir.display())).unwrap();
        let rt = Arc::new(crate::testing::runtime(
            Config::load(&path).unwrap(),
            Vec::new(),
            cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        (Management::new(rt, path), dir)
    }

    #[tokio::test]
    async fn accepted_config_is_captured_and_skipped_reloads_do_not_advance_it() {
        let (state, dir) = state();
        let config = |port| format!("port: {port}\noauth: {{auth-dir: {}}}\n", dir.display());
        std::fs::write(&state.path, config(9001)).unwrap();
        let accepted = reload_config(&state, true).unwrap().unwrap();
        state.rt.publish_config(Config::parse(&config(9002)).unwrap());
        assert_eq!(accepted.port, 9001); // Not the later management publication.
        std::fs::write(&state.path, "").unwrap();
        assert!(reload_config(&state, true).unwrap().is_none());
        assert_eq!(state.rt.config().port, 9002);
        std::fs::write(&state.path, config(9003)).unwrap();
        assert!(reload_config(&state, false).unwrap().is_none()); // Auth-only event.
        assert_eq!(state.rt.config().port, 9002);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn incomplete_scans_preserve_credentials_but_real_deletions_apply() {
        let (state, dir) = state();
        let path = dir.join("account.json");
        let original = r#"{"type":"claude","access_token":"fake"}"#;
        std::fs::write(&path, original).unwrap();
        reload(&state).unwrap();
        let revision = state.rt.store().get("account.json").unwrap().revision;
        // A path that cannot be read as a file is an incomplete scan, not removal.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        reload(&state).unwrap();
        assert_eq!(state.rt.store().get("account.json").map(|c| c.revision), Some(revision));
        std::fs::remove_dir(&path).unwrap();
        reload(&state).unwrap();
        assert!(state.rt.store().get("account.json").is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn unreadable_auth_directory_keeps_credentials_and_publishes_config() {
        let (state, dir) = state();
        let auth = dir.join("auth");
        std::fs::create_dir(&auth).unwrap();
        let config = |port| format!("port: {port}\nauth-dir: {}\n", auth.display());
        std::fs::write(&state.path, config(9001)).unwrap();
        std::fs::write(auth.join("account.json"), r#"{"type":"claude","access_token":"fake"}"#).unwrap();
        reload(&state).unwrap();
        let saved = dir.join("saved");
        std::fs::rename(&auth, &saved).unwrap();
        std::fs::write(&auth, "not a directory").unwrap();
        std::fs::write(&state.path, config(9002)).unwrap();
        reload(&state).unwrap();
        assert_eq!(state.rt.config().port, 9002);
        assert!(state.rt.store().get("account.json").is_some());
        std::fs::remove_file(&auth).unwrap();
        std::fs::create_dir(&auth).unwrap();
        reload(&state).unwrap();
        assert!(state.rt.store().get("account.json").is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn applied_hashes_follow_loaded_bytes_and_revisit_restored_observations() {
        let (state, dir) = state();
        let auth = dir.join("account.json");
        let config_a = format!("port: 9001\nauth-dir: {}\n", dir.display());
        let config_b = format!("port: 9002\nauth-dir: {}\n", dir.display());
        let auth_a = r#"{"type":"claude","access_token":"fake-a"}"#;
        let auth_b = r#"{"type":"claude","access_token":"fake-b"}"#;
        std::fs::write(&state.path, &config_a).unwrap();
        std::fs::write(&auth, auth_a).unwrap();
        let mut cache = HashCache::default();
        let observed = observe(&state, &mut cache);
        std::fs::write(&state.path, &config_b).unwrap();
        std::fs::write(&auth, auth_b).unwrap();
        let (result, read) = reload_snapshot(&state, true, &observed);
        assert_eq!(result.unwrap().unwrap().port, 9002);
        let applied = record(&mut cache, observed.clone(), Some(read));
        assert!(cache.0.is_empty());
        assert_eq!(applied.config, Some(Sha256::digest(config_b.as_bytes()).into()));
        assert_eq!(applied.auth[&auth], <[u8; 32]>::from(Sha256::digest(auth_b.as_bytes())));
        std::fs::write(&state.path, &config_a).unwrap();
        std::fs::write(&auth, auth_a).unwrap();
        let restored = observe(&state, &mut cache);
        assert!(restored != applied, "restored observations must trigger a reload");
        reload_snapshot(&state, true, &restored).0.unwrap();
        assert_eq!(state.rt.config().port, 9001);
        assert_eq!(
            state.rt.store().get("account.json").unwrap().str("access_token"),
            Some("fake-a")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn fallback_reload_hash_uses_the_observed_relative_path() {
        let (state, dir) = state();
        let relative = PathBuf::from(format!(".cpa-fallback-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&relative).unwrap();
        let path = relative.join("account.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::write(&state.path, format!("auth-dir: {}\n", relative.display())).unwrap();
        state
            .fallbacks
            .lock()
            .unwrap()
            .insert(std::path::absolute(&path).unwrap());
        reload(&state).unwrap();
        let observed = observe(&state, &mut HashCache::default());
        let (_, read) = reload_snapshot(&state, true, &observed);
        std::fs::remove_dir_all(&relative).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        assert!(
            read == observed,
            "fallback reads must not add a second absolute hash key"
        );
        assert_eq!(state.rt.store().snapshot().len(), 1);
    }

    #[tokio::test]
    async fn watcher_distinguishes_unreadable_paths_from_their_later_deletion() {
        let (state, dir) = state();
        let path = dir.join("account.json");
        std::fs::write(&path, r#"{"type":"claude","access_token":"fake"}"#).unwrap();
        reload(&state).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let unreadable = observe(&state, &mut HashCache::default());
        let (_, read) = reload_snapshot(&state, true, &unreadable);
        assert!(read == unreadable);
        std::fs::remove_dir(&path).unwrap();
        let deleted = observe(&state, &mut HashCache::default());
        std::fs::remove_dir_all(dir).unwrap();
        assert!(
            unreadable != deleted,
            "deleting a previously unreadable file must trigger a reload"
        );
    }

    #[tokio::test]
    async fn watcher_distinguishes_failed_directory_scans_from_a_missing_directory() {
        let (state, dir) = state();
        let auth = dir.join("auth");
        std::fs::write(&auth, "not a directory").unwrap();
        std::fs::write(&state.path, format!("auth-dir: {}\n", auth.display())).unwrap();
        reload(&state).unwrap();
        let unreadable = observe(&state, &mut HashCache::default());
        let (_, read) = reload_snapshot(&state, true, &unreadable);
        assert!(read == unreadable);
        std::fs::remove_file(&auth).unwrap();
        let deleted = observe(&state, &mut HashCache::default());
        std::fs::remove_dir_all(dir).unwrap();
        assert!(
            unreadable != deleted,
            "removing an unreadable auth directory must trigger a reload"
        );
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>, Arc<tokio::sync::Notify>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            self.1.notify_one();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Capture {
        fn install(&self) -> tracing::subscriber::DefaultGuard {
            let writer = self.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_writer(move || writer.clone())
                .finish();
            tracing::subscriber::set_default(subscriber)
        }

        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }

        async fn wait_for(&self, needle: &str) {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let notified = self.1.notified();
                    if self.text().contains(needle) {
                        break;
                    }
                    notified.await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{needle:?} was not logged:\n{}", self.text()));
        }
    }

    #[tokio::test]
    async fn edit_before_first_stable_observation_is_logged_once() {
        let (state, dir) = state();
        let capture = Capture::default();
        let _guard = capture.install();
        let watcher = start(&state);
        std::fs::write(
            &state.path,
            format!("port: 9001\noauth: {{auth-dir: {}}}\n", dir.display()),
        )
        .unwrap();
        capture.wait_for("port: 0 -> 9001").await;
        watcher.abort();
        let _ = watcher.await;
        assert_eq!(capture.text().matches("port: 0 -> 9001").count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn edit_between_runtime_load_and_start_is_applied() {
        let (state, dir) = state();
        let capture = Capture::default();
        let _guard = capture.install();
        // The runtime holds port 0; the file changes before the watcher hashes it.
        std::fs::write(
            &state.path,
            format!("port: 9001\noauth: {{auth-dir: {}}}\n", dir.display()),
        )
        .unwrap();
        let watcher = start(&state);
        capture.wait_for("config successfully reloaded").await;
        assert_eq!(state.rt.config().port, 9001);
        watcher.abort();
        let _ = watcher.await;
        let text = capture.text();
        assert_eq!(text.matches("port: 0 -> 9001").count(), 1);
        assert_eq!(text.matches("config successfully reloaded").count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn unchanged_config_is_read_quietly_on_first_observation() {
        let (state, dir) = state();
        let capture = Capture::default();
        let _guard = capture.install();
        let initial = state.rt.config();
        let watcher = start(&state);
        // The first reload publishes a new snapshot. An auth file created after that
        // is logged by a later pass of the loop, so every line of the first pass
        // precedes its log line.
        tokio::time::timeout(Duration::from_secs(3), async {
            while Arc::ptr_eq(&state.rt.config(), &initial) {
                tokio::time::sleep(TICK).await;
            }
        })
        .await
        .unwrap();
        std::fs::write(dir.join("a.json"), "{}").unwrap();
        capture.wait_for("auth file changed (CREATE): a.json").await;
        watcher.abort();
        let _ = watcher.await;
        let text = capture.text();
        assert!(!text.contains("config successfully reloaded"), "{text}");
        assert!(!text.contains("config changes detected"), "{text}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A missing auth folder is watched through its parent; once it is created (empty,
    /// so nothing reloads), files written into it are still seen.
    #[tokio::test]
    async fn auth_folder_created_after_start_is_followed() {
        let (state, dir) = state();
        let auth = dir.join("later");
        std::fs::write(&state.path, format!("oauth: {{auth-dir: {}}}\n", auth.display())).unwrap();
        reload(&state).unwrap();
        let capture = Capture::default();
        let _guard = capture.install();
        let initial = state.rt.config();
        let watcher = start(&state);
        tokio::time::timeout(Duration::from_secs(3), async {
            while Arc::ptr_eq(&state.rt.config(), &initial) {
                tokio::time::sleep(TICK).await;
            }
        })
        .await
        .unwrap();
        std::fs::create_dir(&auth).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        std::fs::write(auth.join("a.json"), "{}").unwrap();
        capture.wait_for("auth file changed (CREATE): a.json").await;
        watcher.abort();
        let _ = watcher.await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hash_cache_rereads_only_changed_or_racy_files() {
        let dir = std::env::temp_dir().join(format!("cpa-watch-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.json");
        std::fs::write(&path, b"one").unwrap();
        let old = SystemTime::now() - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let mut cache = HashCache::default();
        let now = SystemTime::now();
        let first = cache.hash(&path, &std::fs::metadata(&path).unwrap(), now).unwrap();
        // Same metadata, old mtime: the cached hash is reused even if the bytes were
        // swapped behind the cache's back (proves no re-read happens).
        let stat = cache.0[&path].0;
        cache.0.insert(path.clone(), (stat, [7; 32]));
        assert_eq!(
            cache.hash(&path, &std::fs::metadata(&path).unwrap(), now),
            Some([7; 32])
        );
        // A real change alters the metadata and is hashed again.
        std::fs::write(&path, b"two!").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let second = cache.hash(&path, &std::fs::metadata(&path).unwrap(), now).unwrap();
        assert_ne!(first, second);
        assert_eq!(second, <Hash>::from(Sha256::digest(b"two!")));
        // A recently modified file is re-read even with identical cached metadata.
        std::fs::write(&path, b"thr!").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        cache.0.insert(path.clone(), (Stat::of(&meta), [7; 32]));
        assert_eq!(
            cache.hash(&path, &meta, SystemTime::now()),
            Some(Sha256::digest(b"thr!").into())
        );
        let _ = std::fs::remove_file(&path);
    }
}
