use rusqlite::{Connection, OpenFlags, Row, params};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{Activity, Range, epoch_ms, meta};

/// The sends in range with their request (a fallback send counts under the request it
/// serves), whether they failed, and their place from the last send of that request,
/// then the filtered ones. A send's place is judged among every send in range, so a
/// filter never turns a failed-over send into its request's last.
const SENDS: &str = "WITH ranged AS (
        SELECT *, COALESCE(parent_request, request_id) AS request,
            (outcome NOT IN ('complete', 'cancelled') AND status IS NOT 429) AS failed,
            ROW_NUMBER() OVER (
                PARTITION BY COALESCE(parent_request, request_id), turn ORDER BY at_ms DESC, send DESC
            ) AS from_last
        FROM sends WHERE at_ms >= ?1 AND at_ms <= ?2
    ), picked AS (
        SELECT * FROM ranged WHERE (?3 IS NULL OR provider = ?3) AND (?4 IS NULL OR credential = ?4)
    )";

/// The totals [`totals`] reads, from column `first` on.
const TOTALS: &str = "COUNT(DISTINCT json_array(request, turn)), COUNT(*), SUM(failed),
    SUM(status IS 429), SUM(from_last > 1),
    SUM(CASE WHEN tokens_exact = 1 THEN COALESCE(uncached_input_tokens, 0) ELSE 0 END),
    SUM(CASE WHEN tokens_exact = 1 THEN COALESCE(cache_read_tokens, 0) ELSE 0 END),
    SUM(CASE WHEN tokens_exact = 1 THEN COALESCE(cache_write_tokens, 0) ELSE 0 END),
    SUM(CASE WHEN tokens_exact = 1 THEN COALESCE(output_tokens, 0) ELSE 0 END)";

fn totals(row: &Row, first: usize, mut object: serde_json::Map<String, Value>) -> rusqlite::Result<Value> {
    let names = [
        "requests",
        "sends",
        "failed",
        "rateLimited",
        "failedOver",
        "uncachedInputTokens",
        "cacheReadTokens",
        "cacheWriteTokens",
        "outputTokens",
    ];
    for (offset, name) in names.into_iter().enumerate() {
        object.insert(name.into(), row.get::<_, i64>(first + offset)?.into());
    }
    Ok(Value::Object(object))
}

impl Activity {
    /// Provider, credential and quota activity over `range`, narrowed to `provider` and
    /// `credential` when given.
    pub(crate) async fn panels(
        &self,
        range: Range,
        provider: Option<String>,
        credential: Option<String>,
    ) -> anyhow::Result<Value> {
        let path = self.path.clone();
        let lost = self.lost.load(Ordering::Relaxed);
        tokio::task::spawn_blocking(move || {
            read(
                &path,
                range,
                provider.as_deref(),
                credential.as_deref(),
                epoch_ms(),
                lost,
            )
        })
        .await?
    }
}

fn read(
    path: &Path,
    range: Range,
    provider: Option<&str>,
    credential: Option<&str>,
    now: i64,
    lost: u64,
) -> anyhow::Result<Value> {
    let mut db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    db.busy_timeout(Duration::from_secs(2))?;
    let conn = db.transaction()?;
    let from = now - range.duration();
    let bucket_ms = range.bucket();
    let filter = params![from, now, provider, credential];
    let providers = conn
        .prepare(&format!(
            "{SENDS} SELECT provider, {TOTALS} FROM picked GROUP BY provider ORDER BY provider"
        ))?
        .query_map(filter, |row| {
            let mut object = serde_json::Map::new();
            object.insert("provider".into(), row.get::<_, String>(0)?.into());
            totals(row, 1, object)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let credentials = conn
        .prepare(&format!(
            "{SENDS} SELECT provider, credential, {TOTALS} FROM picked WHERE credential IS NOT NULL
            GROUP BY provider, credential ORDER BY provider, credential"
        ))?
        .query_map(filter, |row| {
            let mut object = serde_json::Map::new();
            object.insert("provider".into(), row.get::<_, String>(0)?.into());
            object.insert("credential".into(), row.get::<_, String>(1)?.into());
            totals(row, 2, object)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let buckets = conn
        .prepare(&format!(
            "{SENDS} SELECT (at_ms / ?5) * ?5, provider, COUNT(*), SUM(failed) FROM picked
            GROUP BY 1, provider ORDER BY 1, provider"
        ))?
        .query_map(params![from, now, provider, credential, bucket_ms], |row| {
            Ok(json!({
                "at": row.get::<_, i64>(0)?, "provider": row.get::<_, String>(1)?,
                "sends": row.get::<_, i64>(2)?, "failed": row.get::<_, i64>(3)?
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let readings = conn
        .prepare(
            "SELECT provider, credential, long, at_ms, used, reset_ms FROM quota_readings
            WHERE at_ms >= ?1 AND at_ms <= ?2 AND (?3 IS NULL OR provider = ?3)
            AND (?4 IS NULL OR credential = ?4) ORDER BY credential, long, at_ms",
        )?
        .query_map(filter, |row| {
            Ok(json!({
                "provider": row.get::<_, String>(0)?, "credential": row.get::<_, String>(1)?,
                "long": row.get::<_, bool>(2)?, "at": row.get::<_, i64>(3)?,
                "used": row.get::<_, f64>(4)?, "reset": row.get::<_, Option<i64>>(5)?
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let oldest: Option<i64> = conn.query_row("SELECT MIN(at_ms) FROM sends", [], |row| row.get(0))?;
    let lost = lost.max(meta(&conn, "lost_events")?.unwrap_or(0).max(0) as u64);
    let coverage_uncertain = meta(&conn, "uncertain_at")?.is_some_and(|at| at >= from && at <= now);
    Ok(
        json!({"range": range.name(), "from": from, "to": now, "bucketMs": bucket_ms,
        "providers": providers, "credentials": credentials, "buckets": buckets, "readings": readings,
        "oldestSend": oldest, "lostEvents": lost, "coverageUncertain": coverage_uncertain}),
    )
}

#[cfg(test)]
#[path = "panels_tests.rs"]
mod tests;
