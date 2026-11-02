use super::*;

#[derive(Debug, Clone)]
pub enum Outcome {
    Success,
    Failure(ExecError),
    /// A failure that must not change availability (compact request faults, a
    /// missing count_tokens endpoint).
    Neutral(ExecError),
    /// The client went away before the response finished.
    Cancelled,
}

/// One attempt with one credential. Not cloneable: it reports exactly one outcome.
pub struct Lease {
    pub(super) store: Arc<CredentialStore>,
    pub credential: Arc<Credential>,
    pub selection: Selection,
    /// The model key the outcome is recorded under (Go `stateModelForExecution`).
    pub execution_model: String,
    pub attempt: u64,
    pub(super) policy: Arc<Policy>,
    pub(super) reported: bool,
    /// The scheduler generation at the pick (`Scheduler::reserve_probe`): bounded
    /// windows opened or probes reserved after it are not answered by this attempt.
    pub(super) picked: u32,
    /// A credential from the remote dispatcher: its lease ends there, not in the local
    /// scheduler.
    pub(super) remote: Option<crate::remote::RemoteEnd>,
    /// The LCP binding of the pick: the attempt's canonical session and lineage.
    pub lcp: Option<crate::lcp::Match>,
}

impl Lease {
    /// Whether the credential came from the remote dispatcher (Go Home mode).
    pub fn is_remote(&self) -> bool {
        self.remote.is_some()
    }

    /// Resolves when the dispatcher cancels this lease's execution.
    pub(crate) fn remote_cancelled(&self) -> Option<futures_util::future::BoxFuture<'static, ()>> {
        let signal = self.remote.as_ref()?.cancel.clone()?;
        Some(crate::remote::cancelled(signal))
    }

    /// Whether the dispatcher cancelled this lease's execution (Go: its selection is
    /// no longer `Active`).
    pub(crate) fn remote_cancel_requested(&self) -> bool {
        self.remote
            .as_ref()
            .and_then(|remote| remote.cancel.as_ref())
            .is_some_and(|signal| *signal.borrow())
    }

    pub fn complete(mut self, outcome: Outcome) {
        self.report(outcome);
    }

    /// Joins a remote lease's release to `releases`: a Home pick a session kept ends or
    /// runs again during a later request, which awaits the release before its next pick.
    pub(crate) fn release_into(&mut self, releases: &crate::remote::PendingReleases) {
        if let Some(remote) = &mut self.remote {
            remote.releases = releases.clone();
        }
    }

    /// The upstream accepted this attempt (a stream's first chunk arrived): an ended
    /// bounded window it probes has its answer, so other requests may use the account
    /// while the stream runs (`Scheduler::accept_probe`). A no-op for remote leases.
    pub fn accepted(&self) {
        if self.remote.is_none() {
            self.store.accept_probe(self);
        }
    }

    /// Records an intermediate outcome for one model of a pooled alias without ending
    /// the lease.
    pub fn note(&self, model: &str, outcome: &Outcome) {
        if self.remote.is_none() {
            self.store.record_model(self, model, outcome);
        }
    }

    fn report(&mut self, outcome: Outcome) {
        if std::mem::replace(&mut self.reported, true) {
            return;
        }
        match &mut self.remote {
            // Go `reportHomeResult` leaves local cooldowns alone; the scope ends with
            // its release.
            Some(remote) => remote.finish(),
            None => self.store.record(self, &outcome),
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.report(Outcome::Cancelled);
    }
}

impl std::fmt::Debug for Lease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lease")
            .field("credential", &self.credential.id)
            .field("attempt", &self.attempt)
            .finish()
    }
}

impl CredentialStore {
    fn accept_probe(&self, lease: &Lease) {
        let inner = self.read();
        // Like results: a credential edited or re-created since the pick is left alone.
        if !inner
            .creds
            .iter()
            .any(|c| c.id == lease.credential.id && c.revision == lease.credential.revision)
        {
            return;
        }
        self.scheduler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .accept_probe_picked(&lease.credential, &lease.execution_model, lease.picked, Instant::now());
    }

    fn record(&self, lease: &Lease, outcome: &Outcome) {
        self.record_model(lease, &lease.execution_model, outcome);
    }

    fn record_model(&self, lease: &Lease, model: &str, outcome: &Outcome) {
        let slot = match outcome {
            Outcome::Success => 0,
            Outcome::Failure(_) | Outcome::Neutral(_) => 1,
            Outcome::Cancelled => 2,
        };
        self.stats[slot].fetch_add(1, Ordering::Relaxed);
        if !matches!(outcome, Outcome::Cancelled) {
            self.note_activity(&lease.credential.id, matches!(outcome, Outcome::Success));
        }
        let inner = self.read();
        // Outcomes from credentials deleted/re-created during an attempt must not
        // poison the replacement. Metadata edits likewise invalidate stale results.
        if inner
            .creds
            .iter()
            .any(|c| c.id == lease.credential.id && c.revision == lease.credential.revision)
        {
            let (now, wall) = (Instant::now(), std::time::SystemTime::now());
            let persist = self
                .cooldown_dir
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .is_some();
            let mut scheduler = self.scheduler.lock().unwrap_or_else(PoisonError::into_inner);
            let before = persist.then(|| scheduler.records(&lease.credential, now, wall));
            let profile = inner.profile(&lease.credential);
            let rules = profile.rules.as_deref();
            scheduler.record_with(
                &lease.credential,
                rules,
                model,
                outcome,
                &lease.policy,
                lease.picked,
                now,
            );
            scheduler.session_result_with(
                &lease.credential,
                rules,
                &lease.selection,
                outcome,
                &lease.policy,
                lease.lcp.as_ref(),
                now,
            );
            // Go `publishErrorEvent` after MarkResult (and the availability-neutral record).
            if let (Outcome::Failure(error) | Outcome::Neutral(error), Some(queue)) = (outcome, self.error_events.get())
                && queue.wants_errors()
            {
                let records = scheduler.records(&lease.credential, now, wall);
                queue.enqueue_error(&crate::error_events::payload(&lease.credential, model, error, &records));
            }
            // Go MarkResult persists only when this credential's cooldown records changed.
            let changed = before.is_some_and(|before| before != scheduler.records(&lease.credential, now, wall));
            drop(scheduler);
            drop(inner);
            if changed {
                self.persist_cooldowns();
            }
            // Go persists on every recorded result, a client-cancelled stream included.
            self.persist_credential(&lease.credential);
        }
    }
}

/// A response stream that reports its lease's outcome on end, on the first error (after
/// which it yields nothing more), or as `Cancelled` when dropped early.
///
/// A remote lease's cancellation (Go's selection-bound attempt context) is watched by
/// its own task: a draining dispatcher closes the upstream body and ends the lease even
/// while the client is not reading, and the reader then sees `context canceled`.
pub struct Completing {
    slot: Arc<Mutex<Slot>>,
    watcher: Option<tokio::task::JoinHandle<()>>,
}

struct Slot {
    inner: ExecStream,
    lease: Option<Lease>,
    /// The watcher ended the lease; the next poll yields the cancellation error.
    cancelled: bool,
    waker: Option<std::task::Waker>,
}

impl Completing {
    pub fn new(inner: ExecStream, lease: Lease) -> Self {
        let cancelled = lease.remote_cancelled();
        let slot = Arc::new(Mutex::new(Slot {
            inner,
            lease: Some(lease),
            cancelled: false,
            waker: None,
        }));
        let watcher = cancelled.map(|cancelled| {
            let slot = Arc::downgrade(&slot);
            tokio::spawn(async move {
                cancelled.await;
                let Some(slot) = slot.upgrade() else { return };
                let (inner, lease, waker) = {
                    let mut s = slot.lock().unwrap_or_else(PoisonError::into_inner);
                    let Some(lease) = s.lease.take() else { return };
                    s.cancelled = true;
                    let inner = std::mem::replace(&mut s.inner, Box::pin(futures_util::stream::empty()));
                    (inner, lease, s.waker.take())
                };
                // The upstream body closes first, then the lease ends.
                drop(inner);
                lease.complete(Outcome::Cancelled);
                if let Some(waker) = waker {
                    waker.wake();
                }
            })
        });
        Self { slot, watcher }
    }
}

impl Drop for Completing {
    fn drop(&mut self) {
        if let Some(watcher) = &self.watcher {
            watcher.abort();
        }
    }
}

impl Stream for Completing {
    type Item = Result<Bytes, ExecError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut s = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        if std::mem::take(&mut s.cancelled) {
            return Poll::Ready(Some(Err(crate::remote::cancelled_error())));
        }
        if s.lease.is_none() {
            return Poll::Ready(None);
        }
        if self.watcher.is_some() {
            s.waker = Some(cx.waker().clone());
        }
        let item = ready!(s.inner.poll_next_unpin(cx));
        match &item {
            Some(Ok(_)) => {}
            None => s.lease.take().unwrap().complete(Outcome::Success),
            Some(Err(e)) => {
                // The upstream body closes before the lease ends: a remote lease's release
                // must not reach the control plane while the response is still open.
                s.inner = Box::pin(futures_util::stream::empty());
                s.lease.take().unwrap().complete(Outcome::Failure(e.clone()));
            }
        }
        Poll::Ready(item)
    }
}
