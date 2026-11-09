use super::*;
use crate::request_logging::history::file::tests::TempDir;
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
    let freed = enforce_cap(&dir.0, 250).unwrap();
    assert_eq!(freed, 200);
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
    enforce_cap(&dir.0, 150).unwrap();
    assert!(!old.parent().unwrap().exists());
    assert!(new.exists());
}
