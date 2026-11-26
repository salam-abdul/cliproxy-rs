use super::*;
use crate::request_logging::history::test_support::TempDir;
use std::time::Duration;

#[test]
fn retention_deletes_oldest_files_by_partition_and_name() {
    let dir = TempDir::new();
    let names = [
        "date=2026-11-01/080000-1-0000.parquet",
        "date=2026-11-01/090000-1-0001.parquet",
        "date=2026-11-02/070000-1-0000.parquet",
        "date=2026-11-02/080000-1-0001.parquet",
    ];
    let now = std::time::SystemTime::now();
    for (i, name) in names.iter().enumerate() {
        let path = dir.0.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![0u8; 100]).unwrap();
        // Newest mtime on the oldest name: retention must not order by mtime.
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(now - Duration::from_secs(i as u64 * 60)).unwrap();
    }
    let unrelated = dir.0.join("date=2026-11-01/notes.txt");
    std::fs::write(&unrelated, vec![0u8; 1000]).unwrap();
    let (freed, removed) = HistoryDir::new(&dir.0).enforce_cap(250).unwrap();
    assert_eq!(freed, 200);
    assert_eq!(removed, vec![dir.0.join(names[0]), dir.0.join(names[1])]);
    assert!(!dir.0.join(names[0]).exists());
    assert!(!dir.0.join(names[1]).exists());
    assert!(dir.0.join(names[2]).exists());
    assert!(dir.0.join(names[3]).exists());
    assert!(unrelated.exists());
}

#[test]
fn retention_removes_emptied_partitions() {
    let dir = TempDir::new();
    let old = dir.0.join("date=2026-10-01/080000-1-0000.parquet");
    let new = dir.0.join("date=2026-11-01/080000-1-0000.parquet");
    for path in [&old, &new] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![0u8; 100]).unwrap();
    }
    HistoryDir::new(&dir.0).enforce_cap(150).unwrap();
    assert!(!old.parent().unwrap().exists());
    assert!(new.exists());
}

#[test]
fn retention_on_a_folder_not_made_yet_does_nothing_quietly() {
    let dir = TempDir::new();
    let logs = crate::request_logging::history::test_support::Logs::capture();
    assert!(HistoryDir::new(&dir.0).apply_cap(1).is_empty());
    assert_eq!(logs.text(), "");
    assert!(!dir.0.exists());
}

#[test]
fn files_skips_a_partition_or_file_removed_while_it_lists() {
    let dir = TempDir::new();
    let names = [
        "date=2026-11-01/080000-1-0000.parquet",
        "date=2026-11-02/080000-1-0000.parquet",
        "date=2026-11-02/090000-1-0001.parquet",
        "date=2026-11-03/080000-1-0000.parquet",
    ];
    for name in names {
        let path = dir.0.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"x").unwrap();
    }
    // A delete removes the first partition and a cap removes one file just before the
    // listing reads them.
    let gone_partition = dir.0.join("date=2026-11-01");
    let gone_file = dir.0.join(names[1]);
    set_before_read(Some(Box::new(move |path: &Path| {
        if path == gone_partition {
            let _ = std::fs::remove_dir_all(path);
        }
        if path == gone_file {
            let _ = std::fs::remove_file(path);
        }
    })));
    let listed = HistoryDir::new(&dir.0).files();
    set_before_read(None);
    let listed: Vec<PathBuf> = listed.unwrap().into_iter().map(|f| f.path).collect();
    assert_eq!(listed, vec![dir.0.join(names[2]), dir.0.join(names[3])]);
}
