//! The history writer: one background thread per process takes finished sends from a
//! bounded channel, joins and decodes their bodies, and writes row groups. The request
//! path only clones or copies chunks and calls `try_send`; a full channel drops the row
//! and counts it, so inference never waits on the disk.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use cpa_core::config::Config;

use super::Settings;
use super::delete::{apply_tombstones, expire_tombstones, pending_tombstones};
use super::file::{ParquetFile, Row, body_columns, history_files, partial_pid, remove_empty_partitions};
use super::lock::{DirLock, is_abandoned};

// ponytail: each body keeps its first 32 MiB, as the request log does; raise with a
// spool-backed body if longer bodies need keeping.
pub(crate) const BODY_CAP: u64 = 32 << 20;
/// Body bytes held by sends in flight or queued, across the process.
const IN_FLIGHT_CAP: u64 = 512 << 20;
pub(crate) const CHANNEL: usize = 256;
const MAINTENANCE: Duration = Duration::from_secs(3600);
const DELETE_RETRY: Duration = Duration::from_secs(10);

/// A byte budget shared by every [`Body`] that reserves from it.
#[derive(Clone)]
pub(crate) struct Budget(Arc<BudgetState>);

struct BudgetState {
    used: AtomicU64,
    cap: u64,
}

impl Budget {
    pub(crate) fn new(cap: u64) -> Self {
        Self(Arc::new(BudgetState {
            used: AtomicU64::new(0),
            cap,
        }))
    }

    fn global() -> &'static Budget {
        static GLOBAL: OnceLock<Budget> = OnceLock::new();
        GLOBAL.get_or_init(|| Budget::new(IN_FLIGHT_CAP))
    }

    fn try_reserve(&self, n: u64) -> bool {
        let used = &self.0.used;
        let mut current = used.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(n).filter(|total| *total <= self.0.cap) else {
                return false;
            };
            match used.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    fn release(&self, n: u64) {
        self.0.used.fetch_sub(n, Ordering::AcqRel);
    }

    #[cfg(test)]
    pub(crate) fn used(&self) -> u64 {
        self.0.used.load(Ordering::Acquire)
    }
}

/// A body collected as chunks. It keeps at most [`BODY_CAP`] bytes and only what the
/// budget allows; past either it counts the rest and marks itself truncated.
pub(crate) struct Body {
    chunks: Vec<Bytes>,
    kept: u64,
    total: u64,
    truncated: bool,
    lines: bool,
    budget: Budget,
}

impl Body {
    pub(crate) fn new(budget: &Budget) -> Self {
        Self {
            chunks: Vec::new(),
            kept: 0,
            total: 0,
            truncated: false,
            lines: false,
            budget: budget.clone(),
        }
    }

    /// Keeps a handle to `bytes`, without copying.
    pub(crate) fn push_bytes(&mut self, bytes: &Bytes) {
        self.keep(bytes.len(), |n| bytes.slice(..n));
    }

    pub(crate) fn push_slice(&mut self, bytes: &[u8]) {
        self.keep(bytes.len(), |n| Bytes::copy_from_slice(&bytes[..n]));
    }

    /// Appends one scanned line, with `\n` between lines.
    pub(crate) fn push_line(&mut self, line: &[u8]) {
        if std::mem::replace(&mut self.lines, true) {
            self.push_bytes(&Bytes::from_static(b"\n"));
        }
        self.push_slice(line);
    }

    fn keep(&mut self, len: usize, take: impl FnOnce(usize) -> Bytes) {
        let len = len as u64;
        self.total = self.total.saturating_add(len);
        if self.truncated || len == 0 {
            return;
        }
        let n = len.min(BODY_CAP - self.kept);
        if n < len {
            self.truncated = true;
        }
        if n == 0 {
            return;
        }
        if !self.budget.try_reserve(n) {
            self.truncated = true;
            return;
        }
        self.kept += n;
        self.chunks.push(take(n as usize));
    }

    pub(crate) fn total(&self) -> u64 {
        self.total
    }

    pub(crate) fn truncated(&self) -> bool {
        self.truncated
    }

    pub(crate) fn into_vec(mut self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.kept as usize);
        for chunk in std::mem::take(&mut self.chunks) {
            out.extend_from_slice(&chunk);
        }
        out
    }
}

impl Drop for Body {
    fn drop(&mut self) {
        self.budget.release(self.kept);
    }
}

/// A send's row before its bodies are joined, decoded and split into columns.
pub(crate) struct PendingRow {
    pub(crate) row: Row,
    pub(crate) request: Body,
    pub(crate) response: Body,
    /// Websocket transports: one body per frame.
    pub(crate) request_frames: Option<Vec<Body>>,
    pub(crate) response_frames: Option<Vec<Body>>,
    /// The response's `content-encoding`, when its bytes are still encoded.
    pub(crate) response_encoding: Option<String>,
}

impl PendingRow {
    pub(crate) fn new(row: Row, budget: &Budget) -> Self {
        Self {
            row,
            request: Body::new(budget),
            response: Body::new(budget),
            request_frames: None,
            response_frames: None,
            response_encoding: None,
        }
    }
}

/// Thresholds for row groups and files, and the clock that dates them.
#[derive(Clone)]
pub(crate) struct Limits {
    pub(crate) rotate_after: Duration,
    pub(crate) rotate_bytes: u64,
    pub(crate) group_rows: usize,
    pub(crate) group_bytes: u64,
    pub(crate) now: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            rotate_after: Duration::from_secs(600),
            rotate_bytes: 512 << 20,
            group_rows: 4096,
            group_bytes: 64 << 20,
            now: Arc::new(Utc::now),
        }
    }
}

enum Command {
    Row(Box<PendingRow>, Settings),
    Shutdown(SyncSender<()>),
    #[cfg(test)]
    Flush(SyncSender<()>),
    #[cfg(test)]
    Tick(SyncSender<()>),
    #[cfg(test)]
    Sync(SyncSender<()>),
    #[cfg(test)]
    Pause(SyncSender<()>, Receiver<()>),
}

enum State {
    Idle,
    Running(SyncSender<Command>),
    Closed,
}

/// The process's history writer. Its thread starts with the first row while history
/// is on and ends at [`Writer::shutdown`] or when the writer is dropped.
pub struct Writer {
    state: Mutex<State>,
    lost: Arc<AtomicU64>,
    limits: Limits,
    budget: Budget,
}

impl Default for Writer {
    fn default() -> Self {
        Self {
            state: Mutex::new(State::Idle),
            lost: Arc::default(),
            limits: Limits::default(),
            budget: Budget::global().clone(),
        }
    }
}

impl Writer {
    #[cfg(test)]
    pub(crate) fn with_limits(limits: Limits) -> Self {
        Self {
            limits,
            budget: Budget::new(IN_FLIGHT_CAP),
            ..Self::default()
        }
    }

    pub(crate) fn budget(&self) -> &Budget {
        &self.budget
    }

    /// Rows dropped or not written since start.
    #[cfg(test)]
    pub(crate) fn lost(&self) -> u64 {
        self.lost.load(Ordering::Relaxed)
    }

    /// Queues `row` when history is on under `cfg`. Never blocks: a full queue drops it.
    pub(crate) fn submit(&self, cfg: &Config, row: PendingRow) {
        let settings = super::settings(cfg);
        if settings.is_none() {
            return;
        }
        let Some(settings) = settings.as_ref().clone() else {
            return;
        };
        let Some(tx) = self.sender() else {
            if self.lost.fetch_add(1, Ordering::Relaxed) == 0 || self.closed() {
                tracing::warn!("history: a row arrived after the writer stopped and was not written");
            }
            return;
        };
        if tx.try_send(Command::Row(Box::new(row), settings)).is_err() {
            self.lost.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn closed(&self) -> bool {
        matches!(
            *self.state.lock().unwrap_or_else(PoisonError::into_inner),
            State::Closed
        )
    }

    fn sender(&self) -> Option<SyncSender<Command>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match &*state {
            State::Running(tx) => Some(tx.clone()),
            State::Closed => None,
            State::Idle => {
                let (tx, rx) = sync_channel(CHANNEL);
                let worker = Worker::new(rx, self.limits.clone(), self.lost.clone());
                match std::thread::Builder::new()
                    .name("history-writer".into())
                    .spawn(move || worker.run())
                {
                    Ok(_) => {
                        *state = State::Running(tx.clone());
                        Some(tx)
                    }
                    Err(error) => {
                        tracing::warn!("history: cannot start the writer thread: {error}");
                        None
                    }
                }
            }
        }
    }

    /// Writes buffered rows and closes the open file, waiting at most `wait`. Later
    /// rows are ignored.
    pub fn shutdown(&self, wait: Duration) {
        let state = std::mem::replace(
            &mut *self.state.lock().unwrap_or_else(PoisonError::into_inner),
            State::Closed,
        );
        let State::Running(tx) = state else { return };
        let deadline = Instant::now() + wait;
        let (ack, done) = sync_channel(1);
        let mut command = Command::Shutdown(ack);
        loop {
            match tx.try_send(command) {
                Ok(()) => break,
                Err(std::sync::mpsc::TrySendError::Full(back)) if Instant::now() < deadline => {
                    command = back;
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => {
                    tracing::warn!("history: writer did not take the shutdown in time; buffered rows are lost");
                    return;
                }
            }
        }
        if done
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .is_err()
        {
            tracing::warn!("history: writer did not finish in time; the open file is lost");
        }
    }

    #[cfg(test)]
    pub(crate) fn is_running(&self) -> bool {
        matches!(
            *self.state.lock().unwrap_or_else(PoisonError::into_inner),
            State::Running(_)
        )
    }

    #[cfg(test)]
    fn call(&self, command: impl FnOnce(SyncSender<()>) -> Command) {
        let Some(tx) = self.sender() else { return };
        let (ack, done) = sync_channel(1);
        tx.send(command(ack)).unwrap();
        done.recv().unwrap();
    }

    /// Writes buffered rows and finishes the open file.
    #[cfg(test)]
    pub(crate) fn flush(&self) {
        self.call(Command::Flush);
    }

    /// Runs the timers as if the wait for the next command had timed out.
    #[cfg(test)]
    pub(crate) fn tick(&self) {
        self.call(Command::Tick);
    }

    /// Waits until the writer has taken every earlier command, running nothing else.
    #[cfg(test)]
    pub(crate) fn sync(&self) {
        self.call(Command::Sync);
    }

    /// Blocks the writer thread until the returned sender is dropped.
    #[cfg(test)]
    pub(crate) fn pause(&self) -> SyncSender<()> {
        let (release, wait) = sync_channel(1);
        self.call(|ack| Command::Pause(ack, wait));
        release
    }
}

/// The file being filled: created at its first row group, dated by its first row.
struct Window {
    dir: PathBuf,
    started: DateTime<Utc>,
    file: Option<ParquetFile>,
    rows: u64,
}

struct Worker {
    rx: Receiver<Command>,
    limits: Limits,
    lost: Arc<AtomicU64>,
    reported: u64,
    settings: Option<Settings>,
    window: Option<Window>,
    buffer: Vec<Row>,
    buffer_bytes: u64,
    seq: u32,
    maintained: Option<Instant>,
    decoder: Option<tokio::runtime::Runtime>,
    /// When to try pending deletes again after the history lock was busy or a rewrite
    /// failed.
    retry_deletes: Option<Instant>,
    /// The last attempt to apply pending deletes failed; its warning was logged.
    delete_failed: bool,
}

impl Worker {
    fn new(rx: Receiver<Command>, limits: Limits, lost: Arc<AtomicU64>) -> Self {
        Self {
            rx,
            limits,
            lost,
            reported: 0,
            settings: None,
            window: None,
            buffer: Vec::new(),
            buffer_bytes: 0,
            seq: 0,
            maintained: None,
            decoder: None,
            retry_deletes: None,
            delete_failed: false,
        }
    }

    fn run(mut self) {
        loop {
            match self.rx.recv_timeout(self.next_deadline()) {
                Ok(Command::Row(row, settings)) => self.row(*row, &settings),
                Ok(Command::Shutdown(ack)) => {
                    self.rotate();
                    let _ = ack.send(());
                    return;
                }
                #[cfg(test)]
                Ok(Command::Flush(ack)) => {
                    self.rotate();
                    let _ = ack.send(());
                }
                #[cfg(test)]
                Ok(Command::Tick(ack)) => {
                    // As if every deadline had passed.
                    if self.retry_deletes.is_some() {
                        self.retry_deletes = Some(Instant::now());
                    }
                    self.timers();
                    let _ = ack.send(());
                }
                #[cfg(test)]
                Ok(Command::Sync(ack)) => {
                    let _ = ack.send(());
                }
                #[cfg(test)]
                Ok(Command::Pause(ack, release)) => {
                    let _ = ack.send(());
                    let _ = release.recv();
                }
                Err(RecvTimeoutError::Timeout) => self.timers(),
                Err(RecvTimeoutError::Disconnected) => {
                    self.rotate();
                    return;
                }
            }
        }
    }

    fn now(&self) -> DateTime<Utc> {
        (self.limits.now)()
    }

    fn next_deadline(&self) -> Duration {
        let rotation = match &self.window {
            Some(window) => {
                let due = window.started + self.limits.rotate_after;
                (due - self.now()).to_std().unwrap_or(Duration::ZERO)
            }
            None => MAINTENANCE,
        };
        let retry = self
            .retry_deletes
            .map_or(MAINTENANCE, |at| at.saturating_duration_since(Instant::now()));
        rotation.min(retry)
    }

    fn timers(&mut self) {
        if self
            .window
            .as_ref()
            .is_some_and(|w| w.started + self.limits.rotate_after <= self.now())
        {
            self.rotate();
        }
        if self.maintained.is_some_and(|at| at.elapsed() >= MAINTENANCE) {
            self.maintain();
        }
        if self.retry_deletes.is_some_and(|at| at <= Instant::now()) {
            self.apply_deletes();
        }
    }

    /// Applies pending deletes to files finished since the oldest of them was asked
    /// for: a file that was open during a delete may hold rows the delete covers. The
    /// tombstones stay on disk, so a delete this cannot apply now, because the lock is
    /// busy or the server stops, is applied on a retry or after the next start.
    fn apply_deletes(&mut self) {
        self.retry_deletes = None;
        let Some(dir) = self.settings.as_ref().map(|s| s.dir.clone()) else {
            return;
        };
        let stones = pending_tombstones(&dir);
        let Some(since) = stones.iter().map(|stone| stone.created_us).min() else {
            return;
        };
        let retry = Some(Instant::now() + DELETE_RETRY);
        let Ok(Some(_lock)) = DirLock::try_acquire(&dir) else {
            self.retry_deletes = retry;
            return;
        };
        let Ok(files) = history_files(&dir) else {
            self.retry_deletes = retry;
            return;
        };
        // A file open during a delete started that day or the day before: rotation
        // closes every file at UTC midnight. Dates, not timestamps, so a file synced
        // before the delete but published after it is still covered.
        let first_day = DateTime::from_timestamp_micros(since)
            .map(|at| at.date_naive() - chrono::Days::new(1))
            .map(|day| format!("date={}", day.format("%Y-%m-%d")));
        let mut failed = false;
        for file in files
            .iter()
            .filter(|file| first_day.as_ref().is_none_or(|day| file.partition >= *day))
        {
            match apply_tombstones(&file.path, &stones) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    if !self.delete_failed {
                        tracing::warn!(
                            "history: cannot apply a pending delete to {}: {error}; retrying",
                            file.path.display()
                        );
                    }
                    failed = true;
                }
            }
        }
        self.delete_failed = failed;
        if failed {
            self.retry_deletes = retry;
        }
    }

    fn row(&mut self, pending: PendingRow, settings: &Settings) {
        if self.settings.as_ref() != Some(settings) {
            if self.window.as_ref().is_some_and(|w| w.dir != settings.dir) {
                self.rotate();
            }
            let moved = self.settings.as_ref().is_none_or(|s| s.dir != settings.dir);
            self.settings = Some(settings.clone());
            if moved {
                self.maintain();
            }
        }
        let now = self.now();
        if self
            .window
            .as_ref()
            .is_some_and(|w| w.started.date_naive() != now.date_naive())
        {
            self.rotate();
        }
        let row = self.finalize(pending);
        self.buffer_bytes += body_bytes(&row);
        self.buffer.push(row);
        self.window.get_or_insert_with(|| Window {
            dir: settings.dir.clone(),
            started: now,
            file: None,
            rows: 0,
        });
        if self.buffer.len() >= self.limits.group_rows || self.buffer_bytes >= self.limits.group_bytes {
            self.write_group();
        }
        if self
            .window
            .as_ref()
            .and_then(|w| w.file.as_ref())
            .is_some_and(|f| f.bytes_written() >= self.limits.rotate_bytes)
        {
            self.rotate();
        }
        // A queue that never empties never times out; rotation and maintenance must
        // still run on time.
        self.timers();
    }

    fn finalize(&mut self, pending: PendingRow) -> Row {
        let PendingRow {
            mut row,
            request,
            response,
            request_frames,
            response_frames,
            response_encoding,
        } = pending;
        row.request_bytes = i64::try_from(request.total()).unwrap_or(i64::MAX);
        row.request_truncated = request.truncated();
        if request.total() > 0 {
            (row.request_body, row.request_body_raw) = body_columns(request.into_vec(), row.request_truncated);
        }
        row.response_bytes = i64::try_from(response.total()).unwrap_or(i64::MAX);
        row.response_truncated = response.truncated();
        if response.total() > 0 {
            let mut body = response.into_vec();
            if let Some(encoding) = response_encoding {
                let failed;
                (body, failed) = self.decode(body, &encoding);
                row.response_truncated |= failed;
            }
            (row.response_body, row.response_body_raw) = body_columns(body, row.response_truncated);
        }
        let frames = |bodies: Option<Vec<Body>>, truncated: &mut bool| {
            bodies.map(|bodies| {
                bodies
                    .into_iter()
                    .map(|body| {
                        *truncated |= body.truncated();
                        String::from_utf8_lossy(&body.into_vec()).into_owned()
                    })
                    .collect()
            })
        };
        row.request_frames = frames(request_frames, &mut row.request_truncated);
        row.response_frames = frames(response_frames, &mut row.response_truncated);
        row
    }

    /// The decoded body and whether decoding stopped early (an error or the cap).
    fn decode(&mut self, body: Vec<u8>, encoding: &str) -> (Vec<u8>, bool) {
        if self.decoder.is_none() {
            match tokio::runtime::Builder::new_current_thread().build() {
                Ok(runtime) => self.decoder = Some(runtime),
                Err(_) => return (body, true),
            }
        }
        match &self.decoder {
            Some(runtime) => runtime.block_on(decode(body, encoding)),
            None => (body, true),
        }
    }

    fn write_group(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let mut rows = std::mem::take(&mut self.buffer);
        self.buffer_bytes = 0;
        let Some(window) = self.window.as_mut() else {
            self.lost.fetch_add(rows.len() as u64, Ordering::Relaxed);
            return;
        };
        let stones = pending_tombstones(&window.dir);
        rows.retain(|row| !stones.iter().any(|stone| stone.matches(row)));
        if rows.is_empty() {
            return;
        }
        let n = rows.len() as u64;
        if window.file.is_none() {
            match ParquetFile::create(&window.dir, window.started, self.seq) {
                Ok(file) => {
                    self.seq = self.seq.wrapping_add(1);
                    window.file = Some(file);
                }
                Err(error) => {
                    tracing::warn!("history: cannot create a file in {}: {error}", window.dir.display());
                    self.lost.fetch_add(n, Ordering::Relaxed);
                    return;
                }
            }
        }
        let Some(file) = window.file.as_mut() else { return };
        match file.write_group(&rows) {
            Ok(()) => window.rows += n,
            Err(error) => {
                tracing::warn!("history: cannot write a row group: {error}");
                self.lost.fetch_add(n + window.rows, Ordering::Relaxed);
                window.file = None;
                window.rows = 0;
            }
        }
    }

    fn rotate(&mut self) {
        self.write_group();
        if let Some(window) = self.window.take()
            && let Some(file) = window.file
        {
            match file.finish() {
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!("history: cannot finish a file: {error}");
                    self.lost.fetch_add(window.rows, Ordering::Relaxed);
                }
            }
            if let Some(cap) = self.settings.as_ref().and_then(|s| s.max_total_bytes) {
                apply_cap(&window.dir, cap);
            }
        }
        self.apply_deletes();
        let lost = self.lost.load(Ordering::Relaxed);
        if lost > self.reported {
            tracing::warn!("history: {} row(s) were not written", lost - self.reported);
            self.reported = lost;
        }
    }

    /// Removes files left by writers that are gone, then applies the size cap.
    fn maintain(&mut self) {
        self.maintained = Some(Instant::now());
        let Some(settings) = &self.settings else { return };
        let dir = settings.dir.clone();
        let cap = settings.max_total_bytes;
        remove_dead_partials(&dir);
        // Applied before any expires, so a delete waiting since the last run counts.
        self.apply_deletes();
        expire_tombstones(&dir, Utc::now());
        if let Some(cap) = cap {
            apply_cap(&dir, cap);
        }
    }
}

/// [`enforce_cap`] under the history lock; skipped this round while a delete holds it.
fn apply_cap(dir: &Path, cap: u64) {
    match DirLock::try_acquire(dir) {
        Ok(Some(_lock)) => {
            if let Err(error) = enforce_cap(dir, cap) {
                tracing::warn!("history: cannot apply max-total-size-mb: {error}");
            }
        }
        Ok(None) => {}
        Err(error) => tracing::warn!("history: cannot lock {}: {error}", dir.display()),
    }
}

fn body_bytes(row: &Row) -> u64 {
    let text = |s: &Option<String>| s.as_ref().map_or(0, String::len);
    let raw = |b: &Option<Vec<u8>>| b.as_ref().map_or(0, Vec::len);
    let frames = |f: &Option<Vec<String>>| f.as_ref().map_or(0, |f| f.iter().map(String::len).sum());
    (text(&row.request_body)
        + text(&row.response_body)
        + raw(&row.request_body_raw)
        + raw(&row.response_body_raw)
        + frames(&row.request_frames)
        + frames(&row.response_frames)) as u64
}

async fn decode(raw: Vec<u8>, encoding: &str) -> (Vec<u8>, bool) {
    use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder};
    use tokio::io::{AsyncRead, AsyncReadExt};
    let mut reader: std::pin::Pin<Box<dyn AsyncRead + Send + '_>> = match encoding.trim().to_ascii_lowercase().as_str()
    {
        "gzip" => {
            let mut decoder = GzipDecoder::new(raw.as_slice());
            decoder.multiple_members(true);
            Box::pin(decoder)
        }
        "br" => Box::pin(BrotliDecoder::new(raw.as_slice())),
        "deflate" => Box::pin(ZlibDecoder::new(raw.as_slice())),
        "zstd" => {
            let mut decoder = ZstdDecoder::with_params(
                raw.as_slice(),
                &[async_compression::zstd::DParameter::window_log_max(
                    crate::request_logging::ZSTD_WINDOW_LOG_MAX,
                )],
            );
            decoder.multiple_members(true);
            Box::pin(decoder)
        }
        _ => return (raw, false),
    };
    let mut out = Vec::new();
    let failed = reader.as_mut().take(BODY_CAP + 1).read_to_end(&mut out).await.is_err();
    drop(reader);
    if failed && out.is_empty() {
        return (raw, true);
    }
    let over = out.len() as u64 > BODY_CAP;
    out.truncate(BODY_CAP as usize);
    (out, failed || over)
}

/// Deletes the oldest history files, by partition date then name, until the files
/// under `dir` total at most `cap` bytes. Returns the bytes freed.
pub(crate) fn enforce_cap(dir: &Path, cap: u64) -> std::io::Result<u64> {
    let files = history_files(dir)?;
    let mut total: u64 = files.iter().map(|f| f.len).sum();
    let mut freed = 0;
    for file in files {
        if total <= cap {
            break;
        }
        if std::fs::remove_file(&file.path).is_ok() {
            total -= file.len;
            freed += file.len;
        }
    }
    remove_empty_partitions(dir);
    Ok(freed)
}

fn remove_dead_partials(dir: &Path) {
    let Ok(partitions) = std::fs::read_dir(dir) else { return };
    let mut removed = 0u64;
    for partition in partitions.flatten() {
        if !partition.file_name().to_string_lossy().starts_with("date=") {
            continue;
        }
        let Ok(files) = std::fs::read_dir(partition.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            // This process's own files are open and locked here; probing them would
            // release that lock where the OS emulates flock with process-wide locks.
            if partial_pid(&name).is_some_and(|pid| pid != std::process::id())
                && is_abandoned(&file.path()).unwrap_or(false)
                && std::fs::remove_file(file.path()).is_ok()
            {
                removed += 1;
            }
        }
    }
    if removed > 0 {
        tracing::warn!("history: removed {removed} unfinished file(s) left by a writer that stopped");
    }
}

#[cfg(test)]
#[path = "writer_tests.rs"]
mod tests;
