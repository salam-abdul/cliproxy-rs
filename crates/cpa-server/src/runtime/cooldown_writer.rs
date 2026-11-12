//! Cooldown snapshots written off the request threads. A request only hands over its
//! snapshot; one drain at a time writes them in order, and a snapshot waiting for the
//! drain is replaced by a newer one for the same target, so a burst costs one write.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use crate::cooldown_store::{Backend, Changed, Record, Written};

/// Where a snapshot is saved.
pub(super) enum Target {
    Files(PathBuf),
    Backend(Arc<dyn Backend>),
}

impl Target {
    fn same(&self, other: &Target) -> bool {
        match (self, other) {
            (Target::Files(a), Target::Files(b)) => a == b,
            (Target::Backend(a), Target::Backend(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// The complete live set of cooldown records at one moment.
pub(super) struct Snapshot {
    pub(super) target: Target,
    pub(super) records: Vec<Record>,
    pub(super) now: SystemTime,
    pub(super) changed: Changed,
}

#[derive(Default)]
struct State {
    /// Snapshots waiting for the drain, each with its submission number.
    queue: VecDeque<(u64, Snapshot)>,
    /// The running drain's number, while one runs.
    draining: Option<u64>,
    drains: u64,
    submitted: u64,
    /// Every snapshot numbered up to this one is written, or was dropped by a failed drain.
    saved: u64,
}

#[derive(Default)]
pub(super) struct CooldownWriter {
    state: Mutex<State>,
    saved: Condvar,
    /// The directory the last file save went to and the files it left there; `None`
    /// makes the next file save write every file and list the directory for stale ones.
    written: Mutex<Option<(PathBuf, Written)>>,
}

impl CooldownWriter {
    /// Queues `snapshot` and makes sure a drain will write it: on the blocking pool
    /// inside a Tokio runtime, else on this thread before returning.
    pub(super) fn submit(self: &Arc<Self>, mut snapshot: Snapshot) {
        let drain = {
            let mut state = self.lock();
            state.submitted += 1;
            let number = state.submitted;
            match state.queue.back_mut() {
                Some((waiting, older)) if older.target.same(&snapshot.target) => {
                    let changed = std::mem::replace(&mut older.changed, Changed::All);
                    snapshot.changed = changed.merge(snapshot.changed);
                    *waiting = number;
                    *older = snapshot;
                }
                _ => state.queue.push_back((number, snapshot)),
            }
            if state.draining.is_some() {
                return;
            }
            state.drains += 1;
            state.draining = Some(state.drains);
            Drain {
                writer: self.clone(),
                number: state.drains,
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => drop(handle.spawn_blocking(move || drain.run())),
            Err(_) => drain.run(),
        }
    }

    /// Blocks until every snapshot submitted before this call is written.
    pub(super) fn flush(&self) {
        let mut state = self.lock();
        let target = state.submitted;
        while state.saved < target {
            state = self.saved.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn write(&self, snapshot: Snapshot) {
        let saved = match snapshot.target {
            Target::Backend(backend) => backend.save(snapshot.records, snapshot.now),
            Target::Files(dir) => {
                let mut written = self.written.lock().unwrap_or_else(PoisonError::into_inner);
                let previous = written.take().filter(|(d, _)| *d == dir).map(|(_, w)| w);
                crate::cooldown_store::save_changed(
                    &dir,
                    snapshot.records,
                    snapshot.now,
                    &snapshot.changed,
                    previous.as_ref(),
                )
                .map(|files| *written = Some((dir, files)))
            }
        };
        if let Err(error) = saved {
            tracing::warn!(%error, "failed to persist cooldown state");
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The right to drain the queue. Dropped without finishing (its save panicked, or the
/// runtime discarded it before it ran), it drops the queue and releases flush waiters,
/// so a later submit starts a new drain.
struct Drain {
    writer: Arc<CooldownWriter>,
    number: u64,
}

impl Drain {
    fn run(self) {
        loop {
            let (number, snapshot) = {
                let mut state = self.writer.lock();
                match state.queue.pop_front() {
                    Some(next) => next,
                    None => {
                        state.draining = None;
                        self.writer.saved.notify_all();
                        return;
                    }
                }
            };
            self.writer.write(snapshot);
            self.writer.lock().saved = number;
            self.writer.saved.notify_all();
        }
    }
}

impl Drop for Drain {
    fn drop(&mut self) {
        let mut state = self.writer.lock();
        if state.draining != Some(self.number) {
            return;
        }
        if !state.queue.is_empty() {
            tracing::warn!(dropped = state.queue.len(), "cooldown state saves were dropped");
        }
        state.queue.clear();
        state.draining = None;
        state.saved = state.submitted;
        *self.writer.written.lock().unwrap_or_else(PoisonError::into_inner) = None;
        self.writer.saved.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Records each save's record count; panics on the first `panics` saves.
    #[derive(Default)]
    struct Recorder {
        panics: AtomicUsize,
        saves: Mutex<Vec<usize>>,
        gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    }

    impl Backend for Recorder {
        fn load(&self) -> Result<Vec<Record>, String> {
            Ok(Vec::new())
        }
        fn save(&self, records: Vec<Record>, _: SystemTime) -> Result<(), String> {
            if let Some(gate) = self.gate.lock().unwrap().take() {
                let _ = gate.recv_timeout(std::time::Duration::from_secs(5));
            }
            if self.panics.load(Ordering::SeqCst) > 0 {
                self.panics.fetch_sub(1, Ordering::SeqCst);
                panic!("save failed");
            }
            self.saves.lock().unwrap().push(records.len());
            Ok(())
        }
    }

    fn snapshot(backend: &Arc<Recorder>, records: usize) -> Snapshot {
        Snapshot {
            target: Target::Backend(backend.clone()),
            records: vec![Record::default(); records],
            now: SystemTime::UNIX_EPOCH,
            changed: Changed::All,
        }
    }

    #[test]
    fn a_save_that_panics_does_not_stop_later_saves() {
        let writer = Arc::new(CooldownWriter::default());
        let backend = Arc::new(Recorder::default());
        backend.panics.store(1, Ordering::SeqCst);
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            writer.submit(snapshot(&backend, 1));
        }));
        assert!(failed.is_err());
        writer.submit(snapshot(&backend, 2));
        writer.flush();
        assert_eq!(*backend.saves.lock().unwrap(), [2]);
    }

    /// While a save runs, a newer snapshot replaces only a waiting one for the same
    /// target; one for another target keeps its own place in the queue.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn only_a_waiting_snapshot_for_the_same_target_is_replaced() {
        let writer = Arc::new(CooldownWriter::default());
        let (a, b) = (Arc::new(Recorder::default()), Arc::new(Recorder::default()));
        let (release, gate) = std::sync::mpsc::channel();
        *a.gate.lock().unwrap() = Some(gate);
        writer.submit(snapshot(&a, 1));
        while writer.lock().queue.len() == 1 {
            tokio::task::yield_now().await;
        }
        writer.submit(snapshot(&b, 2));
        writer.submit(snapshot(&a, 3));
        writer.submit(snapshot(&a, 4));
        release.send(()).unwrap();
        writer.flush();
        assert_eq!(*a.saves.lock().unwrap(), [1, 4]);
        assert_eq!(*b.saves.lock().unwrap(), [2]);
    }

    #[test]
    fn merged_changes_keep_every_changed_credential() {
        let only = |ids: &[&str]| Changed::Only(ids.iter().map(|id| id.to_string()).collect());
        assert_eq!(only(&["a"]).merge(only(&["b"])), only(&["a", "b"]));
        assert_eq!(only(&["a"]).merge(Changed::All), Changed::All);
        assert_eq!(Changed::All.merge(only(&["b"])), Changed::All);
    }
}
