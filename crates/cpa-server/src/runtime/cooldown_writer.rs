//! Cooldown snapshots written off the request threads. A request only hands over its
//! snapshot; one drain at a time writes the newest snapshot, so writes stay in order and
//! a burst of changes costs one write.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::SystemTime;

use crate::cooldown_store::{Backend, Record, Written};

/// Where a snapshot is saved.
pub(super) enum Target {
    Files(PathBuf),
    Backend(Arc<dyn Backend>),
}

/// The complete live set of cooldown records at one moment.
pub(super) struct Snapshot {
    pub(super) target: Target,
    pub(super) records: Vec<Record>,
    pub(super) now: SystemTime,
    /// The credentials whose records changed since the last snapshot; `None` for all.
    pub(super) changed: Option<BTreeSet<String>>,
}

#[derive(Default)]
struct State {
    pending: Option<Snapshot>,
    draining: bool,
}

#[derive(Default)]
pub(super) struct CooldownWriter {
    state: Mutex<State>,
    idle: Condvar,
    /// The directory the last file save went to and the files it left there; `None`
    /// makes the next file save write every file and list the directory for stale ones.
    written: Mutex<Option<(PathBuf, Written)>>,
}

impl CooldownWriter {
    /// Replaces any snapshot not yet written, keeping its changed credentials, and makes
    /// sure a drain will write it: on the blocking pool inside a Tokio runtime, else on
    /// this thread before returning.
    pub(super) fn submit(self: &Arc<Self>, mut snapshot: Snapshot) {
        {
            let mut state = self.lock();
            if let Some(older) = state.pending.take() {
                snapshot.changed = match (older.changed, snapshot.changed) {
                    (Some(mut older), Some(newer)) => {
                        older.extend(newer);
                        Some(older)
                    }
                    _ => None,
                };
            }
            state.pending = Some(snapshot);
            if state.draining {
                return;
            }
            state.draining = true;
        }
        let writer = self.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => drop(handle.spawn_blocking(move || writer.drain())),
            Err(_) => writer.drain(),
        }
    }

    /// Blocks until every submitted snapshot is written.
    pub(super) fn flush(&self) {
        let mut state = self.lock();
        while state.draining {
            state = self.idle.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn drain(&self) {
        loop {
            let snapshot = {
                let mut state = self.lock();
                match state.pending.take() {
                    Some(snapshot) => snapshot,
                    None => {
                        state.draining = false;
                        self.idle.notify_all();
                        return;
                    }
                }
            };
            self.write(snapshot);
        }
    }

    fn write(&self, snapshot: Snapshot) {
        let saved = match snapshot.target {
            Target::Backend(backend) => backend.save(snapshot.records, snapshot.now),
            Target::Files(dir) => {
                let mut written = self.written.lock().unwrap_or_else(PoisonError::into_inner);
                let previous = written.as_ref().filter(|(d, _)| *d == dir).map(|(_, w)| w);
                match crate::cooldown_store::save_changed(
                    &dir,
                    snapshot.records,
                    snapshot.now,
                    previous,
                    snapshot.changed.as_ref(),
                ) {
                    Ok(saved) => {
                        *written = Some((dir, saved));
                        Ok(())
                    }
                    Err(error) => {
                        *written = None;
                        Err(error)
                    }
                }
            }
        };
        if let Err(error) = saved {
            tracing::warn!(%error, "failed to persist cooldown state");
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
