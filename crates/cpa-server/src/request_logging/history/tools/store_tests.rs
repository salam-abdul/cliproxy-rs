use std::path::PathBuf;

use super::*;
use crate::request_logging::history::test_support::TempDir;
use crate::request_logging::history::tools::scan::{Call, Conv, Def, Outcome};

const DAY: &str = "date=2026-11-24";

/// A stand-in history file: the index only fingerprints files, it never reads them.
fn file(dir: &Path, name: &str) -> (String, PathBuf) {
    let partition = dir.join(DAY);
    std::fs::create_dir_all(&partition).unwrap();
    let path = partition.join(name);
    std::fs::write(&path, b"synthetic").unwrap();
    (format!("{DAY}/{name}"), path)
}

fn send(at_ms: i64, conv: Conv, message: Option<i64>) -> SendFacts {
    SendFacts {
        at_ms,
        provider: Some("claude".into()),
        conv,
        message,
        defs: Vec::new(),
        calls: Vec::new(),
        results: Vec::new(),
        skipped_format: false,
        cut: false,
    }
}

fn with_tools(mut facts: SendFacts, tool: &str, call: i64) -> SendFacts {
    facts.defs.push(Def {
        tool: tool.into(),
        bytes: 40,
    });
    facts.calls.push(Call {
        id: call,
        tool: tool.into(),
        skill: None,
    });
    facts.results.push(Outcome {
        id: call,
        is_error: false,
        bytes: 12,
        loaded: 0,
    });
    facts
}

fn commit(index: &mut Index, rel: &str, path: &Path, sends: &[SendFacts]) -> bool {
    let read_as = Fingerprint::from_path(path).unwrap();
    index.commit(rel, &read_as, path, sends, false).unwrap()
}

fn count(index: &Index, table: &str, rel: &str) -> i64 {
    index
        .conn
        .query_row(&format!("SELECT COUNT(*) FROM {table} WHERE rel = ?1"), [rel], |r| {
            r.get(0)
        })
        .unwrap()
}

fn total(index: &Index, table: &str) -> i64 {
    index
        .conn
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn convs(index: &Index, rel: &str) -> Vec<Option<i64>> {
    let mut stmt = index
        .conn
        .prepare("SELECT conv FROM sends WHERE rel = ?1 ORDER BY rowid")
        .unwrap();
    stmt.query_map([rel], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn commit_resolves_continuations_across_files() {
    let dir = TempDir::new();
    let mut index = Index::open_or_create(&dir.0);
    let (a, a_path) = file(&dir.0, "080000-1-0000.parquet");
    let (b, b_path) = file(&dir.0, "090000-1-0000.parquet");
    assert!(commit(
        &mut index,
        &a,
        &a_path,
        &[with_tools(send(1, Conv::Start(10), Some(100)), "Read", 1)]
    ));
    assert!(commit(
        &mut index,
        &b,
        &b_path,
        &[
            with_tools(send(2, Conv::Continue(100), Some(200)), "Bash", 2),
            send(3, Conv::Continue(200), Some(300)),
            with_tools(send(4, Conv::Continue(999), Some(400)), "Grep", 4),
            send(5, Conv::None, None),
        ]
    ));
    assert_eq!(convs(&index, &b), vec![Some(10), Some(10), None, None]);
    let call_conv = |id: i64| -> Option<i64> {
        index
            .conn
            .query_row("SELECT conv FROM calls WHERE id = ?1", [id], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(call_conv(2), Some(10));
    assert_eq!(call_conv(4), None);
    // Definitions without a conversation are not kept; the message of a send without
    // one is not either.
    assert_eq!(count(&index, "defs", &b), 1);
    assert_eq!(count(&index, "messages", &b), 2);
    // Committing B again replaces its facts.
    assert!(commit(&mut index, &b, &b_path, &[send(2, Conv::Continue(100), None)]));
    assert_eq!(convs(&index, &b), vec![Some(10)]);
    assert_eq!(count(&index, "calls", &b), 0);
}

#[test]
fn duplicate_calls_survive_forgetting_one_file() {
    let dir = TempDir::new();
    let mut index = Index::open_or_create(&dir.0);
    let (a, a_path) = file(&dir.0, "080000-1-0000.parquet");
    let (b, b_path) = file(&dir.0, "090000-1-0000.parquet");
    let resend = with_tools(send(1, Conv::Start(10), Some(100)), "Read", 5);
    assert!(commit(&mut index, &a, &a_path, std::slice::from_ref(&resend)));
    assert!(commit(&mut index, &b, &b_path, &[resend]));
    assert_eq!(total(&index, "calls"), 2);
    index.forget(&a).unwrap();
    assert_eq!(total(&index, "calls"), 1);
    assert_eq!(total(&index, "results"), 1);
    assert_eq!(count(&index, "calls", &b), 1);
    assert_eq!(count(&index, "results", &b), 1);
}

#[test]
fn commit_refuses_a_changed_or_removed_file() {
    let dir = TempDir::new();
    let mut index = Index::open_or_create(&dir.0);
    let (a, a_path) = file(&dir.0, "080000-1-0000.parquet");
    let read_as = Fingerprint::from_path(&a_path).unwrap();
    std::fs::write(&a_path, b"rewritten by a delete").unwrap();
    let facts = [with_tools(send(1, Conv::Start(10), Some(100)), "Read", 1)];
    assert!(!index.commit(&a, &read_as, &a_path, &facts, false).unwrap());
    assert_eq!(count(&index, "files", &a), 0);
    assert_eq!(count(&index, "calls", &a), 0);

    let read_as = Fingerprint::from_path(&a_path).unwrap();
    std::fs::remove_file(&a_path).unwrap();
    assert!(!index.commit(&a, &read_as, &a_path, &facts, false).unwrap());
    assert_eq!(total(&index, "files"), 0);
    assert_eq!(total(&index, "sends"), 0);
}

#[test]
fn forget_drops_every_fact_of_a_file() {
    let dir = TempDir::new();
    let mut index = Index::open_or_create(&dir.0);
    let (a, a_path) = file(&dir.0, "080000-1-0000.parquet");
    let (b, b_path) = file(&dir.0, "090000-1-0000.parquet");
    assert!(commit(
        &mut index,
        &a,
        &a_path,
        &[with_tools(send(1, Conv::Start(10), Some(100)), "Read", 1)]
    ));
    assert!(commit(
        &mut index,
        &b,
        &b_path,
        &[with_tools(send(2, Conv::Start(20), Some(200)), "Bash", 2)]
    ));
    index.forget(&a).unwrap();
    for table in ["files", "sends", "messages", "defs", "calls", "results"] {
        assert_eq!(count(&index, table, &a), 0, "{table}");
        assert_eq!(count(&index, table, &b), 1, "{table}");
    }
    assert!(!index.indexed().unwrap().contains_key(&a));
}

#[test]
fn sweep_forgets_missing_files_and_keeps_capped_ones_for_31_days() {
    let dir = TempDir::new();
    let mut index = Index::open_or_create(&dir.0);
    let (a, a_path) = file(&dir.0, "080000-1-0000.parquet");
    let (b, b_path) = file(&dir.0, "090000-1-0000.parquet");
    let (c, c_path) = file(&dir.0, "100000-1-0000.parquet");
    for (rel, path, conv) in [(&a, &a_path, 1), (&b, &b_path, 2), (&c, &c_path, 3)] {
        assert!(commit(&mut index, rel, path, &[send(conv, Conv::Start(conv), None)]));
    }
    let removed = 1_764_000_000_000;
    index.capped(&b, removed).unwrap();
    let on_disk: HashSet<String> = [c.clone()].into();
    // A missing file is forgotten at the second sweep that finds it missing.
    let missing = index.sweep(&on_disk, removed, &HashSet::new()).unwrap();
    assert_eq!(missing, [a.clone()].into());
    assert!(index.indexed().unwrap().contains_key(&a));
    let missing = index.sweep(&on_disk, removed, &missing).unwrap();
    assert!(missing.is_empty());
    let indexed = index.indexed().unwrap();
    assert!(!indexed.contains_key(&a));
    assert_eq!(indexed.get(&b).map(|(_, capped)| *capped), Some(Some(removed)));
    assert_eq!(indexed.get(&c).map(|(_, capped)| *capped), Some(None));
    assert_eq!(count(&index, "sends", &b), 1);

    index
        .sweep(&on_disk, removed + RETAIN_CAPPED_MS - 1, &HashSet::new())
        .unwrap();
    assert_eq!(count(&index, "sends", &b), 1);
    index
        .sweep(&on_disk, removed + RETAIN_CAPPED_MS, &HashSet::new())
        .unwrap();
    assert_eq!(count(&index, "sends", &b), 0);
    assert!(!index.indexed().unwrap().contains_key(&b));
    assert_eq!(count(&index, "sends", &c), 1);
}

#[test]
fn unreadable_file_is_recorded() {
    let dir = TempDir::new();
    let mut index = Index::open_or_create(&dir.0);
    let (a, a_path) = file(&dir.0, "080000-1-0000.parquet");
    let read_as = Fingerprint::from_path(&a_path).unwrap();
    assert!(index.commit(&a, &read_as, &a_path, &[], true).unwrap());
    let unreadable: bool = index
        .conn
        .query_row("SELECT unreadable FROM files WHERE rel = ?1", [&a], |r| r.get(0))
        .unwrap();
    assert!(unreadable);
    assert_eq!(index.indexed().unwrap().get(&a).map(|(fp, _)| *fp), Some(read_as));
    assert_eq!(count(&index, "sends", &a), 0);
}

#[test]
fn open_existing_creates_no_index_and_lists_capped_files() {
    let dir = TempDir::new();
    let (a, a_path) = file(&dir.0, "080000-1-0000.parquet");
    let (b, b_path) = file(&dir.0, "090000-1-0000.parquet");
    assert!(open_existing(&dir.0).unwrap().is_none());
    assert!(!dir.0.join(INDEX_FILE).exists());
    let mut index = Index::open_or_create(&dir.0);
    assert!(commit(&mut index, &a, &a_path, &[send(1, Conv::Start(1), None)]));
    assert!(commit(&mut index, &b, &b_path, &[send(2, Conv::Start(2), None)]));
    index.capped(&a, 5).unwrap();
    let existing = open_existing(&dir.0).unwrap().unwrap();
    assert_eq!(existing.capped_files().unwrap(), vec![a]);
}

#[test]
fn an_index_from_another_version_is_emptied() {
    let dir = TempDir::new();
    let (a, a_path) = file(&dir.0, "080000-1-0000.parquet");
    let mut index = Index::open_or_create(&dir.0);
    assert!(commit(&mut index, &a, &a_path, &[send(1, Conv::Start(1), None)]));
    assert!(is_current(&index.conn).unwrap());
    index.conn.pragma_update(None, "user_version", VERSION - 1).unwrap();
    assert!(!is_current(&index.conn).unwrap());
    drop(index);
    let index = Index::open(&dir.0).unwrap();
    assert!(is_current(&index.conn).unwrap());
    assert_eq!(total(&index, "files"), 0);
    assert_eq!(total(&index, "sends"), 0);
}

#[cfg(unix)]
#[test]
fn index_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new();
    let _index = Index::open_or_create(&dir.0);
    let mode = std::fs::metadata(dir.0.join(INDEX_FILE)).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

impl Index {
    fn open_or_create(dir: &Path) -> Index {
        std::fs::create_dir_all(dir).unwrap();
        Index::open(dir).unwrap()
    }
}
