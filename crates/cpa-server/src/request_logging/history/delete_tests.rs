use super::*;
use crate::request_logging::history::file::tests::TempDir;
use crate::request_logging::history::file::{ParquetFile, Source, read_rows};
use crate::request_logging::history::writer::{Limits, PendingRow, Writer};

fn at(rfc3339: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(rfc3339).unwrap().with_timezone(&Utc)
}

fn row(id: &str, session: &str, started: &str) -> Row {
    Row {
        exchange_id: id.into(),
        session_id: Some(session.into()),
        account: Some("a@example.invalid".into()),
        model: Some("claude-sonnet-4-6".into()),
        model_requested: Some("sonnet".into()),
        started_at_us: at(started).timestamp_micros(),
        source: Source::Proxy,
        request_body: Some(format!("body of {id}")),
        ..Row::default()
    }
}

fn write_file(dir: &Path, opened: &str, seq: u32, rows: &[Row]) -> PathBuf {
    let mut file = ParquetFile::create(dir, at(opened), seq).unwrap();
    file.write_group(rows).unwrap();
    file.finish().unwrap()
}

fn session(s: &str) -> Selector {
    Selector {
        session: Some(s.into()),
        ..Selector::default()
    }
}

fn ids(path: &Path) -> Vec<String> {
    read_rows(path).unwrap().into_iter().map(|r| r.exchange_id).collect()
}

#[test]
fn an_empty_selector_refuses() {
    let dir = TempDir::new();
    let only = write_file(
        &dir.0,
        "2026-11-09T08:00:00Z",
        0,
        &[row("a", "s1", "2026-11-09T08:00:00Z")],
    );
    let error = delete(&dir.0, &Selector::default(), false).unwrap_err();
    assert!(error.to_string().contains("--session"), "{error}");
    assert!(only.exists());
}

#[test]
fn whole_matching_files_are_deleted_and_partial_ones_rewritten() {
    let dir = TempDir::new();
    let whole = write_file(
        &dir.0,
        "2026-11-08T08:00:00Z",
        0,
        &[row("a", "s1", "2026-11-08T08:00:00Z")],
    );
    let mixed = write_file(
        &dir.0,
        "2026-11-09T08:00:00Z",
        1,
        &[
            row("b", "s1", "2026-11-09T08:00:00Z"),
            row("c", "s2", "2026-11-09T08:00:01Z"),
        ],
    );
    let untouched = write_file(
        &dir.0,
        "2026-11-09T09:00:00Z",
        2,
        &[row("d", "s2", "2026-11-09T09:00:00Z")],
    );
    let untouched_bytes = std::fs::read(&untouched).unwrap();

    let report = delete(&dir.0, &session("s1"), false).unwrap();

    assert_eq!(report.files_deleted, 1);
    assert_eq!(report.files_rewritten, 1);
    assert_eq!(report.rows_deleted, 2);
    assert!(report.bytes_freed > 0);
    assert!(report.pending_for_writer);
    assert!(!whole.exists());
    assert_eq!(ids(&mixed), ["c"]);
    assert_eq!(std::fs::read(&untouched).unwrap(), untouched_bytes);
    let leftovers: Vec<_> = std::fs::read_dir(mixed.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".partial"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn time_bounds_include_from_and_exclude_to() {
    let dir = TempDir::new();
    let file = write_file(
        &dir.0,
        "2026-11-09T08:00:00Z",
        0,
        &[
            row("before", "s", "2026-11-09T07:59:59Z"),
            row("from", "s", "2026-11-09T08:00:00Z"),
            row("to", "s", "2026-11-09T09:00:00Z"),
        ],
    );
    let selector = Selector {
        from: Some(at("2026-11-09T08:00:00Z")),
        to: Some(at("2026-11-09T09:00:00Z")),
        ..Selector::default()
    };
    let report = delete(&dir.0, &selector, false).unwrap();
    assert_eq!(report.rows_deleted, 1);
    assert_eq!(ids(&file), ["before", "to"]);
}

#[test]
fn a_row_in_a_later_partition_still_matches_its_start_time() {
    let dir = TempDir::new();
    // A send that started before midnight and finished after it lands in the next day's file.
    let file = write_file(
        &dir.0,
        "2026-11-02T00:00:05Z",
        0,
        &[row("late", "s", "2026-11-01T23:59:58Z")],
    );
    let selector = Selector {
        to: Some(at("2026-11-02T00:00:00Z")),
        ..Selector::default()
    };
    assert_eq!(delete(&dir.0, &selector, false).unwrap().rows_deleted, 1);
    assert!(!file.exists());
}

#[test]
fn model_matches_the_upstream_or_requested_model_and_account_is_exact() {
    let dir = TempDir::new();
    let file = write_file(
        &dir.0,
        "2026-11-09T08:00:00Z",
        0,
        &[row("a", "s", "2026-11-09T08:00:00Z")],
    );
    let by_alias = Selector {
        model: Some("sonnet".into()),
        account: Some("A@example.invalid".into()),
        ..Selector::default()
    };
    assert_eq!(delete(&dir.0, &by_alias, false).unwrap().rows_deleted, 0);
    let by_alias = Selector {
        account: Some("a@example.invalid".into()),
        ..by_alias
    };
    assert_eq!(delete(&dir.0, &by_alias, false).unwrap().rows_deleted, 1);
    assert!(!file.exists());
}

#[test]
fn dry_run_reports_without_deleting() {
    let dir = TempDir::new();
    let file = write_file(
        &dir.0,
        "2026-11-09T08:00:00Z",
        0,
        &[
            row("a", "s1", "2026-11-09T08:00:00Z"),
            row("b", "s2", "2026-11-09T08:00:00Z"),
        ],
    );
    let before = std::fs::read(&file).unwrap();
    let report = delete(&dir.0, &session("s1"), true).unwrap();
    assert_eq!(report.rows_deleted, 1);
    assert_eq!(report.files_rewritten, 1);
    assert_eq!(std::fs::read(&file).unwrap(), before);
    assert!(!dir.0.join(".deletes").exists());
}

fn on(dir: &Path) -> cpa_core::config::Config {
    crate::testing::config(&format!(
        "observability: {{history: {{enable: true, dir: '{}'}}}}",
        dir.display()
    ))
}

#[test]
fn a_tombstone_removes_rows_from_the_open_file() {
    let dir = TempDir::new();
    let writer = Writer::with_limits(Limits {
        group_rows: 1,
        ..Limits::default()
    });
    let cfg = on(&dir.0);
    // Group size 1: the first row is already in the open file's first row group.
    writer.submit(
        &cfg,
        PendingRow::new(row("a", "s1", "2026-11-09T08:00:00Z"), writer.budget()),
    );
    writer.tick();
    let report = delete(&dir.0, &session("s1"), false).unwrap();
    assert!(report.pending_for_writer);
    assert_eq!(report.rows_deleted, 0);
    writer.submit(
        &cfg,
        PendingRow::new(row("b", "s1", "2026-11-09T08:00:01Z"), writer.budget()),
    );
    writer.submit(
        &cfg,
        PendingRow::new(row("c", "s2", "2026-11-09T08:00:02Z"), writer.budget()),
    );
    writer.flush();
    let mut left = Vec::new();
    for partition in std::fs::read_dir(&dir.0).unwrap() {
        let partition = partition.unwrap().path();
        if partition.is_dir() && partition.file_name().unwrap() != ".deletes" {
            for file in std::fs::read_dir(partition).unwrap() {
                left.extend(ids(&file.unwrap().path()));
            }
        }
    }
    assert_eq!(left, ["c"]);
}

#[test]
fn delete_refuses_while_another_live_process_holds_the_lock() {
    let dir = TempDir::new();
    write_file(
        &dir.0,
        "2026-11-09T08:00:00Z",
        0,
        &[row("a", "s1", "2026-11-09T08:00:00Z")],
    );
    let _held = crate::request_logging::history::lock::DirLock::try_acquire(&dir.0)
        .unwrap()
        .unwrap();
    let error = delete(&dir.0, &session("s1"), false).unwrap_err();
    assert!(error.to_string().contains(".lock"), "{error}");
}

#[test]
fn a_stale_lock_is_taken_over() {
    let dir = TempDir::new();
    let file = write_file(
        &dir.0,
        "2026-11-09T08:00:00Z",
        0,
        &[row("a", "s1", "2026-11-09T08:00:00Z")],
    );
    let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" })
        .args(if cfg!(windows) { &["/C", "exit"][..] } else { &[][..] })
        .spawn()
        .unwrap();
    let dead = child.id();
    child.wait().unwrap();
    std::fs::write(dir.0.join(".lock"), dead.to_string()).unwrap();
    assert_eq!(delete(&dir.0, &session("s1"), false).unwrap().rows_deleted, 1);
    assert!(!file.exists());
}

#[test]
fn parse_time_accepts_dates_and_rfc3339() {
    assert_eq!(parse_time("2026-11-09").unwrap(), at("2026-11-09T00:00:00Z"));
    assert_eq!(
        parse_time("2026-11-09T10:00:00+02:00").unwrap(),
        at("2026-11-09T08:00:00Z")
    );
    assert!(parse_time("yesterday").is_err());
}

#[test]
fn a_tombstone_spares_rows_that_started_after_the_delete() {
    let dir = TempDir::new();
    let writer = Writer::with_limits(Limits {
        group_rows: 1,
        ..Limits::default()
    });
    let cfg = on(&dir.0);
    writer.submit(
        &cfg,
        PendingRow::new(row("a", "s1", "2026-11-09T08:00:00Z"), writer.budget()),
    );
    writer.tick();
    assert!(delete(&dir.0, &session("s1"), false).unwrap().pending_for_writer);
    let mut later = row("later", "s1", "2026-11-09T08:00:00Z");
    later.started_at_us = (Utc::now() + chrono::Duration::hours(1)).timestamp_micros();
    writer.submit(&cfg, PendingRow::new(later, writer.budget()));
    writer.flush();
    let mut left = Vec::new();
    for partition in std::fs::read_dir(&dir.0).unwrap() {
        let partition = partition.unwrap().path();
        if partition.is_dir() && partition.file_name().unwrap() != ".deletes" {
            for file in std::fs::read_dir(partition).unwrap() {
                left.extend(ids(&file.unwrap().path()));
            }
        }
    }
    assert_eq!(left, ["later"]);
}

fn left_ids(dir: &Path) -> Vec<String> {
    let mut left = Vec::new();
    for partition in std::fs::read_dir(dir).unwrap() {
        let partition = partition.unwrap().path();
        if partition.is_dir() && partition.file_name().unwrap().to_string_lossy().starts_with("date=") {
            for file in std::fs::read_dir(partition).unwrap() {
                let path = file.unwrap().path();
                if path.extension().is_some_and(|e| e == "parquet") {
                    left.extend(ids(&path));
                }
            }
        }
    }
    left.sort();
    left
}

#[test]
fn a_delete_covers_rows_a_server_still_buffers() {
    let dir = TempDir::new();
    // Default limits: rows wait in memory and no file exists yet.
    let writer = Writer::with_limits(Limits::default());
    let cfg = on(&dir.0);
    writer.submit(
        &cfg,
        PendingRow::new(row("a", "s1", "2026-11-09T08:00:00Z"), writer.budget()),
    );
    writer.tick();
    let report = delete(&dir.0, &session("s1"), false).unwrap();
    assert!(report.pending_for_writer);
    writer.submit(
        &cfg,
        PendingRow::new(row("b", "s1", "2026-11-09T08:00:01Z"), writer.budget()),
    );
    writer.submit(
        &cfg,
        PendingRow::new(row("c", "s2", "2026-11-09T08:00:02Z"), writer.budget()),
    );
    writer.flush();
    assert_eq!(left_ids(&dir.0), ["c"]);
}

#[test]
fn the_writer_rewrites_a_file_only_under_the_lock() {
    let dir = TempDir::new();
    let writer = Writer::with_limits(Limits {
        group_rows: 1,
        ..Limits::default()
    });
    let cfg = on(&dir.0);
    writer.submit(
        &cfg,
        PendingRow::new(row("a", "s1", "2026-11-09T08:00:00Z"), writer.budget()),
    );
    writer.tick();
    delete(&dir.0, &session("s1"), false).unwrap();
    let held = crate::request_logging::history::lock::DirLock::try_acquire(&dir.0)
        .unwrap()
        .unwrap();
    writer.flush();
    assert_eq!(left_ids(&dir.0), ["a"], "rewritten without the lock");
    drop(held);
    writer.tick();
    assert!(left_ids(&dir.0).is_empty());
}

#[test]
fn a_tombstone_outlives_the_first_rotation() {
    let dir = TempDir::new();
    let writer = Writer::with_limits(Limits::default());
    let cfg = on(&dir.0);
    writer.submit(
        &cfg,
        PendingRow::new(row("a", "s2", "2026-11-09T08:00:00Z"), writer.budget()),
    );
    writer.tick();
    delete(&dir.0, &session("s1"), false).unwrap();
    writer.flush();
    // A send that started before the delete but finished after this rotation.
    writer.submit(
        &cfg,
        PendingRow::new(row("late", "s1", "2026-11-09T08:00:01Z"), writer.budget()),
    );
    writer.flush();
    assert_eq!(left_ids(&dir.0), ["a"]);
}

#[test]
fn expired_tombstones_are_removed() {
    let dir = TempDir::new();
    std::fs::create_dir_all(&dir.0).unwrap();
    delete(&dir.0, &session("s1"), false).unwrap();
    assert_eq!(pending_tombstones(&dir.0).len(), 1);
    expire_tombstones(&dir.0, Utc::now() + chrono::Duration::hours(25));
    assert!(pending_tombstones(&dir.0).is_empty());
}

#[test]
fn a_delete_the_server_could_not_apply_is_applied_after_a_restart() {
    let dir = TempDir::new();
    let cfg = on(&dir.0);
    let writer = Writer::with_limits(Limits {
        group_rows: 1,
        ..Limits::default()
    });
    writer.submit(
        &cfg,
        PendingRow::new(row("a", "s1", "2026-11-09T08:00:00Z"), writer.budget()),
    );
    writer.tick();
    delete(&dir.0, &session("s1"), false).unwrap();
    // A delete holds the lock while the server shuts down, so it cannot rewrite.
    let held = crate::request_logging::history::lock::DirLock::try_acquire(&dir.0)
        .unwrap()
        .unwrap();
    writer.shutdown(std::time::Duration::from_secs(5));
    assert_eq!(left_ids(&dir.0), ["a"]);
    drop(held);
    let restarted = Writer::with_limits(Limits::default());
    restarted.submit(
        &cfg,
        PendingRow::new(row("b", "s2", "2026-11-09T09:00:00Z"), restarted.budget()),
    );
    restarted.flush();
    assert_eq!(left_ids(&dir.0), ["b"]);
}

#[test]
fn a_rewrite_keeps_the_other_row_groups() {
    use parquet::file::reader::FileReader;
    let dir = TempDir::new();
    let mut file = ParquetFile::create(&dir.0, at("2026-11-09T08:00:00Z"), 0).unwrap();
    file.write_group(&[row("a", "s2", "2026-11-09T08:00:00Z")]).unwrap();
    file.write_group(&[
        row("b", "s1", "2026-11-09T08:00:01Z"),
        row("c", "s2", "2026-11-09T08:00:02Z"),
    ])
    .unwrap();
    file.write_group(&[row("d", "s2", "2026-11-09T08:00:03Z")]).unwrap();
    let path = file.finish().unwrap();
    assert_eq!(delete(&dir.0, &session("s1"), false).unwrap().rows_deleted, 1);
    assert_eq!(ids(&path), ["a", "c", "d"]);
    let reader =
        parquet::file::serialized_reader::SerializedFileReader::new(std::fs::File::open(&path).unwrap()).unwrap();
    assert_eq!(reader.metadata().num_row_groups(), 3);
}

#[test]
fn the_command_spares_rows_that_started_after_it() {
    let dir = TempDir::new();
    let mut later = row("later", "s1", "2026-11-09T08:00:00Z");
    later.started_at_us = (Utc::now() + chrono::Duration::hours(1)).timestamp_micros();
    let file = write_file(
        &dir.0,
        "2026-11-09T08:00:00Z",
        0,
        &[row("a", "s1", "2026-11-09T08:00:00Z"), later],
    );
    assert_eq!(delete(&dir.0, &session("s1"), false).unwrap().rows_deleted, 1);
    assert_eq!(ids(&file), ["later"]);
}
