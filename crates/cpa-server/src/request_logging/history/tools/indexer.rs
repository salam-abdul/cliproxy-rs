//! The tool indexer: one background thread that keeps the tool index in step with the
//! finished history files. It owns the only SQLite connection the server opens on the
//! index while writing, so the history writer only queues work and never waits on it.

use std::collections::HashSet;
use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use super::super::Settings;
use super::super::dir::HistoryDir;
use super::super::file::{SCAN_COLUMNS, for_each_group};
use super::scan::scan;
use super::store::{Fingerprint, Index, rel_path};

/// How long after a refused commit the next pass starts: a delete changed a file while
/// it was read, and newer files wait for it so their continuations find their parents.
const RETRY: Duration = Duration::from_secs(10);
/// How long the thread sleeps with nothing to do.
const IDLE: Duration = Duration::from_secs(3600);

/// The clock that dates cap removals and sweeps.
pub(crate) type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

enum Msg {
    Nudge,
    #[cfg(test)]
    Sync(SyncSender<()>),
    #[cfg(test)]
    Tick(SyncSender<()>),
}

/// Work from the history writer, applied at the start of the next pass.
enum Queued {
    Forget(String),
    Capped(String, i64),
}

/// How a pass ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Done,
    /// A file changed between its read and its commit.
    Refused,
    Stopped,
    Failed,
}

struct Shared {
    /// The settings of the newest nudge: merged nudges run with the last one's.
    latest: Mutex<Option<Settings>>,
    queue: Mutex<Vec<Queued>>,
    stop: AtomicBool,
    now: Clock,
    #[cfg(test)]
    hooks: Hooks,
}

enum State {
    Idle,
    Running { tx: SyncSender<Msg>, done: Receiver<()> },
    Closed,
}

/// The process's tool indexer. Its thread starts at the first nudge and ends at
/// [`Indexer::shutdown`] or when the indexer is dropped.
pub(crate) struct Indexer {
    shared: Arc<Shared>,
    state: Mutex<State>,
}

impl Indexer {
    pub(crate) fn new(now: Clock) -> Self {
        Self {
            shared: Arc::new(Shared {
                latest: Mutex::new(None),
                queue: Mutex::new(Vec::new()),
                stop: AtomicBool::new(false),
                now,
                #[cfg(test)]
                hooks: Hooks::default(),
            }),
            state: Mutex::new(State::Idle),
        }
    }

    /// Asks for a pass under `settings`. Never blocks: a pass already asked for takes
    /// this one's settings instead.
    pub(crate) fn nudge(&self, settings: &Settings) {
        if self.shared.stop.load(Ordering::Relaxed) {
            return;
        }
        *self.shared.latest.lock().unwrap_or_else(PoisonError::into_inner) = Some(settings.clone());
        self.wake();
    }

    /// Drops the facts of the history file `rel` (relative to the history folder) at
    /// the start of the next pass: a delete rewrote or removed it.
    pub(crate) fn forget(&self, rel: String) {
        self.enqueue(Queued::Forget(rel));
    }

    /// Marks the history file `rel` as removed by the size cap now, so its facts stay for
    /// 31 days.
    pub(crate) fn capped(&self, rel: String) {
        let at = (self.shared.now)().timestamp_millis();
        self.enqueue(Queued::Capped(rel, at));
    }

    fn enqueue(&self, work: Queued) {
        self.shared
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(work);
        self.wake();
    }

    /// Starts the thread when settings are known, and asks it for a pass.
    fn wake(&self) {
        if let Some(tx) = self.sender() {
            // A full channel already holds a nudge, which reads the newest settings.
            let _ = tx.try_send(Msg::Nudge);
        }
    }

    fn sender(&self) -> Option<SyncSender<Msg>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match &*state {
            State::Running { tx, .. } => Some(tx.clone()),
            State::Closed => None,
            State::Idle => {
                if self.shared.stop.load(Ordering::Relaxed)
                    || self
                        .shared
                        .latest
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .is_none()
                {
                    return None;
                }
                let (tx, rx) = sync_channel(1);
                let (finished, done) = sync_channel(1);
                let shared = Arc::clone(&self.shared);
                let spawned = std::thread::Builder::new().name("history-tools".into()).spawn(move || {
                    run(&shared, &rx);
                    let _ = finished.send(());
                });
                match spawned {
                    Ok(_) => {
                        *state = State::Running { tx: tx.clone(), done };
                        Some(tx)
                    }
                    Err(error) => {
                        tracing::warn!("history: cannot start the tool indexer thread: {error}");
                        None
                    }
                }
            }
        }
    }

    /// Stops the thread after the file it is reading, without committing that file,
    /// waiting at most `wait`. Later nudges do nothing.
    pub(crate) fn shutdown(&self, wait: Duration) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let state = std::mem::replace(
            &mut *self.state.lock().unwrap_or_else(PoisonError::into_inner),
            State::Closed,
        );
        let State::Running { tx, done } = state else { return };
        drop(tx);
        if done.recv_timeout(wait).is_err() {
            tracing::warn!("history: the tool indexer did not stop in time");
        }
    }
}

fn run(shared: &Shared, rx: &Receiver<Msg>) {
    let mut retry: Option<Instant> = None;
    loop {
        let wait = retry.map_or(IDLE, |at| at.saturating_duration_since(Instant::now()));
        match rx.recv_timeout(wait) {
            Ok(Msg::Nudge) => {}
            #[cfg(test)]
            Ok(Msg::Sync(ack)) => {
                let _ = ack.send(());
                continue;
            }
            #[cfg(test)]
            Ok(Msg::Tick(ack)) => {
                if retry.is_some() {
                    retry = (pass(shared) == End::Refused).then(|| Instant::now() + RETRY);
                }
                let _ = ack.send(());
                continue;
            }
            Err(RecvTimeoutError::Timeout) if retry.is_some() => {}
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        retry = (pass(shared) == End::Refused).then(|| Instant::now() + RETRY);
    }
}

/// One pass: apply queued work, forget files gone from disk, then index every file not
/// indexed as it now is, oldest first, so a continuation's parent is indexed before it.
fn pass(shared: &Shared) -> End {
    let Some(settings) = shared.latest.lock().unwrap_or_else(PoisonError::into_inner).clone() else {
        return End::Done;
    };
    let queued = std::mem::take(&mut *shared.queue.lock().unwrap_or_else(PoisonError::into_inner));
    if !settings.dir.is_dir() {
        // Nothing was written yet, so nothing was indexed.
        return End::Done;
    }
    let result = (|| -> anyhow::Result<End> {
        let mut index = Index::open(&settings.dir)?;
        for work in queued {
            match work {
                Queued::Forget(rel) => index.forget(&rel)?,
                Queued::Capped(rel, at) => index.capped(&rel, at)?,
            }
        }
        let history = HistoryDir::new(&settings.dir);
        let files = history.files()?;
        let on_disk: HashSet<String> = files
            .iter()
            .map(|file| format!("{}/{}", file.partition, file.name))
            .collect();
        index.sweep(&on_disk, (shared.now)().timestamp_millis())?;
        let indexed = index.indexed()?;
        for file in &files {
            if shared.stop.load(Ordering::Relaxed) {
                return Ok(End::Stopped);
            }
            let Some(rel) = rel_path(&settings.dir, &file.path) else {
                continue;
            };
            let Ok(current) = Fingerprint::from_path(&file.path) else {
                continue;
            };
            if indexed.get(&rel).is_some_and(|(seen, _)| *seen == current) {
                continue;
            }
            // Gone or not openable now: the next pass sees it again or sweeps it.
            let Ok(handle) = File::open(&file.path) else { continue };
            let Ok(read_as) = Fingerprint::from_file(&handle) else {
                continue;
            };
            let mut sends = Vec::new();
            let read = for_each_group(&handle, SCAN_COLUMNS, |rows| {
                if shared.stop.load(Ordering::Relaxed) {
                    return Err(std::io::Error::other("stopping"));
                }
                sends.extend(rows.iter().map(|row| scan(row, &settings.alias_prefixes)));
                Ok(())
            });
            drop(handle);
            if shared.stop.load(Ordering::Relaxed) {
                return Ok(End::Stopped);
            }
            let unreadable = read.is_err();
            if unreadable {
                sends.clear();
            }
            #[cfg(test)]
            shared.hooks.before_commit(&rel);
            if shared.stop.load(Ordering::Relaxed) {
                return Ok(End::Stopped);
            }
            #[cfg(test)]
            index.set_hold(shared.hooks.take_hold());
            if !index.commit(&rel, &read_as, &file.path, &sends, unreadable)? {
                return Ok(End::Refused);
            }
        }
        Ok(End::Done)
    })();
    result.unwrap_or_else(|_| {
        // Fixed text: errors here can name history paths.
        tracing::warn!("history: tool index pass stopped");
        End::Failed
    })
}

/// Points where a test holds the indexer thread.
#[cfg(test)]
#[derive(Default)]
struct Hooks {
    /// Between reading a file and committing it: sends the file, then waits.
    before_commit: Mutex<Option<(SyncSender<String>, Receiver<()>)>>,
    /// Inside the commit transaction, after the file was checked.
    hold: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

#[cfg(test)]
impl Hooks {
    fn before_commit(&self, rel: &str) {
        let hook = self.before_commit.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some((reached, release)) = hook {
            let _ = reached.send(rel.to_owned());
            let _ = release.recv();
        }
    }

    fn take_hold(&self) -> Option<Box<dyn FnOnce() + Send>> {
        self.hold.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

#[cfg(test)]
impl Indexer {
    /// Waits until the thread has taken every earlier message, so a nudge sent before
    /// has finished its pass.
    pub(crate) fn sync(&self) {
        if let Some(tx) = self.sender() {
            let (ack, done) = sync_channel(1);
            if tx.send(Msg::Sync(ack)).is_ok() {
                let _ = done.recv();
            }
        }
    }

    /// Runs the pass a refused commit scheduled, as if its delay had passed.
    pub(crate) fn tick(&self) {
        if let Some(tx) = self.sender() {
            let (ack, done) = sync_channel(1);
            if tx.send(Msg::Tick(ack)).is_ok() {
                let _ = done.recv();
            }
        }
    }

    /// Holds the next file between its read and its commit: the first receiver gets the
    /// file's relative path when it is reached, and the thread goes on when the sender
    /// sends or is dropped.
    pub(crate) fn pause_before_commit(&self) -> (Receiver<String>, SyncSender<()>) {
        let (reached, at) = sync_channel(1);
        let (release, wait) = sync_channel(1);
        *self.shared.hooks.before_commit.lock().unwrap() = Some((reached, wait));
        (at, release)
    }

    /// Holds the next commit inside its transaction, the same way.
    pub(crate) fn pause_in_commit(&self) -> (Receiver<()>, SyncSender<()>) {
        let (reached, at) = sync_channel(1);
        let (release, wait) = sync_channel::<()>(1);
        *self.shared.hooks.hold.lock().unwrap() = Some(Box::new(move || {
            let _ = reached.send(());
            let _ = wait.recv();
        }));
        (at, release)
    }

    /// Runs one pass under `settings` on the calling thread.
    pub(crate) fn pass_here(&self, settings: &Settings) {
        *self.shared.latest.lock().unwrap() = Some(settings.clone());
        pass(&self.shared);
    }

    pub(crate) fn stopping(&self) -> bool {
        self.shared.stop.load(Ordering::Relaxed)
    }

    pub(crate) fn is_running(&self) -> bool {
        matches!(
            *self.state.lock().unwrap_or_else(PoisonError::into_inner),
            State::Running { .. }
        )
    }
}

#[cfg(test)]
#[path = "indexer_tests.rs"]
mod tests;
