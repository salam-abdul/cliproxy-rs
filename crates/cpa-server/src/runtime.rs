//! Axum-independent runtime: config snapshot, credential store, selection, credential
//! preparation and attempt outcomes.
//!
//! Every attempt is a [`Lease`]: an owned guard that reports exactly one [`Outcome`],
//! either explicitly or as `Cancelled` when dropped (client gone mid-execute, stream
//! dropped). Streams wrap their lease in [`Completing`].

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};

use bytes::Bytes;
use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch, Source};
use cpa_core::exec::{ExecError, ExecStream, FailureScope};
use cpa_exec::{Executors, Readiness};
use futures_util::{Stream, StreamExt};
use serde_json::{Map, Value};

use crate::refresh::RefreshState;
use crate::registry::Registry;
use crate::scheduler::{Candidate, Policy, Profile, Scheduler};

mod credential_updates;
mod lease;
mod selection;
mod store;

use credential_updates::Version;
pub use lease::{Completing, Lease, Outcome};
pub use selection::{AcquireError, Admit, Selection, admission, standalone_admission};
pub(crate) use store::write_bytes_atomic;
pub use store::{AttemptStats, CredentialActivity, CredentialStore, PatchError, RECENT_BUCKET_SECONDS};
use store::{Inner, write_atomic};

/// The config snapshot and credential epoch a registry was built from.
type RegistryCache = (Arc<Config>, (u64, u64, u64), Arc<Registry>);

pub struct Runtime {
    /// The published config and the policy derived from it, replaced together.
    published: RwLock<Published>,
    store: Arc<CredentialStore>,
    pub executors: Executors,
    refresh_task: Mutex<Option<tokio::task::AbortHandle>>,
    refresh_state: Mutex<RefreshState>,
    /// The registry derived from the current config and credential generation.
    registry: Mutex<Option<RegistryCache>>,
    oauth_sink: RwLock<Option<OAuthCallbackSink>>,
    /// Go `modelPoolOffsets`: rotation cursors for OpenAI-compatible alias pools.
    pool_offsets: Mutex<HashMap<String, usize>>,
    /// Usage records for `GET /observability/usage/queue` (management configures it).
    usage: Arc<crate::usage::UsageQueue>,
    pub(crate) claude_usage: crate::claude_usage::ClaudeUsage,
    pub(crate) native_state: RwLock<std::sync::Weak<crate::native_claude::State>>,
    pub(crate) native_events: crate::native_claude::diagnostics::Diagnostics,
    /// `--local-model`: embedded model catalogs only, no remote catalog refresh.
    local_model: std::sync::atomic::AtomicBool,
    /// The remote dispatcher that replaces local selection (Go Home mode).
    remote: RwLock<Option<Arc<dyn crate::remote::RemoteDispatch>>>,
    /// Set only by [`crate::testing::runtime`]: executor calls get credentials that
    /// cannot leave the machine.
    pub(crate) deny_external: std::sync::atomic::AtomicBool,
    /// Marked on every config publish, so background loops sleep until it changes.
    config_changed: tokio::sync::watch::Sender<()>,
    /// Quota windows by credential, parsed once per observation for selection.
    quota_windows: Mutex<HashMap<String, crate::scheduler::Observation>>,
    history: crate::request_logging::history::writer::Writer,
}

/// One config publication: requests read the config and its policy from the same one.
#[derive(Clone)]
struct Published {
    config: Arc<Config>,
    policy: Arc<Policy>,
}

/// An OAuth provider redirect received on the main listener.
#[derive(Debug, Clone)]
pub struct OAuthCallback {
    /// `anthropic`, `codex`, `antigravity` or `devin`.
    pub provider: &'static str,
    pub state: String,
    pub code: String,
    pub error: String,
}

/// Hands a callback to a pending management login (Go
/// `WriteOAuthCallbackFileForPendingSession`); returns false when no login with that
/// state is pending or the callback could not be recorded.
pub type OAuthCallbackSink = Arc<dyn Fn(&OAuthCallback) -> bool + Send + Sync>;

impl Runtime {
    /// The scheduler policy is derived from the config (`routing` plus the OAuth
    /// provider rules), as on every publish.
    pub fn new(config: Config, credentials: Vec<Credential>, executors: Executors) -> Self {
        let policy = Policy::from_config(&config);
        let cooldown_dir = cooldown_dir(&config, &policy);
        let (enabled, strict) = signature_cache_config(&config);
        cpa_translate::set_antigravity_signature_cache_config(enabled, strict);
        let config = Arc::new(config);
        let rt = Self {
            published: RwLock::new(Published {
                config: config.clone(),
                policy: Arc::new(Policy::default()),
            }),
            store: CredentialStore::new(credentials),
            executors,
            refresh_task: Mutex::default(),
            refresh_state: Mutex::default(),
            registry: Mutex::default(),
            oauth_sink: RwLock::default(),
            pool_offsets: Mutex::default(),
            usage: Arc::default(),
            native_events: Default::default(),
            native_state: Default::default(),
            claude_usage: Default::default(),
            local_model: Default::default(),
            remote: RwLock::default(),
            deny_external: Default::default(),
            config_changed: tokio::sync::watch::Sender::new(()),
            quota_windows: Mutex::default(),
            history: Default::default(),
        };
        rt.publish(config, policy);
        rt.store.configure_cooldown_store(cooldown_dir);
        // Failed attempts publish Go's error events on the RESP `errors` channel.
        let _ = rt.store.error_events.set(rt.usage.clone());
        rt
    }

    /// The usage queue: the request path calls `enqueue` with one serialized Go
    /// `queuedUsageDetail` per upstream request when `accepts()` (additive API).
    pub fn usage_queue(&self) -> &crate::usage::UsageQueue {
        &self.usage
    }

    /// The model-traffic history writer (`observability.history`).
    pub fn history(&self) -> &crate::request_logging::history::writer::Writer {
        &self.history
    }

    /// Records `--local-model` (Go `modelCatalogUpdaterPlan`'s `localModel`): remote
    /// model catalog updaters must not start when set (additive API).
    pub fn set_local_model(&self, on: bool) {
        self.local_model.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Persists cooldowns to `backend` instead of `.cds` files (PGSTORE's cooldown
    /// table; additive API). Call before serving.
    pub fn set_cooldown_backend(&self, backend: Arc<dyn crate::cooldown_store::Backend>) {
        self.store.set_cooldown_backend(backend);
    }

    /// Whether `--local-model` was given.
    pub fn local_model(&self) -> bool {
        self.local_model.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The config snapshot to use for one whole request.
    pub fn config(&self) -> Arc<Config> {
        self.published().config
    }

    fn published(&self) -> Published {
        self.published.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Resolves after the next config publish.
    pub fn subscribe_config(&self) -> tokio::sync::watch::Receiver<()> {
        self.config_changed.subscribe()
    }

    /// Replaces the config and the scheduler policy derived from it in one snapshot.
    /// Requests already running keep theirs. Every config publisher goes through here.
    pub fn publish_config(&self, config: Config) {
        let policy = Policy::from_config(&config);
        let (enabled, strict) = signature_cache_config(&config);
        cpa_translate::set_antigravity_signature_cache_config(enabled, strict);
        let dir = cooldown_dir(&config, &policy);
        let ws_auth = crate::relay::ws_auth(&config);
        let previous = self.publish(Arc::new(config), policy);
        // Go's websocket auth change handler: turning ws-auth on ends the relay sessions
        // that connected without a key.
        if ws_auth && !crate::relay::ws_auth(&previous) {
            self.executors.google.aistudio.relay.stop();
        }
        self.store.configure_cooldown_store(dir);
        self.config_changed.send_replace(());
    }

    /// A coherent config/policy pair for the entire route attempt loop.
    pub fn request_snapshot(&self) -> (Arc<Config>, Arc<Policy>) {
        let Published { config, policy } = self.published();
        (config, policy)
    }

    pub fn store(&self) -> &Arc<CredentialStore> {
        &self.store
    }

    /// Go `nextModelPoolOffset`: the start index for the next pass over a pool.
    pub fn next_pool_offset(&self, key: &str, size: usize) -> usize {
        if size <= 1 || key.trim().is_empty() {
            return 0;
        }
        let key = key.trim();
        let mut offsets = self.pool_offsets.lock().unwrap_or_else(PoisonError::into_inner);
        // ponytail: Go never prunes, so keys of removed credentials would accumulate
        // forever. Past 4096 keys the map is cleared, which restarts rotation for every
        // pool from offset 0. Upgrade path: drop a pool's key when its credential set
        // changes in reconcile.
        if offsets.len() >= 4096 && !offsets.contains_key(key) {
            offsets.clear();
        }
        let slot = offsets.entry(key.to_owned()).or_default();
        let offset = if *slot >= 2_147_483_640 { 0 } else { *slot };
        *slot = offset + 1;
        offset % size
    }

    /// One credential's projected state for one registered model (Go
    /// `clientModelProjectionForAuth`), for listings and `auto`.
    pub fn suspension(&self, credential_id: &str, model: &str) -> crate::registry::Suspension {
        use crate::registry::Suspension;
        let Some(credential) = self.store.get(credential_id) else {
            return Suspension::None;
        };
        let aliases = crate::registry::global_aliases(&self.config());
        let key = crate::registry::selection_model(&aliases, &credential, model);
        self.store.suspension(&credential, &key)
    }

    /// Wires OAuth callback delivery (management owns pending login sessions).
    pub fn set_oauth_callback_sink(&self, sink: Option<OAuthCallbackSink>) {
        *self.oauth_sink.write().unwrap_or_else(PoisonError::into_inner) = sink;
    }

    pub fn deliver_oauth_callback(&self, callback: &OAuthCallback) -> bool {
        let sink = self.oauth_sink.read().unwrap_or_else(PoisonError::into_inner).clone();
        sink.is_some_and(|sink| sink(callback))
    }

    /// The credential an executor call receives: the stored one, or in a test runtime
    /// ([`crate::testing`]) a copy that cannot leave the machine.
    pub(crate) fn for_executor(&self, credential: &Arc<Credential>) -> Arc<Credential> {
        if self.deny_external.load(std::sync::atomic::Ordering::Relaxed) {
            crate::testing::guarded(credential, &self.config())
        } else {
            credential.clone()
        }
    }

    /// The model registry for the current config and credential set. Rebuilt only when
    /// either changed, so it is always derived, never separately maintained.
    pub fn registry(&self) -> Arc<Registry> {
        let config = self.config();
        // A refreshed static catalog re-registers every credential (Go's model refresh
        // callback), so the catalog generation is part of the key.
        // Capability probes (Antigravity web search) refine registered models later.
        let generation = (
            self.store.epoch(),
            cpa_core::registry::catalog_generation(),
            cpa_core::registry::dynamic::capabilities_generation(),
        );
        let mut cache = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((cfg, g, registry)) = cache.as_ref()
            && Arc::ptr_eq(cfg, &config)
            && *g == generation
        {
            return registry.clone();
        }
        let snapshot = self.store.snapshot();
        let registry = Arc::new(Registry::build(&config, &snapshot));
        let credentials: Vec<_> = snapshot.iter().map(|c| self.for_executor(c)).collect();
        self.executors.models_registered(&credentials, &config);
        *cache = Some((config, generation, registry.clone()));
        registry
    }

    pub fn policy(&self) -> Arc<Policy> {
        self.published().policy
    }

    /// Replaces the scheduler policy and keeps the config.
    pub fn publish_policy(&self, policy: Policy) {
        self.publish(self.config(), policy);
    }

    /// Publishes `config` with `policy` as one snapshot; returns the previous config.
    fn publish(&self, config: Arc<Config>, policy: Policy) -> Arc<Config> {
        let policy = Arc::new(policy);
        let previous = {
            let mut current = self.published.write().unwrap_or_else(PoisonError::into_inner);
            self.store.set_policy(policy.clone());
            std::mem::replace(&mut *current, Published { config, policy }).config
        };
        self.store.clear_disabled_cooldowns();
        previous
    }

    /// Selects a credential that registered the route model. Waits for preparation only
    /// when the executor reports [`Readiness::PrepareNow`] (single-flighted per
    /// credential); token refreshes stay with the background loop. The returned lease
    /// holds the committed snapshot.
    pub async fn acquire(
        &self,
        selection: Selection,
        cfg: &Config,
        policy: Arc<Policy>,
        registry: &Registry,
    ) -> Result<Lease, AcquireError> {
        let aliases = crate::registry::global_aliases(cfg);
        let scope = selection.clone();
        let admit = admission(registry, &aliases, &scope, &self.executors);
        let mut lease = {
            // `soonest-reset` ranks by the quota windows the executors last observed,
            // parsed once per observation.
            let mut parsed = self.quota_windows.lock().unwrap_or_else(PoisonError::into_inner);
            if parsed.len() >= 4096 {
                parsed.clear();
            }
            let parsed = std::cell::RefCell::new(&mut *parsed);
            let now = std::time::SystemTime::now();
            let cap = policy.max_trusted_cooldown;
            let ranks = |c: &Credential| {
                let observation = |provider: &str, s: &cpa_exec::codex_quota::Snapshot| {
                    let mut parsed = parsed.borrow_mut();
                    match parsed.get(&c.id) {
                        Some(o) if o.observed_at() == s.observed_at => *o,
                        _ => {
                            let o = crate::scheduler::Observation::parse(provider, &s.signals, s.observed_at);
                            parsed.insert(c.id.clone(), o);
                            o
                        }
                    }
                };
                let provider = c.provider.trim();
                let observed = if provider.eq_ignore_ascii_case("claude") {
                    self.executors
                        .claude
                        .quota()
                        .with_snapshot(&c.id, |s| observation("claude", s))
                } else if provider.eq_ignore_ascii_case("codex") {
                    self.executors
                        .codex
                        .quota()
                        .with_snapshot(&c.id, |s| observation("codex", s))
                } else {
                    None
                };
                observed.map_or_else(crate::scheduler::Windows::default, |o| {
                    let mut windows = o.at(now);
                    // `max-trusted-cooldown` bounds trust in an observed exhausted window as
                    // in a cooldown: past the bound, the next request may probe the account.
                    if !cap.is_zero() && o.observed_at().checked_add(cap).is_some_and(|end| end <= now) {
                        windows.exhausted = false;
                    }
                    windows
                })
            };
            self.store.select_ranked(selection, policy, &admit, &ranks)?
        };
        if self.executors.readiness(&lease.credential, cfg) != Readiness::PrepareNow {
            return Ok(lease);
        }
        let id = lease.credential.id.clone();
        match self.prepare_credential(&id, cfg, None).await {
            Ok(current) => lease.credential = current,
            Err(error) => {
                lease.complete(Outcome::Failure(error.clone()));
                return Err(AcquireError::Prepare { id, error });
            }
        }
        Ok(lease)
    }

    /// A lease on credential `id` outside selection, prepared like [`Self::acquire`]
    /// (Go's credits fallback, `findAllAntigravityCreditsCandidateAuths` then
    /// `prepareRequestAuth`). `None` when the credential is gone or preparation fails;
    /// Go records no result for either.
    pub(crate) async fn lease_credential(
        &self,
        id: &str,
        selection: Selection,
        cfg: &Config,
        prepare: impl FnOnce(&Credential) -> Credential,
    ) -> Option<Lease> {
        let mut credential = self.store.get(id)?;
        if self.executors.readiness(&credential, cfg) == Readiness::PrepareNow {
            credential = self.prepare_credential(id, cfg, None).await.ok()?;
        }
        // Outside selection: the lease answers no bounded window or probe reservation.
        let picked = self
            .store
            .scheduler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .outside_selection_generation();
        Some(Lease {
            store: self.store.clone(),
            credential: Arc::new(prepare(&credential)),
            execution_model: selection.model.clone(),
            selection,
            attempt: self.store.attempts.fetch_add(1, Ordering::Relaxed),
            policy: self.policy(),
            reported: false,
            picked,
            remote: None,
            lcp: None,
        })
    }

    /// The installed remote dispatcher (Go Home mode), if any.
    pub fn remote_dispatch(&self) -> Option<Arc<dyn crate::remote::RemoteDispatch>> {
        self.remote.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Routes every request through `dispatch` (Go Home mode) or back to the local
    /// scheduler with `None` (additive API).
    pub fn set_remote_dispatch(&self, dispatch: Option<Arc<dyn crate::remote::RemoteDispatch>>) {
        *self.remote.write().unwrap_or_else(PoisonError::into_inner) = dispatch;
    }

    /// One remote pick (Go `pickHomeDispatchSelection`): the lease ends through the
    /// dispatcher, and its release joins `releases`. Also returns Home's request-retry
    /// limit and the client key Home authenticated (empty when it sent none).
    pub(crate) async fn acquire_remote(
        &self,
        dispatch: &dyn crate::remote::RemoteDispatch,
        selection: Selection,
        request: crate::remote::RemoteRequest,
        releases: &crate::remote::PendingReleases,
    ) -> Result<(Lease, Option<i64>, String), crate::remote::RemoteError> {
        let grant = dispatch.dispatch(request).await?;
        let lease = Lease {
            store: self.store.clone(),
            credential: Arc::new(grant.credential),
            execution_model: selection.model.clone(),
            selection,
            attempt: self.store.attempts.fetch_add(1, Ordering::Relaxed),
            policy: self.policy(),
            reported: false,
            picked: 0,
            remote: Some(crate::remote::RemoteEnd {
                end: Some(grant.end),
                releases: releases.clone(),
                cancel: grant.cancel,
            }),
            // ponytail: Go's Home dispatch keys its own session aliases on LCP matches
            // (home_session_alias.go); remote picks here carry no LCP binding. Bind them in
            // `lcp` if Home sessions need those aliases.
            lcp: None,
        };
        Ok((lease, grant.request_retry, grant.user_api_key))
    }

    /// Go `prepareHomeRequestAuth`: a dispatched credential that requests must wait for
    /// (a Claude identity, a Meta mint) is prepared on this attempt's copy only,
    /// serialized per credential ID. Home owns the stored credential, so nothing is
    /// committed locally.
    pub(crate) async fn prepare_remote(&self, lease: &mut Lease, cfg: &Config) -> Result<(), ExecError> {
        if self.executors.readiness(&lease.credential, cfg) != Readiness::PrepareNow {
            return Ok(());
        }
        let lock = self.store.prepare_lock(&lease.credential.id);
        let _guard = lock.lock().await;
        let patch = self
            .executors
            .prepare(&self.for_executor(&lease.credential), cfg)
            .await?;
        let mut prepared = (*lease.credential).clone();
        patch.apply(&mut prepared.metadata);
        lease.credential = Arc::new(prepared);
        Ok(())
    }

    /// Prepares and commits one credential. `failed` is the revision whose token an
    /// upstream rejected: preparation then runs even when not due, unless another task
    /// already replaced that revision (Go `refreshAuthForRequest`).
    async fn prepare_credential(
        &self,
        id: &str,
        cfg: &Config,
        failed: Option<u64>,
    ) -> Result<Arc<Credential>, ExecError> {
        let lock = self.store.prepare_lock(id);
        let guard = lock.lock_owned().await;
        // Another request may have prepared it while we waited.
        let (current, version) = self
            .store
            .get_versioned(id)
            .filter(|(c, _)| !c.disabled)
            .ok_or_else(|| {
                ExecError::local(
                    409,
                    FailureScope::Request,
                    "credential removed or disabled during preparation",
                )
            })?;
        match failed {
            Some(revision) if current.revision != revision => return Ok(current),
            Some(_) => {}
            None if self.executors.readiness(&current, cfg) == Readiness::Ready => return Ok(current),
            None => {}
        }
        let patch = self.executors.prepare(&self.for_executor(&current), cfg).await?;
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            store.apply_refresh(&current, version, &patch)
        })
        .await
        .map_err(|e| PatchError::Io(e.to_string()))
        .and_then(|r| r)
        .map_err(|e| {
            ExecError::local(
                500,
                FailureScope::Request,
                format!("committing prepared credential: {e:?}"),
            )
        })
    }

    /// Go `tryRefreshAfterUnauthorized`: after an upstream 401 on a credential whose
    /// executor reports a refresh credential, prepare it once (due or not) and return the
    /// committed replacement to retry with.
    // ponytail: refresh goes through `Executors::prepare`; a provider whose prepare only
    // refreshes inside its lead (Claude) cannot recover a revoked but unexpired token.
    // Force a refresh past the lead here to recover it.
    pub async fn refresh_after_unauthorized(&self, credential: &Credential, cfg: &Config) -> Option<Arc<Credential>> {
        if !self.executors.has_refresh_credential(credential) {
            return None;
        }
        let refreshed = self
            .prepare_credential(&credential.id, cfg, Some(credential.revision))
            .await
            .ok()?;
        (refreshed.revision != credential.revision).then_some(refreshed)
    }

    /// One replaceable refresh loop. Uses the same preparation lock and atomic
    /// revision-checked commit as request acquisition; executors never persist files.
    /// ponytail: the executor's `needs_prepare_at` alone decides eligibility, and a
    /// failed refresh only backs off (`refresh.rs`: `invalid_grant` doubles up to 30
    /// minutes), so a revoked refresh token is retried at that cap indefinitely. Record a
    /// terminal refresh state on the credential if those retries must stop.
    pub fn start_auto_refresh(self: &Arc<Self>) {
        let mut task = self.refresh_task.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(previous) = task.take() {
            previous.abort();
        }
        // Wakes at the next deadline, or at the latest after `HORIZON` of awake time while
        // a credential can ever come due, to catch wall-clock jumps such as a resumed
        // laptop (monotonic time stops during suspend). Credentials that never need
        // preparation (API keys, Gemini CLI tokens) set no timer.
        const HORIZON: Duration = Duration::from_secs(600);
        // Store changes (each committed refresh is one) are coalesced over this delay.
        const COALESCE: Duration = Duration::from_secs(1);
        let weak = Arc::downgrade(self);
        let mut store_changes = self.store.subscribe();
        let mut config_changes = self.subscribe_config();
        let handle = tokio::spawn(async move {
            loop {
                let Some(rt) = weak.upgrade() else {
                    break;
                };
                // Everything published so far is in this scan, including the commits of
                // the previous batch.
                store_changes.borrow_and_update();
                config_changes.borrow_and_update();
                let cfg = rt.config();
                let snapshot = rt.store.snapshot();
                let (now, wall) = (Instant::now(), chrono::Utc::now());
                let horizon = chrono::Duration::from_std(HORIZON).unwrap_or_default();
                let mut wake: Option<Instant> = None;
                let mut soonest = |at: Instant| wake = Some(wake.map_or(at, |w| w.min(at)));
                let jobs: Vec<_> = {
                    let mut state = rt.refresh_state.lock().unwrap_or_else(PoisonError::into_inner);
                    state.reconcile(&snapshot);
                    let mut jobs = Vec::new();
                    // Far enough ahead that any expiry has passed, near enough for chrono.
                    let eventually = wall + chrono::Duration::days(36_500);
                    for c in snapshot.iter().filter(|c| refresh_candidate(c)) {
                        if rt.executors.needs_prepare_at(c, &cfg, eventually) {
                            soonest(now + HORIZON);
                        }
                        match rt.executors.prepare_due(c, &cfg, wall, horizon) {
                            Some(at) if at > wall => soonest(now + (at - wall).to_std().unwrap_or_default()),
                            Some(_) if state.reserve(c, now) => jobs.push(c.clone()),
                            // Reserved or backing off: look again when that ends.
                            Some(_) => {
                                if let Some(at) = state.retry_at(c).filter(|at| *at > now) {
                                    soonest(at);
                                }
                            }
                            None => {}
                        }
                    }
                    jobs
                };
                let busy = !jobs.is_empty();
                // ponytail: bounded batches, so a slow worker delays the next scan.
                // Use an independent queue if refresh latency matters for large pools.
                futures_util::stream::iter(jobs)
                    .for_each_concurrent(refresh_workers(&cfg), |credential| {
                        let rt = rt.clone();
                        let cfg = cfg.clone();
                        async move {
                            let result = rt.prepare_credential(&credential.id, &cfg, None).await;
                            let Some(current) = rt.store.get(&credential.id) else {
                                return;
                            };
                            // A concurrent removal/re-import or management edit must not
                            // transfer the old refresh failure/backoff to the replacement.
                            if result.is_err() && current.revision != credential.revision {
                                return;
                            }
                            if result
                                .as_ref()
                                .is_ok_and(|prepared| prepared.revision != current.revision)
                            {
                                return;
                            }
                            let ineffective =
                                result.is_ok() && rt.executors.readiness(&current, &cfg) != Readiness::Ready;
                            let backoff = rt.refresh_state.lock().unwrap_or_else(PoisonError::into_inner).finish(
                                &current,
                                result.as_ref().err(),
                                ineffective,
                                Instant::now(),
                            );
                            // The error body can echo request fields, so only its classification is logged.
                            if let Err(error) = &result {
                                tracing::warn!(
                                    "credential refresh failed: {} ({}) status={} scope={:?} invalid_grant={} retry_in={}s",
                                    current.id,
                                    current.provider,
                                    error.status,
                                    error.scope,
                                    crate::refresh::invalid_grant(error),
                                    backoff.as_secs(),
                                );
                            }
                        }
                    })
                    .await;
                drop(rt);
                // After a batch, look again at once: finished jobs set their backoff,
                // and the next scan turns it into a deadline.
                if busy {
                    continue;
                }
                let deadline = async {
                    match wake {
                        Some(at) => tokio::time::sleep_until(at.into()).await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    () = deadline => {}
                    changed = store_changes.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        tokio::time::sleep(COALESCE).await;
                    }
                    changed = config_changes.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        tokio::time::sleep(COALESCE).await;
                    }
                }
            }
        });
        *task = Some(handle.abort_handle());
    }

    pub fn stop_auto_refresh(&self) {
        if let Some(task) = self.refresh_task.lock().unwrap_or_else(PoisonError::into_inner).take() {
            task.abort();
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Some(task) = self
            .refresh_task
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }
}

/// Where cooldowns persist: `auth-dir` while `save-cooldown-status` is on.
fn cooldown_dir(cfg: &Config, policy: &Policy) -> Option<std::path::PathBuf> {
    policy.save_cooldown_status.then(|| cfg.auth_dir.clone())
}

/// `oauth.auth-auto-refresh-workers`: non-positive means Go's default of 16.
fn refresh_workers(cfg: &Config) -> usize {
    cfg.document
        .get("oauth")
        .and_then(|o| o.get("auth-auto-refresh-workers"))
        .and_then(serde_yaml_ng::Value::as_i64)
        .filter(|n| *n > 0)
        .map_or(16, |n| n as usize)
}

impl Runtime {
    /// Runs the executor's single-flighted preparation now, due or not (management
    /// refresh, Go `RefreshAuthFile`). Returns the committed credential.
    pub async fn refresh_credential(&self, id: &str) -> Result<Arc<Credential>, ExecError> {
        let cfg = self.config();
        let revision = self.store.get(id).map(|c| c.revision);
        self.prepare_credential(id, &cfg, revision).await
    }
}

/// Go `configuredSignatureCacheEnabled` / `configuredSignatureBypassStrict`
/// (internal/api/server_reload.go): `oauth.providers.antigravity.signature-cache-enabled`
/// (default true) and `.signature-bypass-strict` (default false), legacy
/// `antigravity-signature-*` spellings included. Applied on every publish, as Go
/// applies them at startup and on each reload.
fn signature_cache_config(cfg: &Config) -> (bool, bool) {
    let flag = |key: &str| {
        cfg.document
            .get("oauth")
            .and_then(|o| o.get("providers"))
            .and_then(|p| p.get("antigravity"))
            .and_then(|a| a.get(key))
            .and_then(serde_yaml_ng::Value::as_bool)
    };
    (
        flag("signature-cache-enabled").unwrap_or(true),
        flag("signature-bypass-strict").unwrap_or(false),
    )
}

/// Credentials the background refresh considers: enabled auth files, never API-key
/// kinds (Go `nextRefreshCheckAt` skips `AuthKind() == apikey` for every provider).
fn refresh_candidate(c: &Credential) -> bool {
    matches!(c.source, Source::File(_)) && !c.disabled && cpa_core::registry::dynamic::auth_kind(c) != Some("apikey")
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
