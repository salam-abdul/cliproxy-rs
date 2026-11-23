use rusqlite::{Connection, params};

use super::{Activity, Command, bounded};
use crate::scheduler::Reading;

/// Readings of one window within these milliseconds keep only the latest.
const BUCKET_MS: i64 = 600_000;

pub(super) struct ReadingRow {
    credential: String,
    provider: String,
    long: bool,
    at_ms: i64,
    used: f64,
    reset_ms: Option<i64>,
}

impl Activity {
    /// Records `readings` of `credential`'s quota observed at `at_ms`.
    pub(crate) fn record_readings(&self, provider: &str, credential: &str, at_ms: i64, readings: &[Reading]) {
        if readings.is_empty() {
            return;
        }
        let (Some(provider), Some(credential)) = (bounded(Some(provider)), bounded(Some(credential))) else {
            self.lost
                .fetch_add(readings.len() as u64, std::sync::atomic::Ordering::Relaxed);
            return;
        };
        let rows = readings
            .iter()
            .map(|reading| ReadingRow {
                credential: credential.clone(),
                provider: provider.clone(),
                long: reading.long,
                at_ms,
                used: reading.used_percent,
                reset_ms: reading
                    .reset
                    .and_then(crate::quota_time::epoch_millis)
                    .and_then(|ms| i64::try_from(ms).ok()),
            })
            .collect();
        self.send(Command::Readings(rows));
    }
}

pub(super) fn write_reading(conn: &Connection, row: &ReadingRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO quota_readings (credential, provider, long, bucket_ms, at_ms, used, reset_ms)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            row.credential,
            row.provider,
            row.long,
            row.at_ms.div_euclid(BUCKET_MS) * BUCKET_MS,
            row.at_ms,
            row.used,
            row.reset_ms,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod tests;
