use std::path::{Path, PathBuf};

use super::*;
use crate::request_logging::history::file::{ParquetFile, rewrite_without};
use crate::request_logging::history::row::Row;
use crate::request_logging::history::test_support::{Logs, TempDir, claude_row, indexed_calls};
use crate::request_logging::history::tools::store::INDEX_FILE;

const WAIT: Duration = Duration::from_secs(10);

fn at(rfc3339: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(rfc3339).unwrap().with_timezone(&Utc)
}

fn settings(dir: &Path, secrets: &[&str]) -> Settings {
    Settings {
        dir: dir.to_path_buf(),
        max_total_bytes: None,
        alias_prefixes: secrets.iter().map(|s| cpa_exec::claude::alias_prefix(s)).collect(),
    }
}

fn indexer() -> Indexer {
    Indexer::new(Arc::new(Utc::now))
}

fn write(dir: &Path, opened: &str, rows: &[Row]) -> PathBuf {
    let mut file = ParquetFile::create(dir, at(opened), 0).unwrap();
    file.write_group(rows.to_vec()).unwrap();
    file.finish().unwrap()
}

fn rel(dir: &Path, path: &Path) -> String {
    rel_path(dir, path).unwrap()
}

fn run(indexer: &Indexer, settings: &Settings) {
    indexer.nudge(settings);
    indexer.sync();
}

fn us(rfc3339: &str) -> i64 {
    at(rfc3339).timestamp_micros()
}

/// A continuation of the send `parent` that calls `Bash` as `call`.
fn continuation(id: &str, parent: &str, started: &str, call: &str) -> Row {
    Row {
        exchange_id: id.into(),
        session_id: Some("s1".into()),
        started_at_us: us(started),
        provider: Some("claude".into()),
        request_body: Some(format!(
            r#"{{"thread":{{"type":"continue","previous_message_id":"msg_{parent}"}},"messages":[{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_a0","content":"ok"}}]}}]}}"#
        )),
        response_body: Some(format!(
            r#"{{"id":"msg_{id}","content":[{{"type":"tool_use","id":"{call}","name":"Bash","input":{{}}}}]}}"#
        )),
        ..Row::default()
    }
}

fn conv_of(dir: &Path, call: &str) -> Option<i64> {
    let conn = rusqlite::Connection::open(dir.join(INDEX_FILE)).unwrap();
    conn.query_row(
        "SELECT conv FROM calls WHERE id = ?1",
        [crate::request_logging::history::tools::hash(call.as_bytes())],
        |r| r.get(0),
    )
    .unwrap()
}

fn indexed_rels(dir: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(dir.join(INDEX_FILE)).unwrap();
    let mut stmt = conn.prepare("SELECT rel FROM files ORDER BY rel").unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn calls(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(c, t)| (c.to_string(), t.to_string())).collect()
}

#[test]
fn catch_up_indexes_oldest_first() {
    let dir = TempDir::new();
    let older = write(
        &dir.0,
        "2026-11-24T08:00:00Z",
        &[claude_row("p", "s1", us("2026-11-24T08:00:00Z"), "Read", "toolu_a0")],
    );
    write(
        &dir.0,
        "2026-11-24T09:00:00Z",
        &[continuation("q", "p", "2026-11-24T09:00:00Z", "toolu_b0")],
    );
    let settings = settings(&dir.0, &[]);
    let first = indexer();
    run(&first, &settings);
    let conv = conv_of(&dir.0, "toolu_a0");
    assert!(conv.is_some());
    assert_eq!(conv_of(&dir.0, "toolu_b0"), conv);
    first.shutdown(WAIT);

    // A fresh index whose first commit of the older file is refused: the pass ends
    // there, and the retry indexes both files with the same result.
    std::fs::remove_file(dir.0.join(INDEX_FILE)).unwrap();
    let _ = std::fs::remove_file(dir.0.join(format!("{INDEX_FILE}-wal")));
    let _ = std::fs::remove_file(dir.0.join(format!("{INDEX_FILE}-shm")));
    let second = Indexer::with_retry(Arc::new(Utc::now), Duration::from_millis(50));
    let (reached, release) = second.pause_before_commit();
    second.nudge(&settings);
    assert_eq!(reached.recv_timeout(WAIT).unwrap(), rel(&dir.0, &older));
    let file = File::options().write(true).open(&older).unwrap();
    file.set_modified(std::time::SystemTime::now() + Duration::from_secs(60))
        .unwrap();
    drop(file);
    release.send(()).unwrap();
    second.sync();
    assert!(
        indexed_rels(&dir.0).is_empty(),
        "the pass went on after a refused commit"
    );
    // The retry runs when the wait for a message times out, with no nudge.
    let started = Instant::now();
    while indexed_rels(&dir.0).len() < 2 && started.elapsed() < WAIT {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(indexed_rels(&dir.0).len(), 2);
    assert_eq!(conv_of(&dir.0, "toolu_a0"), conv);
    assert_eq!(conv_of(&dir.0, "toolu_b0"), conv);
}

#[test]
fn unreadable_file_does_not_stop_newer_files() {
    let dir = TempDir::new();
    let partition = dir.0.join("date=2026-11-24");
    std::fs::create_dir_all(&partition).unwrap();
    std::fs::write(partition.join("070000-1-0000.parquet"), b"not parquet").unwrap();
    write(
        &dir.0,
        "2026-11-24T08:00:00Z",
        &[claude_row("p", "s1", us("2026-11-24T08:00:00Z"), "Read", "toolu_a0")],
    );
    run(&indexer(), &settings(&dir.0, &[]));
    assert_eq!(indexed_calls(&dir.0), calls(&[("toolu_a0", "Read")]));
    let conn = rusqlite::Connection::open(dir.0.join(INDEX_FILE)).unwrap();
    let unreadable: Vec<String> = conn
        .prepare("SELECT rel FROM files WHERE unreadable")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(unreadable, vec!["date=2026-11-24/070000-1-0000.parquet".to_owned()]);
}

#[test]
fn rewritten_file_is_indexed_again() {
    let dir = TempDir::new();
    let path = write(
        &dir.0,
        "2026-11-24T08:00:00Z",
        &[
            claude_row("p", "s1", us("2026-11-24T08:00:00Z"), "Read", "toolu_a0"),
            claude_row("q", "s2", us("2026-11-24T08:00:01Z"), "Bash", "toolu_b0"),
        ],
    );
    let indexer = indexer();
    let settings = settings(&dir.0, &[]);
    run(&indexer, &settings);
    assert_eq!(
        indexed_calls(&dir.0),
        calls(&[("toolu_a0", "Read"), ("toolu_b0", "Bash")])
    );
    rewrite_without(&path, |row| row.session_id.as_deref() == Some("s1")).unwrap();
    run(&indexer, &settings);
    assert_eq!(indexed_calls(&dir.0), calls(&[("toolu_b0", "Bash")]));
}

#[test]
fn missing_file_loses_its_facts() {
    let dir = TempDir::new();
    let path = write(
        &dir.0,
        "2026-11-24T08:00:00Z",
        &[claude_row("p", "s1", us("2026-11-24T08:00:00Z"), "Read", "toolu_a0")],
    );
    let indexer = indexer();
    let settings = settings(&dir.0, &[]);
    run(&indexer, &settings);
    assert_eq!(indexed_calls(&dir.0).len(), 1);
    std::fs::remove_file(&path).unwrap();
    // A cap removal may still be on its way to the queue: one pass keeps the facts.
    run(&indexer, &settings);
    assert_eq!(indexed_calls(&dir.0).len(), 1);
    run(&indexer, &settings);
    assert!(indexed_calls(&dir.0).is_empty());
    assert!(indexed_rels(&dir.0).is_empty());
}

#[test]
fn a_cap_reported_one_pass_late_keeps_the_facts() {
    let dir = TempDir::new();
    let path = write(
        &dir.0,
        "2026-11-24T08:00:00Z",
        &[claude_row("p", "s1", us("2026-11-24T08:00:00Z"), "Read", "toolu_a0")],
    );
    let indexer = indexer();
    let settings = settings(&dir.0, &[]);
    run(&indexer, &settings);
    std::fs::remove_file(&path).unwrap();
    run(&indexer, &settings);
    indexer.capped(&dir.0, rel(&dir.0, &path), true);
    indexer.sync();
    run(&indexer, &settings);
    run(&indexer, &settings);
    assert_eq!(indexed_calls(&dir.0).len(), 1);
    assert!(capped_at(&dir.0, &rel(&dir.0, &path)).is_some());
}

fn capped_at(dir: &Path, rel: &str) -> Option<i64> {
    let conn = rusqlite::Connection::open(dir.join(INDEX_FILE)).unwrap();
    conn.query_row("SELECT capped_ms FROM files WHERE rel = ?1", [rel], |r| r.get(0))
        .unwrap()
}

#[test]
fn an_index_open_failure_keeps_the_queue() {
    let dir = TempDir::new();
    let path = write(
        &dir.0,
        "2026-11-24T08:00:00Z",
        &[claude_row("p", "s1", us("2026-11-24T08:00:00Z"), "Read", "toolu_a0")],
    );
    let indexer = indexer();
    let settings = settings(&dir.0, &[]);
    indexer.pass_here(&settings);
    std::fs::remove_file(&path).unwrap();
    let index = dir.0.join(INDEX_FILE);
    let aside = dir.0.join("index-aside");
    std::fs::rename(&index, &aside).unwrap();
    std::fs::create_dir(&index).unwrap();
    let rel = rel(&dir.0, &path);
    indexer.capped(&dir.0, rel.clone(), false);
    indexer.pass_here(&settings);
    assert_eq!(indexer.queued(), 1, "the failed pass dropped its queue");
    std::fs::remove_dir(&index).unwrap();
    std::fs::rename(&aside, &index).unwrap();
    indexer.pass_here(&settings);
    assert_eq!(indexer.queued(), 0);
    assert!(capped_at(&dir.0, &rel).is_some());
}

#[test]
fn queued_work_without_a_wake_waits_for_the_next_pass() {
    let dir = TempDir::new();
    two_sessions(&dir.0);
    let indexer = indexer();
    let settings = settings(&dir.0, &[]);
    run(&indexer, &settings);
    indexer.forget(&dir.0, "date=2026-11-24/080000-0-0000.parquet".into(), false);
    indexer.capped(&dir.0, "date=2026-11-24/090000-0-0000.parquet".into(), false);
    indexer.sync();
    assert_eq!(indexer.queued(), 2, "work queued without a wake started a pass");
    indexer.forget(&dir.0, "date=2026-11-24/100000-0-0000.parquet".into(), true);
    indexer.sync();
    assert_eq!(indexer.queued(), 0);
}

#[test]
fn queued_work_for_another_history_folder_is_dropped() {
    let dir = TempDir::new();
    let other = TempDir::new();
    let path = two_sessions(&dir.0);
    let indexer = indexer();
    let settings = settings(&dir.0, &[]);
    run(&indexer, &settings);
    std::fs::remove_file(&path).unwrap();
    // A cap removal reported for the same name in another folder does not keep this
    // folder's facts.
    indexer.capped(&other.0, rel(&dir.0, &path), false);
    run(&indexer, &settings);
    run(&indexer, &settings);
    assert_eq!(indexer.queued(), 0);
    assert!(indexed_calls(&dir.0).is_empty(), "work for another folder was applied");
}

#[test]
fn cap_removed_file_keeps_facts_for_31_days() {
    let dir = TempDir::new();
    let path = write(
        &dir.0,
        "2026-11-24T08:00:00Z",
        &[claude_row("p", "s1", us("2026-11-24T08:00:00Z"), "Read", "toolu_a0")],
    );
    let removed = at("2026-11-25T00:00:00Z");
    let clock = Arc::new(Mutex::new(removed));
    let now = Arc::clone(&clock);
    let indexer = Indexer::new(Arc::new(move || *now.lock().unwrap()));
    let settings = settings(&dir.0, &[]);
    run(&indexer, &settings);
    std::fs::remove_file(&path).unwrap();
    indexer.capped(&dir.0, rel(&dir.0, &path), true);
    run(&indexer, &settings);
    assert_eq!(indexed_calls(&dir.0).len(), 1);
    *clock.lock().unwrap() = removed + chrono::TimeDelta::milliseconds(2_678_400_000 - 1);
    run(&indexer, &settings);
    assert_eq!(indexed_calls(&dir.0).len(), 1);
    *clock.lock().unwrap() = removed + chrono::TimeDelta::milliseconds(2_678_400_000);
    run(&indexer, &settings);
    assert!(indexed_calls(&dir.0).is_empty());
}

/// A file with a row of session `s1` (call `toolu_a0`) and one of `s2` (`toolu_b0`).
fn two_sessions(dir: &Path) -> PathBuf {
    write(
        dir,
        "2026-11-24T08:00:00Z",
        &[
            claude_row("p", "s1", us("2026-11-24T08:00:00Z"), "Read", "toolu_a0"),
            claude_row("q", "s2", us("2026-11-24T08:00:01Z"), "Bash", "toolu_b0"),
        ],
    )
}

#[test]
fn delete_during_read_leaves_no_facts() {
    // The file rewritten between its read and its commit.
    let dir = TempDir::new();
    let path = two_sessions(&dir.0);
    let settings = settings(&dir.0, &[]);
    let indexer = indexer();
    let (reached, release) = indexer.pause_before_commit();
    indexer.nudge(&settings);
    reached.recv_timeout(WAIT).unwrap();
    rewrite_without(&path, |row| row.session_id.as_deref() == Some("s1")).unwrap();
    release.send(()).unwrap();
    indexer.sync();
    assert!(!indexed_calls(&dir.0).iter().any(|(call, _)| call == "toolu_a0"));
    run(&indexer, &settings);
    assert_eq!(indexed_calls(&dir.0), calls(&[("toolu_b0", "Bash")]));

    // The file removed between its read and its commit.
    let dir = TempDir::new();
    let path = two_sessions(&dir.0);
    let settings = self::settings(&dir.0, &[]);
    let indexer = self::indexer();
    let (reached, release) = indexer.pause_before_commit();
    indexer.nudge(&settings);
    reached.recv_timeout(WAIT).unwrap();
    std::fs::remove_file(&path).unwrap();
    release.send(()).unwrap();
    indexer.sync();
    assert!(indexed_calls(&dir.0).is_empty());
    run(&indexer, &settings);
    assert!(indexed_calls(&dir.0).is_empty());

    // `cliproxy history delete` run while the commit transaction is held: the commit
    // checked the file before the delete rewrote it, and the delete's forget waits for
    // the commit and then drops what it wrote.
    let dir = TempDir::new();
    two_sessions(&dir.0);
    let settings = self::settings(&dir.0, &[]);
    let indexer = self::indexer();
    let (reached, release) = indexer.pause_in_commit();
    indexer.nudge(&settings);
    reached.recv_timeout(WAIT).unwrap();
    let history = dir.0.clone();
    let delete = std::thread::spawn(move || {
        let selector = crate::request_logging::history::delete::Selector {
            session: Some("s1".into()),
            ..Default::default()
        };
        crate::request_logging::history::delete::delete(&history, &selector, false)
    });
    // Lets the delete reach its forget, which waits on the held transaction; the
    // outcome is the same if it has not.
    std::thread::sleep(Duration::from_millis(200));
    release.send(()).unwrap();
    let report = delete.join().unwrap().unwrap();
    assert_eq!(report.rows_deleted, 1);
    indexer.sync();
    assert!(!indexed_calls(&dir.0).iter().any(|(call, _)| call == "toolu_a0"));
    run(&indexer, &settings);
    assert_eq!(indexed_calls(&dir.0), calls(&[("toolu_b0", "Bash")]));
}

#[test]
fn nudge_never_blocks() {
    let dir = TempDir::new();
    two_sessions(&dir.0);
    let settings = settings(&dir.0, &[]);
    let indexer = indexer();
    let (reached, release) = indexer.pause_before_commit();
    indexer.nudge(&settings);
    reached.recv_timeout(WAIT).unwrap();
    let started = Instant::now();
    for _ in 0..5 {
        indexer.nudge(&settings);
        indexer.forget(&dir.0, "date=2026-11-24/000000-1-0000.parquet".into(), true);
    }
    assert!(started.elapsed() < Duration::from_secs(1));
    drop(release);
    indexer.sync();
    assert_eq!(indexed_calls(&dir.0).len(), 2);
}

#[test]
fn latest_settings_win_over_a_merged_nudge() {
    let dir = TempDir::new();
    two_sessions(&dir.0);
    let indexer = indexer();
    let (reached, release) = indexer.pause_before_commit();
    indexer.nudge(&settings(&dir.0, &[]));
    reached.recv_timeout(WAIT).unwrap();
    // Written while the first pass is held, so only a later pass reads it.
    let alias = format!("{}abandon_Grep", cpa_exec::claude::alias_prefix("k1"));
    write(
        &dir.0,
        "2026-11-24T09:00:00Z",
        &[claude_row("r", "s3", us("2026-11-24T09:00:00Z"), &alias, "toolu_c0")],
    );
    indexer.nudge(&settings(&dir.0, &[]));
    indexer.nudge(&settings(&dir.0, &["k1"]));
    drop(release);
    indexer.sync();
    assert!(indexed_calls(&dir.0).contains(&("toolu_c0".to_owned(), "Grep".to_owned())));
}

#[test]
fn shutdown_stops_the_indexer() {
    let dir = TempDir::new();
    two_sessions(&dir.0);
    let settings = settings(&dir.0, &[]);
    let indexer = Arc::new(indexer());
    let (reached, release) = indexer.pause_before_commit();
    indexer.nudge(&settings);
    reached.recv_timeout(WAIT).unwrap();
    let watched = Arc::clone(&indexer);
    let releaser = std::thread::spawn(move || {
        while !watched.stopping() {
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(release);
    });
    indexer.shutdown(WAIT);
    releaser.join().unwrap();
    assert!(!indexer.is_running());
    assert!(indexed_rels(&dir.0).is_empty(), "a file was committed after shutdown");
    indexer.nudge(&settings);
    assert!(!indexer.is_running());
}

#[test]
fn pass_error_log_has_no_body_text() {
    let dir = TempDir::new();
    let mut secret = claude_row("p", "s1", us("2026-11-24T08:00:00Z"), "Read", "toolu_a0");
    secret.request_body = Some(r#"{"messages":[{"role":"user","content":"SYNTHETIC-SECRET"}]}"#.into());
    write(&dir.0, "2026-11-24T08:00:00Z", &[secret]);
    let partition = dir.0.join("date=2026-11-24");
    std::fs::write(partition.join("070000-1-0000.parquet"), b"SYNTHETIC-SECRET").unwrap();
    let settings = settings(&dir.0, &[]);
    let indexer = indexer();
    let logs = Logs::capture();
    indexer.pass_here(&settings);
    assert_eq!(logs.text(), "");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(dir.0.join(format!("{INDEX_FILE}{suffix}")));
    }
    std::fs::create_dir(dir.0.join(INDEX_FILE)).unwrap();
    indexer.pass_here(&settings);
    let text = logs.text();
    assert_eq!(text.matches("history: tool index pass stopped").count(), 1, "{text}");
    assert!(!text.contains("SYNTHETIC-SECRET"), "{text}");
    assert!(!text.contains(&*dir.0.to_string_lossy()), "{text}");
}
