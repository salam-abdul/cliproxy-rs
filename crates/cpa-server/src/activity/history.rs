use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{Activity, Range, epoch_ms, meta};

/// The most model rows a read returns.
const MODELS: usize = 500;
/// The most failure reasons a read returns.
const ERRORS: usize = 5;

/// One row per provider (kind 0), filtered bucket (1), filtered failure reason (2), Claude
/// credential bucket (3), filtered provider and model (4), and the filtered token totals (5).
///
/// A native request is its `requests` row from `?4` on, under `claude`, with the credential
/// of its last native send. A proxy request is a `(request_id, turn)` of proxy sends that
/// serve no other request, taken from its last send; when that send is in range, so is every
/// send after it, so judging among sends in range finds it. A fallback send carries the
/// native request as `parent_request`, so the request counts once, under its native row.
/// Tokens come from every send in range and only from exact ones.
const READ: &str = "WITH native AS (
        SELECT r.at_ms, 'claude' AS provider, r.model, r.outcome, NULL AS status,
            r.outcome NOT IN ('success', 'client_cancelled', 'fallback_served') AS failed,
            (SELECT s.credential FROM sends s WHERE s.request_id = r.request_id AND s.source = 'native'
                ORDER BY s.send DESC LIMIT 1) AS credential
        FROM requests r WHERE r.at_ms >= ?4 AND r.at_ms <= ?2
    ), proxy AS (
        SELECT at_ms, provider, model, outcome, status, outcome NOT IN ('complete', 'cancelled') AS failed,
            credential
        FROM (
            SELECT at_ms, provider, model, outcome, status, credential, ROW_NUMBER() OVER (
                PARTITION BY request_id, turn ORDER BY at_ms DESC, send DESC
            ) AS from_last
            FROM sends WHERE source = 'proxy' AND parent_request IS NULL AND at_ms >= ?1 AND at_ms <= ?2
        ) WHERE from_last = 1
    ), every AS MATERIALIZED (
        SELECT * FROM native UNION ALL SELECT * FROM proxy
    ), picked AS (
        SELECT * FROM every WHERE ?3 IS NULL OR provider = ?3
    ), spent AS (
        SELECT provider, model, tokens_exact IS 1 AS exact,
            CASE WHEN tokens_exact = 1 THEN COALESCE(uncached_input_tokens, 0) ELSE 0 END AS uncached,
            CASE WHEN tokens_exact = 1 THEN COALESCE(cache_read_tokens, 0) ELSE 0 END AS cache_read,
            CASE WHEN tokens_exact = 1 THEN COALESCE(cache_write_tokens, 0) ELSE 0 END AS cache_write,
            CASE WHEN tokens_exact = 1 THEN COALESCE(output_tokens, 0) ELSE 0 END AS output
        FROM sends WHERE at_ms >= ?1 AND at_ms <= ?2 AND (?3 IS NULL OR provider = ?3)
    )
    SELECT * FROM (
        SELECT 0 AS kind, provider AS name, NULL AS model, NULL AS at, NULL AS status, 0 AS requests,
            0 AS failures, 0 AS uncached, 0 AS cache_read, 0 AS cache_write, 0 AS output, 0 AS sends,
            0 AS exact
        FROM every GROUP BY provider
        UNION ALL SELECT 1, NULL, NULL, (at_ms / ?5) * ?5, NULL, COUNT(*), SUM(failed), 0, 0, 0, 0, 0, 0
        FROM picked GROUP BY 4
        UNION ALL SELECT 2, outcome, NULL, NULL, status, COUNT(*), COUNT(*), 0, 0, 0, 0, 0, 0
        FROM picked WHERE failed GROUP BY outcome, status
        UNION ALL SELECT 3, credential, NULL, (at_ms / ?5) * ?5, NULL, COUNT(*), SUM(failed), 0, 0, 0, 0, 0, 0
        FROM every WHERE provider = 'claude' AND credential IS NOT NULL GROUP BY credential, 4
        UNION ALL SELECT 4, provider, model, NULL, NULL, SUM(requests), 0, SUM(uncached), SUM(cache_read),
            SUM(cache_write), SUM(output), SUM(sends), SUM(exact)
        FROM (
            SELECT provider, model, 1 AS requests, 0 AS uncached, 0 AS cache_read, 0 AS cache_write,
                0 AS output, 0 AS sends, 0 AS exact FROM picked
            UNION ALL SELECT provider, model, 0, uncached, cache_read, cache_write, output, 1, exact FROM spent
        ) GROUP BY provider, model
        UNION ALL SELECT 5, NULL, NULL, NULL, NULL, 0, 0, COALESCE(SUM(uncached), 0),
            COALESCE(SUM(cache_read), 0), COALESCE(SUM(cache_write), 0), COALESCE(SUM(output), 0),
            COUNT(*), COALESCE(SUM(exact), 0)
        FROM spent
    ) ORDER BY kind, CASE WHEN kind IN (2, 4) THEN -requests END, CASE WHEN kind = 4 THEN -sends END,
        name, model, at, status";

impl Activity {
    /// Native and proxy requests over `range`, with requests, failures, tokens and models
    /// narrowed to `provider` when given.
    pub(crate) async fn history(&self, range: Range, provider: Option<String>) -> anyhow::Result<Value> {
        let path = self.path.clone();
        let lost = self.lost.load(Ordering::Relaxed);
        tokio::task::spawn_blocking(move || read(&path, range, provider.as_deref(), epoch_ms(), lost)).await?
    }
}

pub(super) fn read(path: &Path, range: Range, provider: Option<&str>, now: i64, lost: u64) -> anyhow::Result<Value> {
    let mut db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    db.busy_timeout(Duration::from_secs(2))?;
    let conn = db.transaction()?;
    let from = now - range.duration();
    let bucket_ms = range.bucket();
    // Native rows older than the first send have no proxy record beside them, so the read
    // starts at the first send and reads nothing before one exists.
    let oldest: Option<i64> = conn.query_row("SELECT MIN(at_ms) FROM sends", [], |row| row.get(0))?;
    let native_from = oldest.map(|start| start.max(from));
    let mut providers = Vec::new();
    let mut buckets = Vec::new();
    let mut errors = Vec::new();
    let mut account_buckets = Vec::new();
    let mut models = Vec::new();
    let mut models_truncated = false;
    let mut tokens = json!({"uncachedInputTokens": 0, "cacheReadTokens": 0, "cacheWriteTokens": 0,
        "outputTokens": 0, "sends": 0, "exactSends": 0});
    if native_from.is_some() {
        let mut statement = conn.prepare(READ)?;
        let mut rows = statement.query(params![from, now, provider, native_from, bucket_ms])?;
        while let Some(row) = rows.next()? {
            let count = |index: usize| row.get::<_, i64>(index);
            match row.get::<_, i64>(0)? {
                0 => providers.push(Value::from(row.get::<_, String>(1)?)),
                1 => buckets.push(json!({"at": count(3)?, "requests": count(5)?, "failures": count(6)?})),
                2 if errors.len() < ERRORS => errors.push(json!({
                    "outcome": row.get::<_, String>(1)?, "status": row.get::<_, Option<i64>>(4)?,
                    "requests": count(5)?
                })),
                3 => account_buckets.push(json!({
                    "credential": row.get::<_, String>(1)?, "at": count(3)?,
                    "requests": count(5)?, "failures": count(6)?
                })),
                4 if models.len() < MODELS => models.push(json!({
                    "provider": row.get::<_, String>(1)?, "model": row.get::<_, Option<String>>(2)?,
                    "requests": count(5)?, "uncachedInputTokens": count(7)?, "cacheReadTokens": count(8)?,
                    "cacheWriteTokens": count(9)?, "outputTokens": count(10)?, "sends": count(11)?,
                    "exactSends": count(12)?
                })),
                4 => models_truncated = true,
                5 => {
                    tokens = json!({
                        "uncachedInputTokens": count(7)?, "cacheReadTokens": count(8)?,
                        "cacheWriteTokens": count(9)?, "outputTokens": count(10)?, "sends": count(11)?,
                        "exactSends": count(12)?
                    })
                }
                _ => {}
            }
        }
    }
    let statuses = statuses(&conn, from, now, range.duration() / 144)?;
    let oldest_status: Option<i64> = conn.query_row("SELECT MIN(at_ms) FROM account_status", [], |row| row.get(0))?;
    let lost = lost.max(meta(&conn, "lost_events")?.unwrap_or(0).max(0) as u64);
    let coverage_uncertain = meta(&conn, "uncertain_at")?.is_some_and(|at| at >= from && at <= now);
    Ok(
        json!({"range": range.name(), "from": from, "to": now, "bucketMs": bucket_ms,
        "oldest": oldest, "oldestStatus": oldest_status, "lostEvents": lost,
        "coverageUncertain": coverage_uncertain, "providers": providers, "buckets": buckets,
        "errors": errors, "tokens": tokens, "models": models, "modelsTruncated": models_truncated,
        "accountBuckets": account_buckets, "statuses": statuses}),
    )
}

/// The latest status of each account, model and availability in every `sample_ms` slice
/// of the range.
fn statuses(conn: &Connection, from: i64, now: i64, sample_ms: i64) -> rusqlite::Result<Vec<Value>> {
    conn.prepare(
        "SELECT s.account, s.model, s.at_ms, s.availability,
        s.five_hour, s.weekly, s.five_hour_reset, s.weekly_reset
        FROM account_status s JOIN (
            SELECT account, model, availability, MAX(at_ms) AS at_ms FROM account_status
            WHERE at_ms >= ?1 AND at_ms <= ?2
            GROUP BY account, model, availability, at_ms / ?3
        ) latest ON s.account = latest.account AND s.model = latest.model
            AND s.availability = latest.availability AND s.at_ms = latest.at_ms
        ORDER BY s.at_ms",
    )?
    .query_map(params![from, now, sample_ms], |row| {
        Ok(json!({
            "account": row.get::<_, String>(0)?, "model": row.get::<_, String>(1)?,
            "at": row.get::<_, i64>(2)?, "availability": row.get::<_, String>(3)?,
            "fiveHourUsed": row.get::<_, Option<f64>>(4)?, "weeklyUsed": row.get::<_, Option<f64>>(5)?,
            "fiveHourReset": row.get::<_, Option<i64>>(6)?, "weeklyReset": row.get::<_, Option<i64>>(7)?
        }))
    })?
    .collect()
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
