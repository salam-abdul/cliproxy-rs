use super::*;
use crate::activity::{Activity, DAY_MS, MAX_REQUESTS, MAX_SENDS, MAX_STATUSES, epoch_ms, prune, trim};
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

struct Store {
    dir: PathBuf,
    path: PathBuf,
}

impl Store {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("activity-quota-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("activity.sqlite");
        Self { dir, path }
    }

    fn start(&self) -> Activity {
        Activity::start(self.path.clone(), Arc::new(AtomicU64::new(0))).unwrap()
    }

    /// Every reading as (long, bucket_ms, used), oldest first.
    fn rows(&self) -> Vec<(bool, i64, f64)> {
        Connection::open(&self.path)
            .unwrap()
            .prepare("SELECT long, bucket_ms, used FROM quota_readings ORDER BY at_ms")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn short(used_percent: f64) -> Reading {
    Reading {
        long: false,
        used_percent,
        reset: None,
    }
}

#[test]
fn a_reading_is_kept_once_per_credential_window_and_ten_minutes() {
    let store = Store::new();
    let activity = store.start();
    let bucket = epoch_ms() / 600_000 * 600_000 - 3_600_000;
    let minute = 60_000;
    activity.record_readings("codex", "codex-a.json", bucket + minute, &[short(30.0)]);
    activity.record_readings("codex", "codex-a.json", bucket + 6 * minute, &[short(35.0)]);
    activity.flush();
    assert_eq!(store.rows(), [(false, bucket, 35.0)]);
    activity.record_readings("codex", "codex-a.json", bucket + 17 * minute, &[short(50.0)]);
    activity.flush();
    assert_eq!(store.rows(), [(false, bucket, 35.0), (false, bucket + 600_000, 50.0)]);
}

#[test]
fn readings_older_than_the_retention_or_past_the_cap_are_removed() {
    let store = Store::new();
    let activity = store.start();
    let now = epoch_ms();
    activity.record_readings("claude", "claude-a.json", now - 32 * DAY_MS, &[short(10.0)]);
    activity.record_readings("claude", "claude-a.json", now - 2 * DAY_MS, &[short(20.0)]);
    activity.record_readings("claude", "claude-a.json", now - DAY_MS, &[short(30.0)]);
    activity.flush();
    let conn = Connection::open(&store.path).unwrap();
    prune(&conn, now).unwrap();
    let used = |rows: Vec<(bool, i64, f64)>| rows.into_iter().map(|(_, _, used)| used).collect::<Vec<_>>();
    assert_eq!(used(store.rows()), [20.0, 30.0]);
    trim(&conn, MAX_REQUESTS, MAX_STATUSES, MAX_SENDS, 1).unwrap();
    assert_eq!(used(store.rows()), [30.0]);
}
