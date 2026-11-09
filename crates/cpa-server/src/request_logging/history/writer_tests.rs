use super::*;
use crate::request_logging::history::file::read_rows;
use crate::request_logging::history::file::tests::TempDir;
use crate::request_logging::history::row::{Outcome, Row, Source};
use bytes::Bytes;
use std::path::Path;
use std::sync::Mutex;

fn on(dir: &Path) -> Config {
    crate::testing::config(&format!(
        "observability: {{history: {{enable: true, dir: '{}'}}}}",
        dir.display()
    ))
}

fn off() -> Config {
    crate::testing::config("{}")
}

fn at(rfc3339: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(rfc3339).unwrap().with_timezone(&Utc)
}

struct Clock(Arc<Mutex<DateTime<Utc>>>);

impl Clock {
    fn new(start: &str) -> Self {
        Self(Arc::new(Mutex::new(at(start))))
    }
    fn set(&self, to: &str) {
        *self.0.lock().unwrap() = at(to);
    }
    fn limits(&self) -> Limits {
        let clock = self.0.clone();
        Limits {
            now: Arc::new(move || *clock.lock().unwrap()),
            ..Limits::default()
        }
    }
}

fn row(id: &str) -> Row {
    Row {
        exchange_id: id.into(),
        started_at_us: 1,
        source: Source::Proxy,
        outcome: Outcome::Complete,
        ..Row::default()
    }
}

fn pending(writer: &Writer, id: &str, request: &[u8], response_lines: &[&[u8]]) -> PendingRow {
    let mut p = PendingRow::new(row(id), writer.budget());
    p.request.push_slice(request);
    for line in response_lines {
        p.response.push_line(line);
    }
    p
}

fn parquet_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(partitions) = std::fs::read_dir(dir) else {
        return out;
    };
    for partition in partitions {
        let partition = partition.unwrap().path();
        if !partition.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(&partition).unwrap() {
            let path = file.unwrap().path();
            if path.extension().is_some_and(|e| e == "parquet") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn all_rows(dir: &Path) -> Vec<Row> {
    parquet_files(dir).iter().flat_map(|p| read_rows(p).unwrap()).collect()
}

#[test]
fn rows_land_in_a_dated_partition_after_rotation() {
    let dir = TempDir::new();
    let clock = Clock::new("2026-11-09T08:15:30Z");
    let writer = Writer::with_limits(clock.limits());
    writer.submit(
        &on(&dir.0),
        pending(&writer, "a", b"{\"model\":\"m\"}", &[b"data: 1", b"data: 2"]),
    );
    writer.flush();
    let files = parquet_files(&dir.0);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].parent().unwrap().file_name().unwrap(), "date=2026-11-09");
    let rows = read_rows(&files[0]).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].request_body.as_deref(), Some("{\"model\":\"m\"}"));
    assert_eq!(rows[0].response_body.as_deref(), Some("data: 1\ndata: 2"));
    assert_eq!(rows[0].request_bytes, 13);
    assert_eq!(rows[0].response_bytes, 15);
    assert!(!rows[0].request_truncated && !rows[0].response_truncated);
}

#[test]
fn rotation_happens_on_age() {
    let dir = TempDir::new();
    let clock = Clock::new("2026-11-09T08:00:00Z");
    let writer = Writer::with_limits(clock.limits());
    writer.submit(&on(&dir.0), pending(&writer, "a", b"x", &[]));
    // `submit` returns before the thread dates the row.
    writer.tick();
    clock.set("2026-11-09T08:05:00Z");
    writer.tick();
    assert!(parquet_files(&dir.0).is_empty());
    clock.set("2026-11-09T08:10:01Z");
    writer.tick();
    assert_eq!(parquet_files(&dir.0).len(), 1);
}

#[test]
fn rotation_happens_on_size() {
    let dir = TempDir::new();
    let clock = Clock::new("2026-11-09T08:00:00Z");
    let writer = Writer::with_limits(Limits {
        rotate_bytes: 1,
        group_rows: 1,
        ..clock.limits()
    });
    writer.submit(&on(&dir.0), pending(&writer, "a", b"x", &[]));
    writer.submit(&on(&dir.0), pending(&writer, "b", b"y", &[]));
    writer.tick();
    assert_eq!(parquet_files(&dir.0).len(), 2);
}

#[test]
fn rotation_happens_on_utc_date_change() {
    let dir = TempDir::new();
    let clock = Clock::new("2026-11-09T23:59:50Z");
    let writer = Writer::with_limits(clock.limits());
    writer.submit(&on(&dir.0), pending(&writer, "a", b"x", &[]));
    writer.tick();
    clock.set("2026-11-10T00:00:10Z");
    writer.submit(&on(&dir.0), pending(&writer, "b", b"y", &[]));
    writer.flush();
    let files = parquet_files(&dir.0);
    let partitions: Vec<_> = files
        .iter()
        .map(|f| f.parent().unwrap().file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(partitions, ["date=2026-11-09", "date=2026-11-10"]);
    assert_eq!(read_rows(&files[0]).unwrap()[0].exchange_id, "a");
    assert_eq!(read_rows(&files[1]).unwrap()[0].exchange_id, "b");
}

#[test]
fn shutdown_flushes_buffered_rows_within_the_wait() {
    let dir = TempDir::new();
    let writer = Writer::with_limits(Limits::default());
    writer.submit(&on(&dir.0), pending(&writer, "a", b"x", &[]));
    writer.shutdown(Duration::from_secs(5));
    assert_eq!(all_rows(&dir.0).len(), 1);
    writer.submit(&on(&dir.0), pending(&writer, "b", b"y", &[]));
    assert!(!writer.is_running());
    assert_eq!(all_rows(&dir.0).len(), 1);
    assert_eq!(writer.lost(), 1, "a row after shutdown is counted");
}

#[test]
fn rows_after_shutdown_warn_once() {
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let capture = Capture::default();
    let sink = capture.clone();
    let _log = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .finish(),
    );
    let dir = TempDir::new();
    let writer = Writer::with_limits(Limits::default());
    writer.shutdown(Duration::from_secs(5));
    for id in ["a", "b", "c"] {
        writer.submit(&on(&dir.0), pending(&writer, id, b"x", &[]));
    }
    assert_eq!(writer.lost(), 3);
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert_eq!(logs.matches("was not written").count(), 1, "{logs}");
}

fn dead_pid() -> u32 {
    let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" })
        .args(if cfg!(windows) { &["/C", "exit"][..] } else { &[][..] })
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

#[test]
fn a_dead_processes_partial_is_removed_at_start() {
    let dir = TempDir::new();
    let partition = dir.0.join("date=2026-11-01");
    std::fs::create_dir_all(&partition).unwrap();
    let dead = partition.join(format!("081530-{}-0000.partial", dead_pid()));
    std::fs::write(&dead, b"PAR1").unwrap();
    let writer = Writer::with_limits(Limits::default());
    writer.submit(&on(&dir.0), pending(&writer, "a", b"x", &[]));
    writer.flush();
    assert!(!dead.exists());
}

#[test]
fn an_abandoned_partial_named_with_this_processes_pid_is_removed() {
    // A container's server is always PID 1, so a crashed run's files carry the PID
    // of the run that follows it.
    let dir = TempDir::new();
    let partition = dir.0.join("date=2026-11-01");
    std::fs::create_dir_all(&partition).unwrap();
    let stale = partition.join(format!("081530-{}-0000.partial", std::process::id()));
    std::fs::write(&stale, b"PAR1").unwrap();
    let writer = Writer::with_limits(Limits::default());
    writer.submit(&on(&dir.0), pending(&writer, "a", b"x", &[]));
    writer.flush();
    assert!(!stale.exists());
}

#[test]
fn a_live_processes_partial_survives_startup_cleanup() {
    let dir = TempDir::new();
    let partition = dir.0.join("date=2026-11-01");
    std::fs::create_dir_all(&partition).unwrap();
    // Another writer's file: any PID, held open with its lock.
    let live = partition.join("081530-1-9999.partial");
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&live)
        .unwrap();
    assert!(crate::request_logging::history::lock::try_lock(&held).unwrap());
    let writer = Writer::with_limits(Limits::default());
    writer.submit(&on(&dir.0), pending(&writer, "a", b"x", &[]));
    writer.flush();
    assert!(live.exists());
    drop(held);
}

#[test]
fn websocket_byte_counts_add_up_every_frame() {
    let dir = TempDir::new();
    let writer = Writer::with_limits(Limits::default());
    let mut p = PendingRow::new(row("a"), writer.budget());
    let frames = |bodies: &[&[u8]]| {
        bodies
            .iter()
            .map(|b| {
                let mut body = Body::new(writer.budget());
                body.push_slice(b);
                body
            })
            .collect::<Vec<_>>()
    };
    p.request_frames = Some(frames(&[b"abc"]));
    p.response_frames = Some(frames(&[b"de", b"fghi"]));
    writer.submit(&on(&dir.0), p);
    writer.flush();
    let rows = all_rows(&dir.0);
    assert_eq!((rows[0].request_bytes, rows[0].response_bytes), (3, 6));
}

#[test]
fn a_full_channel_drops_rows_and_counts_them() {
    let dir = TempDir::new();
    let writer = Writer::with_limits(Limits::default());
    let cfg = on(&dir.0);
    let release = writer.pause();
    for i in 0..CHANNEL {
        writer.submit(&cfg, pending(&writer, &format!("r{i}"), b"x", &[]));
    }
    assert_eq!(writer.lost(), 0);
    let started = std::time::Instant::now();
    writer.submit(&cfg, pending(&writer, "overflow", b"x", &[]));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(writer.lost(), 1);
    drop(release);
    writer.flush();
    assert_eq!(all_rows(&dir.0).len(), CHANNEL);
}

#[test]
fn writer_starts_on_first_row_after_reload() {
    let dir = TempDir::new();
    let writer = Writer::with_limits(Limits::default());
    writer.submit(&off(), pending(&writer, "a", b"x", &[]));
    assert!(!writer.is_running());
    assert!(!dir.0.exists());
    writer.submit(&on(&dir.0), pending(&writer, "b", b"y", &[]));
    writer.flush();
    let rows = all_rows(&dir.0);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].exchange_id, "b");
}

#[test]
fn an_unwritable_dir_counts_rows_and_never_fails_requests() {
    let dir = TempDir::new();
    std::fs::create_dir_all(&dir.0).unwrap();
    let blocked = dir.0.join("not-a-dir");
    std::fs::write(&blocked, b"").unwrap();
    let writer = Writer::with_limits(Limits::default());
    writer.submit(&on(&blocked), pending(&writer, "a", b"x", &[]));
    writer.flush();
    assert_eq!(writer.lost(), 1);
    let ok = dir.0.join("ok");
    writer.submit(&on(&ok), pending(&writer, "b", b"y", &[]));
    writer.flush();
    assert_eq!(all_rows(&ok).len(), 1);
}

#[test]
fn a_native_body_is_decoded_on_the_writer_thread() {
    let dir = TempDir::new();
    let writer = Writer::with_limits(Limits::default());
    let mut p = PendingRow::new(row("n"), writer.budget());
    let encoded = zstd::bulk::compress(b"event: message_start\ndata: {}\n\n", 3).unwrap();
    p.response.push_bytes(&Bytes::from(encoded));
    p.response_encoding = Some("zstd".into());
    writer.submit(&on(&dir.0), p);
    writer.flush();
    let rows = all_rows(&dir.0);
    assert_eq!(
        rows[0].response_body.as_deref(),
        Some("event: message_start\ndata: {}\n\n")
    );
}

#[test]
fn age_rotation_happens_while_rows_keep_arriving() {
    let dir = TempDir::new();
    let clock = Clock::new("2026-11-09T08:00:00Z");
    let writer = Writer::with_limits(clock.limits());
    writer.submit(&on(&dir.0), pending(&writer, "a", b"x", &[]));
    writer.tick();
    clock.set("2026-11-09T08:10:01Z");
    writer.submit(&on(&dir.0), pending(&writer, "b", b"y", &[]));
    // A barrier that runs no timers: only the row itself can trigger the rotation.
    writer.sync();
    assert_eq!(parquet_files(&dir.0).len(), 1);
}

#[test]
fn a_body_cut_inside_a_character_stays_text() {
    let dir = TempDir::new();
    let writer = Writer::with_limits(Limits::default());
    let mut p = PendingRow::new(row("a"), writer.budget());
    // 'é' is two bytes; the cap falls between them.
    let mut body = vec![b'a'; (BODY_CAP - 1) as usize];
    body.extend_from_slice("é".as_bytes());
    p.response.push_bytes(&Bytes::from(body));
    writer.submit(&on(&dir.0), p);
    writer.flush();
    let rows = all_rows(&dir.0);
    assert!(rows[0].response_truncated);
    assert_eq!(rows[0].response_body_raw, None);
    assert_eq!(
        rows[0].response_body.as_ref().map(String::len),
        Some((BODY_CAP - 1) as usize)
    );
}
