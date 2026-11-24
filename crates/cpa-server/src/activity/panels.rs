use rusqlite::{Connection, OpenFlags, Row, params};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::cycles::{Point, cycles};
use super::{Activity, DAY_MS, Range, epoch_ms, meta};

/// The sends in range with their request (a fallback send counts under the request it
/// serves), whether they failed or were cancelled (a 429 is neither, so the three counts
/// are disjoint), and their place from the last send of that request,
/// then the filtered ones. A send's place is judged among every send in range, so a
/// filter never turns a failed-over send into its request's last. `picked` is materialized
/// so the provider, credential and bucket groups share one pass over `sends`.
const SENDS: &str = "WITH ranged AS (
        SELECT at_ms, provider, credential, outcome, status, turn, tokens_exact, uncached_input_tokens,
            cache_read_tokens, cache_write_tokens, output_tokens, COALESCE(parent_request, request_id) AS request,
            (outcome NOT IN ('complete', 'cancelled') AND status IS NOT 429) AS failed,
            (outcome = 'cancelled' AND status IS NOT 429) AS cancelled,
            ROW_NUMBER() OVER (
                PARTITION BY COALESCE(parent_request, request_id), turn ORDER BY at_ms DESC, send DESC
            ) AS from_last
        FROM sends WHERE at_ms >= ?1 AND at_ms <= ?2
    ), picked AS MATERIALIZED (
        SELECT * FROM ranged WHERE (?3 IS NULL OR provider = ?3) AND (?4 IS NULL OR credential = ?4)
    )";

/// The totals [`totals`] reads, from column `first` on.
const TOTALS: &str = "COUNT(DISTINCT json_array(request, turn)), COUNT(*), SUM(failed),
    SUM(status IS 429), SUM(from_last > 1),
    SUM(CASE WHEN tokens_exact = 1 THEN COALESCE(uncached_input_tokens, 0) ELSE 0 END),
    SUM(CASE WHEN tokens_exact = 1 THEN COALESCE(cache_read_tokens, 0) ELSE 0 END),
    SUM(CASE WHEN tokens_exact = 1 THEN COALESCE(cache_write_tokens, 0) ELSE 0 END),
    SUM(CASE WHEN tokens_exact = 1 THEN COALESCE(output_tokens, 0) ELSE 0 END),
    SUM(cancelled), SUM(tokens_exact IS 1)";

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
        "cancelled",
        "exactSends",
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
    let mut providers = Vec::new();
    let mut credentials = Vec::new();
    let mut buckets = Vec::new();
    let mut statement = conn.prepare(&format!(
        "{SENDS} SELECT 0, NULL, provider, NULL, {TOTALS} FROM picked GROUP BY provider
        UNION ALL SELECT 1, NULL, provider, credential, {TOTALS} FROM picked WHERE credential IS NOT NULL
            GROUP BY provider, credential
        UNION ALL SELECT 2, (at_ms / ?5) * ?5, provider, NULL, 0, COUNT(*), SUM(failed), SUM(status IS 429),
            0, 0, 0, 0, 0, SUM(cancelled), 0 FROM picked GROUP BY 2, provider
        ORDER BY 1, 2, 3, 4"
    ))?;
    let mut rows = statement.query(params![from, now, provider, credential, bucket_ms])?;
    while let Some(row) = rows.next()? {
        let mut object = serde_json::Map::new();
        object.insert("provider".into(), row.get::<_, String>(2)?.into());
        match row.get::<_, i64>(0)? {
            0 => providers.push(totals(row, 4, object)?),
            1 => {
                object.insert("credential".into(), row.get::<_, String>(3)?.into());
                credentials.push(totals(row, 4, object)?);
            }
            _ => buckets.push(json!({
                "at": row.get::<_, i64>(1)?, "provider": object["provider"],
                "sends": row.get::<_, i64>(5)?, "failed": row.get::<_, i64>(6)?,
                "rateLimited": row.get::<_, i64>(7)?, "cancelled": row.get::<_, i64>(13)?
            })),
        }
    }
    drop(rows);
    drop(statement);
    let cycles = weekly_cycles(&conn, from, now, provider, credential)?;
    let oldest: Option<i64> = conn.query_row("SELECT MIN(at_ms) FROM sends", [], |row| row.get(0))?;
    let lost = lost.max(meta(&conn, "lost_events")?.unwrap_or(0).max(0) as u64);
    let coverage_uncertain = meta(&conn, "uncertain_at")?.is_some_and(|at| at >= from && at <= now);
    Ok(
        json!({"range": range.name(), "from": from, "to": now, "bucketMs": bucket_ms,
        "providers": providers, "credentials": credentials, "buckets": buckets, "cycles": cycles,
        "oldestSend": oldest, "lostEvents": lost, "coverageUncertain": coverage_uncertain}),
    )
}

/// A cycle first seen at this use or more began before its first reading.
const PARTIAL_FROM: f64 = 5.0;
/// The smallest rise in use, in points, that gets a tokens-per-percent figure.
const MIN_RISE: f64 = 5.0;

/// Each credential's weekly quota cycles that end at or after `from`, plus its latest ended
/// cycle before `from` when none of the former has ended, computed from readings since a
/// week before `from`.
fn weekly_cycles(
    conn: &Connection,
    from: i64,
    now: i64,
    provider: Option<&str>,
    credential: Option<&str>,
) -> rusqlite::Result<Vec<Value>> {
    let readings = conn
        .prepare(
            "SELECT provider, credential, at_ms, used, reset_ms FROM quota_readings
            WHERE long = 1 AND at_ms >= ?1 AND at_ms <= ?2 AND (?3 IS NULL OR provider = ?3)
            AND (?4 IS NULL OR credential = ?4) ORDER BY credential, at_ms, rowid",
        )?
        .query_map(params![from - 7 * DAY_MS, now, provider, credential], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                Point {
                    at: row.get(2)?,
                    used: row.get(3)?,
                    reset: row.get(4)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut tokens = conn.prepare(
        "SELECT COALESCE(SUM(COALESCE(uncached_input_tokens, 0) + COALESCE(cache_write_tokens, 0)
            + COALESCE(output_tokens, 0)), 0)
        FROM sends WHERE credential = ?1 AND tokens_exact = 1 AND at_ms >= ?2 AND at_ms <= ?3",
    )?;
    let mut out = Vec::new();
    for group in readings.chunk_by(|a, b| a.1 == b.1) {
        let Some((provider, credential, _)) = group.first() else {
            continue;
        };
        let points: Vec<Point> = group.iter().map(|(_, _, point)| *point).collect();
        let all = cycles(&points, now);
        let ended_in_range = all.iter().any(|cycle| cycle.end >= from && cycle.ended);
        let earlier = all
            .iter()
            .rposition(|cycle| cycle.end < from)
            .filter(|_| !ended_in_range);
        for (index, cycle) in all.iter().enumerate() {
            let in_range = cycle.end >= from;
            if !in_range && !(Some(index) == earlier && cycle.ended) {
                continue;
            }
            let tokens: i64 = tokens.query_row(params![credential, cycle.start, cycle.end], |row| row.get(0))?;
            let rise = cycle.last - cycle.first;
            out.push(json!({
                "provider": provider, "credential": credential,
                "start": cycle.start, "end": cycle.end, "reset": cycle.reset,
                "first": cycle.first, "peak": cycle.peak, "last": cycle.last, "ended": cycle.ended,
                "unused": cycle.ended.then_some(100.0 - cycle.last),
                "partial": cycle.first >= PARTIAL_FROM, "inRange": in_range, "tokens": tokens,
                "tokensPerPercent": (rise >= MIN_RISE && tokens > 0).then(|| tokens as f64 / rise),
            }));
        }
    }
    Ok(out)
}

#[cfg(test)]
#[path = "panels_tests.rs"]
mod tests;
