//! The history writer: one background thread per process takes finished sends from a
//! bounded channel, joins and decodes their bodies, and writes row groups. The request
//! path only clones or copies chunks and calls `try_send`; a full channel drops the row
//! and counts it, so inference never waits on the disk.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use cpa_core::config::Config;

use super::Settings;
use super::body::{BODY_CAP, Body, Budget, PendingRow};
use super::delete::apply_tombstones;
use super::dir::HistoryDir;
use super::file::ParquetFile;
use super::row::{Row, body_columns};

pub(crate) const CHANNEL: usize = 256;
const MAINTENANCE: Duration = Duration::from_secs(3600);
const DELETE_RETRY: Duration = Duration::from_secs(10);

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
    /// A row arrived after the writer stopped, and that was logged.
    stop_reported: AtomicBool,
    limits: Limits,
    budget: Budget,
}

impl Default for Writer {
    fn default() -> Self {
        Self {
            state: Mutex::new(State::Idle),
            lost: Arc::default(),
            stop_reported: AtomicBool::new(false),
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
            budget: Budget::new(super::body::IN_FLIGHT_CAP),
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
        let Some(settings) = super::settings(cfg).as_ref().clone() else {
            return;
        };
        let sent = self
            .sender()
            .map(|tx| tx.try_send(Command::Row(Box::new(row), settings)));
        if matches!(sent, Some(Ok(()))) {
            return;
        }
        self.lost.fetch_add(1, Ordering::Relaxed);
        // A full queue is reported with the next file; a stopped writer never writes
        // again, so it is reported once here.
        if matches!(sent, None | Some(Err(TrySendError::Disconnected(_))))
            && !self.stop_reported.swap(true, Ordering::Relaxed)
        {
            tracing::warn!("history: a row arrived after the writer stopped and was not written");
        }
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
        let history = HistoryDir::new(&dir);
        let stones = history.tombstones();
        let Some(since) = stones.iter().map(|stone| stone.created_us).min() else {
            return;
        };
        let retry = Some(Instant::now() + DELETE_RETRY);
        let Ok(Some(_lock)) = history.lock() else {
            self.retry_deletes = retry;
            return;
        };
        let Ok(files) = history.files() else {
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
        self.buffer_bytes += row.body_bytes();
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
        let frames = |bodies: Option<Vec<Body>>, bytes: &mut i64, truncated: &mut bool| {
            bodies.map(|bodies| {
                bodies
                    .into_iter()
                    .map(|body| {
                        *bytes = bytes.saturating_add(i64::try_from(body.total()).unwrap_or(i64::MAX));
                        *truncated |= body.truncated();
                        String::from_utf8_lossy(&body.into_vec()).into_owned()
                    })
                    .collect()
            })
        };
        row.request_frames = frames(request_frames, &mut row.request_bytes, &mut row.request_truncated);
        row.response_frames = frames(response_frames, &mut row.response_bytes, &mut row.response_truncated);
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
        let stones = HistoryDir::new(&window.dir).tombstones();
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
                HistoryDir::new(&window.dir).apply_cap(cap);
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
        let open = self
            .window
            .as_ref()
            .and_then(|w| w.file.as_ref())
            .map(ParquetFile::partial);
        let history = HistoryDir::new(&dir);
        history.remove_dead_partials(open);
        // Applied before any expires, so a delete waiting since the last run counts.
        self.apply_deletes();
        history.expire_tombstones(Utc::now());
        if let Some(cap) = cap {
            history.apply_cap(cap);
        }
    }
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

#[cfg(test)]
#[path = "writer_tests.rs"]
mod tests;
