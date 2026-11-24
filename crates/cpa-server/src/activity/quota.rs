use rusqlite::{Connection, params};

use super::{Activity, Command, bounded};
use crate::scheduler::Reading;

/// Readings of one window within these milliseconds keep only the latest, per stated
/// reset within the same span.
const BUCKET_MS: i64 = 600_000;

/// `reset_slot` is the stated reset in units of [`BUCKET_MS`], or -1 when none was stated,
/// so the last reading before a reset is not replaced by the first one after it. Its default
/// lets an older build, which does not write it, still insert readings.
pub(super) const READINGS_TABLE: &str = "CREATE TABLE IF NOT EXISTS quota_readings (
        credential TEXT NOT NULL, provider TEXT NOT NULL, long INTEGER NOT NULL,
        bucket_ms INTEGER NOT NULL, at_ms INTEGER NOT NULL, used REAL NOT NULL, reset_ms INTEGER,
        reset_slot INTEGER NOT NULL DEFAULT -1,
        PRIMARY KEY (credential, long, bucket_ms, reset_slot)
    );";

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

/// Rebuilds a `quota_readings` table written without `reset_slot`, keeping its rows,
/// since SQLite cannot change a primary key in place.
pub(super) fn rekey_readings(conn: &mut Connection) -> rusqlite::Result<()> {
    let columns = conn
        .prepare("SELECT name FROM pragma_table_info('quota_readings')")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if columns.is_empty() || columns.iter().any(|name| name == "reset_slot") {
        return Ok(());
    }
    let transaction = conn.transaction()?;
    transaction.execute_batch(&format!(
        "ALTER TABLE quota_readings RENAME TO quota_readings_unslotted;
        {READINGS_TABLE}
        INSERT INTO quota_readings (credential, provider, long, bucket_ms, at_ms, used, reset_ms, reset_slot)
            SELECT credential, provider, long, bucket_ms, at_ms, used, reset_ms,
                CASE WHEN reset_ms IS NULL THEN -1 ELSE reset_ms / {BUCKET_MS} END
            FROM quota_readings_unslotted;
        DROP TABLE quota_readings_unslotted;"
    ))?;
    transaction.commit()
}

pub(super) fn write_reading(conn: &Connection, row: &ReadingRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO quota_readings
        (credential, provider, long, bucket_ms, at_ms, used, reset_ms, reset_slot)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            row.credential,
            row.provider,
            row.long,
            row.at_ms.div_euclid(BUCKET_MS) * BUCKET_MS,
            row.at_ms,
            row.used,
            row.reset_ms,
            row.reset_ms.map_or(-1, |ms| ms.div_euclid(BUCKET_MS)),
        ],
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod tests;
