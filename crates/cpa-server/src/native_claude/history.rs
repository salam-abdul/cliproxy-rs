use anyhow::Context;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
    time::{Duration, SystemTime},
};

const QUEUE: usize = 1024;
const DAY_MS: i64 = 86_400_000;
const RETAIN_MS: i64 = 31 * DAY_MS;
const MAX_REQUESTS: i64 = 250_000;
const MAX_STATUSES: i64 = 50_000;

#[derive(Clone, Copy)]
pub(crate) enum Range {
    Day,
    Week,
    Month,
}

impl Range {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "24h" => Some(Self::Day),
            "7d" => Some(Self::Week),
            "30d" => Some(Self::Month),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Day => "24h",
            Self::Week => "7d",
            Self::Month => "30d",
        }
    }

    fn duration(self) -> i64 {
        match self {
            Self::Day => DAY_MS,
            Self::Week => 7 * DAY_MS,
            Self::Month => 30 * DAY_MS,
        }
    }

    fn bucket(self) -> i64 {
        match self {
            Self::Day => 3_600_000,
            Self::Week => 6 * 3_600_000,
            Self::Month => DAY_MS,
        }
    }
}

struct RequestRow {
    at: i64,
    id: String,
    account: Option<String>,
    model: Option<String>,
    outcome: String,
    status: Option<i64>,
    input: Option<i64>,
    output: Option<i64>,
    cache_read: Option<i64>,
    cache_write: Option<i64>,
    attempts: Option<i64>,
}

struct StatusRow {
    at: i64,
    account: String,
    model: String,
    availability: String,
    five_hour: Option<f64>,
    weekly: Option<f64>,
    five_hour_reset: Option<i64>,
    weekly_reset: Option<i64>,
}

enum Command {
    Request(Box<RequestRow>),
    Status(Vec<StatusRow>),
    Finish(std::sync::mpsc::Sender<()>),
    #[cfg(test)]
    Flush(std::sync::mpsc::Sender<()>),
}

impl Command {
    fn records(&self) -> u64 {
        match self {
            Self::Request(_) => 1,
            Self::Status(rows) => rows.len() as u64,
            Self::Finish(_) => 0,
            #[cfg(test)]
            Self::Flush(_) => 0,
        }
    }
}

pub(super) struct History {
    path: PathBuf,
    tx: SyncSender<Command>,
    lost: Arc<AtomicU64>,
}

impl History {
    pub(super) fn start(path: PathBuf, lost: Arc<AtomicU64>) -> anyhow::Result<Self> {
        secure_file(&path)?;
        let mut conn = Connection::open(&path)?;
        conn.busy_timeout(Duration::from_secs(2))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS requests (
                request_id TEXT PRIMARY KEY, at_ms INTEGER NOT NULL, account TEXT, model TEXT,
                outcome TEXT NOT NULL, status INTEGER, input_tokens INTEGER, output_tokens INTEGER,
                cache_read_tokens INTEGER, cache_write_tokens INTEGER, attempts INTEGER
            );
            CREATE INDEX IF NOT EXISTS requests_by_time ON requests(at_ms);
            CREATE INDEX IF NOT EXISTS requests_by_account_time ON requests(account, at_ms);
            CREATE TABLE IF NOT EXISTS account_status (
                account TEXT NOT NULL, model TEXT NOT NULL, bucket_ms INTEGER NOT NULL,
                at_ms INTEGER NOT NULL, availability TEXT NOT NULL,
                five_hour REAL, weekly REAL, five_hour_reset INTEGER, weekly_reset INTEGER,
                PRIMARY KEY(account, model, bucket_ms, availability)
            );
            CREATE INDEX IF NOT EXISTS account_status_by_time ON account_status(at_ms);
            CREATE TABLE IF NOT EXISTS history_meta (
                key TEXT PRIMARY KEY, value INTEGER NOT NULL
            );",
        )?;
        prune(&conn, epoch_ms())?;
        let previous_lost = meta(&conn, "lost_events")?.unwrap_or(0).max(0) as u64;
        lost.store(previous_lost, Ordering::Relaxed);
        let unclean = meta(&conn, "dirty")? == Some(1);
        let startup = conn.transaction()?;
        if unclean {
            set_meta(&startup, "uncertain_at", epoch_ms())?;
        }
        set_meta(&startup, "dirty", 1)?;
        startup.commit()?;
        let (tx, rx) = sync_channel(QUEUE);
        let writer_lost = lost.clone();
        std::thread::Builder::new()
            .name("native-history".into())
            .spawn(move || {
                let mut last_prune = std::time::Instant::now();
                while let Ok(first) = rx.recv() {
                    let mut commands = vec![first];
                    while commands.len() < 64 {
                        match rx.try_recv() {
                            Ok(next) => commands.push(next),
                            Err(_) => break,
                        }
                    }
                    let count = commands.iter().map(Command::records).sum::<u64>();
                    let finish = commands.iter().any(|command| matches!(command, Command::Finish(_)));
                    let result = (|| -> rusqlite::Result<()> {
                        let transaction = conn.transaction()?;
                        for command in &commands {
                            match command {
                                Command::Request(row) => write_request(&transaction, row)?,
                                Command::Status(rows) => {
                                    for row in rows {
                                        write_status(&transaction, row)?;
                                    }
                                }
                                Command::Finish(_) => {}
                                #[cfg(test)]
                                Command::Flush(_) => {}
                            }
                        }
                        set_meta(
                            &transaction,
                            "lost_events",
                            writer_lost.load(Ordering::Relaxed).min(i64::MAX as u64) as i64,
                        )?;
                        if finish {
                            set_meta(&transaction, "dirty", 0)?;
                        }
                        transaction.commit()
                    })();
                    if result.is_err() {
                        writer_lost.fetch_add(count, Ordering::Relaxed);
                    }
                    // ponytail: Hourly trimming keeps writes cheap; track live row counts for hard caps.
                    if !finish && last_prune.elapsed() >= Duration::from_secs(3600) {
                        if prune(&conn, epoch_ms()).is_err() {
                            writer_lost.fetch_add(1, Ordering::Relaxed);
                        }
                        last_prune = std::time::Instant::now();
                    }
                    #[cfg(test)]
                    for command in &commands {
                        if let Command::Flush(done) = command {
                            let _ = done.send(());
                        }
                    }
                    for command in commands {
                        if let Command::Finish(done) = command {
                            let _ = done.send(());
                        }
                    }
                    if finish {
                        break;
                    }
                }
            })?;
        Ok(Self { path, tx, lost })
    }

    pub(super) fn record_event(&self, event: &Value) {
        if let Some(row) = RequestRow::from_event(event) {
            self.send(Command::Request(Box::new(row)));
        }
    }

    pub(super) fn record_status(&self, status: &Value) {
        let Some(at) = status["observedAt"].as_i64() else {
            return;
        };
        let Some(model) = bounded(status["model"].as_str()) else {
            return;
        };
        let rows = status["accounts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|account| {
                let id = bounded(account["id"].as_str())?;
                let availability = bounded(account["availability"].as_str())?;
                Some(StatusRow {
                    at,
                    account: id,
                    model: model.clone(),
                    availability,
                    five_hour: percent(&account["includedQuota"]["fiveHour"]),
                    weekly: percent(&account["includedQuota"]["weekly"]),
                    five_hour_reset: timestamp(&account["includedQuota"]["fiveHourResetsAt"]),
                    weekly_reset: timestamp(&account["includedQuota"]["weeklyResetsAt"]),
                })
            })
            .collect();
        self.send(Command::Status(rows));
    }

    fn send(&self, command: Command) {
        let count = command.records();
        if self.tx.try_send(command).is_err() {
            self.lost.fetch_add(count, Ordering::Relaxed);
        }
    }

    pub(super) async fn query(&self, range: Range, account: Option<String>) -> anyhow::Result<Value> {
        let path = self.path.clone();
        let lost = self.lost.load(Ordering::Relaxed);
        tokio::task::spawn_blocking(move || query(&path, range, account.as_deref(), epoch_ms(), lost)).await?
    }

    #[cfg(test)]
    fn flush(&self) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.tx.send(Command::Flush(tx)).unwrap();
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }
}

impl Drop for History {
    fn drop(&mut self) {
        let (done, ack) = std::sync::mpsc::channel();
        if self.tx.try_send(Command::Finish(done)).is_ok() {
            let _ = ack.recv_timeout(Duration::from_secs(2));
        }
    }
}

fn meta(conn: &Connection, key: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row("SELECT value FROM history_meta WHERE key = ?1", [key], |row| row.get(0))
        .optional()
}

fn set_meta(conn: &Connection, key: &str, value: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO history_meta (key, value) VALUES (?1, ?2)
        ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

impl RequestRow {
    fn from_event(event: &Value) -> Option<Self> {
        if !["inference", "rejected", "transport-failure"].contains(&event["type"].as_str()?) {
            return None;
        }
        let usage = &event["usage"];
        Some(Self {
            at: timestamp(&event["at"])?,
            id: bounded(event["requestId"].as_str())?,
            account: bounded(event["account"].as_str()),
            model: bounded(event["model"].as_str()),
            outcome: bounded(event["outcome"].as_str())?,
            status: nonnegative(&event["status"]),
            input: nonnegative(&usage["input_tokens"]),
            output: nonnegative(&usage["output_tokens"]),
            cache_read: nonnegative(&usage["cache_read_input_tokens"]),
            cache_write: nonnegative(&usage["cache_creation_input_tokens"]),
            attempts: nonnegative(&event["attempts"]),
        })
    }
}

fn bounded(value: Option<&str>) -> Option<String> {
    value.filter(|s| !s.is_empty() && s.len() <= 256).map(str::to_owned)
}

fn nonnegative(value: &Value) -> Option<i64> {
    value.as_i64().filter(|n| *n >= 0)
}

fn percent(value: &Value) -> Option<f64> {
    value.as_f64().filter(|n| n.is_finite() && (0.0..=100.0).contains(n))
}

fn timestamp(value: &Value) -> Option<i64> {
    Some(
        chrono::DateTime::parse_from_rfc3339(value.as_str()?)
            .ok()?
            .timestamp_millis(),
    )
}

fn epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn secure_file(path: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => anyhow::ensure!(meta.file_type().is_file(), "History path is not a regular file"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            options
                .open(path)
                .with_context(|| "Could not create private history file")?;
        }
        Err(e) => return Err(e.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn write_request(conn: &Connection, row: &RequestRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO requests VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            row.id,
            row.at,
            row.account,
            row.model,
            row.outcome,
            row.status,
            row.input,
            row.output,
            row.cache_read,
            row.cache_write,
            row.attempts
        ],
    )?;
    Ok(())
}

fn write_status(conn: &Connection, row: &StatusRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO account_status VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            row.account,
            row.model,
            row.at / 600_000 * 600_000,
            row.at,
            row.availability,
            row.five_hour,
            row.weekly,
            row.five_hour_reset,
            row.weekly_reset
        ],
    )?;
    Ok(())
}

fn prune(conn: &Connection, now: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM requests WHERE at_ms < ?1", [now - RETAIN_MS])?;
    conn.execute("DELETE FROM account_status WHERE at_ms < ?1", [now - RETAIN_MS])?;
    trim(conn, MAX_REQUESTS, MAX_STATUSES)?;
    Ok(())
}

fn trim(conn: &Connection, request_limit: i64, status_limit: i64) -> rusqlite::Result<()> {
    let requests: i64 = conn.query_row("SELECT MAX(0, COUNT(*) - ?1) FROM requests", [request_limit], |row| {
        row.get(0)
    })?;
    if requests > 0 {
        conn.execute(
            "DELETE FROM requests WHERE request_id IN
            (SELECT request_id FROM requests ORDER BY at_ms, request_id LIMIT ?1)",
            [requests],
        )?;
    }
    let statuses: i64 = conn.query_row(
        "SELECT MAX(0, COUNT(*) - ?1) FROM account_status",
        [status_limit],
        |row| row.get(0),
    )?;
    if statuses > 0 {
        conn.execute(
            "DELETE FROM account_status WHERE rowid IN
            (SELECT rowid FROM account_status ORDER BY at_ms, rowid LIMIT ?1)",
            [statuses],
        )?;
    }
    Ok(())
}

fn query(path: &Path, range: Range, account: Option<&str>, now: i64, lost: u64) -> anyhow::Result<Value> {
    let mut db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    db.busy_timeout(Duration::from_secs(2))?;
    let conn = db.transaction()?;
    let from = now - range.duration();
    let bucket_ms = range.bucket();
    let mut statement = conn.prepare(
        "SELECT (at_ms / ?3) * ?3, COUNT(*),
        SUM(CASE WHEN outcome NOT IN ('success','client_cancelled') THEN 1 ELSE 0 END)
        FROM requests WHERE at_ms >= ?1 AND at_ms <= ?2 AND (?4 IS NULL OR account = ?4)
        GROUP BY 1 ORDER BY 1",
    )?;
    let buckets = statement
        .query_map(params![from, now, bucket_ms, account], |row| {
            Ok(json!({
                "at": row.get::<_, i64>(0)?, "requests": row.get::<_, i64>(1)?,
                "failures": row.get::<_, i64>(2)?
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut statement = conn.prepare(
        "SELECT account, (at_ms / ?3) * ?3, COUNT(*),
        SUM(CASE WHEN outcome NOT IN ('success','client_cancelled') THEN 1 ELSE 0 END)
        FROM requests WHERE at_ms >= ?1 AND at_ms <= ?2 AND account IS NOT NULL
        AND (?4 IS NULL OR account = ?4) GROUP BY account, 2 ORDER BY account, 2",
    )?;
    let account_buckets = statement
        .query_map(params![from, now, bucket_ms, account], |row| {
            Ok(json!({
                "account": row.get::<_, String>(0)?, "at": row.get::<_, i64>(1)?,
                "requests": row.get::<_, i64>(2)?, "failures": row.get::<_, i64>(3)?
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut statement = conn.prepare(
        "SELECT outcome, COUNT(*) FROM requests
        WHERE at_ms >= ?1 AND at_ms <= ?2 AND (?3 IS NULL OR account = ?3)
        AND outcome NOT IN ('success','client_cancelled')
        GROUP BY outcome ORDER BY COUNT(*) DESC LIMIT 5",
    )?;
    let errors = statement
        .query_map(params![from, now, account], |row| {
            Ok(json!({
                "outcome": row.get::<_, String>(0)?, "requests": row.get::<_, i64>(1)?
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut statement = conn.prepare(
        "SELECT model, COUNT(*),
        SUM(COALESCE(input_tokens,0)), SUM(COALESCE(output_tokens,0)),
        SUM(COALESCE(cache_read_tokens,0)), SUM(COALESCE(cache_write_tokens,0)),
        COUNT(input_tokens), COUNT(output_tokens), COUNT(cache_read_tokens), COUNT(cache_write_tokens)
        FROM requests WHERE at_ms >= ?1 AND at_ms <= ?2 AND (?3 IS NULL OR account = ?3)
        GROUP BY model ORDER BY COUNT(*) DESC LIMIT 501",
    )?;
    let mut models = statement
        .query_map(params![from, now, account], |row| {
            Ok(json!({
                "model": row.get::<_, Option<String>>(0)?, "requests": row.get::<_, i64>(1)?,
                "inputTokens": row.get::<_, i64>(2)?, "outputTokens": row.get::<_, i64>(3)?,
                "cacheReadTokens": row.get::<_, i64>(4)?, "cacheWriteTokens": row.get::<_, i64>(5)?,
                "reportedInput": row.get::<_, i64>(6)?, "reportedOutput": row.get::<_, i64>(7)?,
                "reportedCacheRead": row.get::<_, i64>(8)?, "reportedCacheWrite": row.get::<_, i64>(9)?
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let models_truncated = models.len() > 500;
    models.truncate(500);
    let sample_ms = range.duration() / 144;
    let mut statement = conn.prepare(
        "SELECT s.account, s.model, s.at_ms, s.availability,
        s.five_hour, s.weekly, s.five_hour_reset, s.weekly_reset
        FROM account_status s JOIN (
            SELECT account, model, availability, MAX(at_ms) AS at_ms FROM account_status
            WHERE at_ms >= ?1 AND at_ms <= ?2 AND (?3 IS NULL OR account = ?3)
            GROUP BY account, model, availability, at_ms / ?4
        ) latest ON s.account = latest.account AND s.model = latest.model
            AND s.availability = latest.availability AND s.at_ms = latest.at_ms
        ORDER BY s.at_ms",
    )?;
    let statuses = statement
        .query_map(params![from, now, account, sample_ms], |row| {
            Ok(json!({
                "account": row.get::<_, String>(0)?, "model": row.get::<_, String>(1)?,
                "at": row.get::<_, i64>(2)?, "availability": row.get::<_, String>(3)?,
                "fiveHourUsed": row.get::<_, Option<f64>>(4)?, "weeklyUsed": row.get::<_, Option<f64>>(5)?,
                "fiveHourReset": row.get::<_, Option<i64>>(6)?, "weeklyReset": row.get::<_, Option<i64>>(7)?
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let oldest: Option<i64> = conn.query_row("SELECT MIN(at_ms) FROM requests", [], |row| row.get(0))?;
    let oldest_status: Option<i64> = conn.query_row("SELECT MIN(at_ms) FROM account_status", [], |row| row.get(0))?;
    let lost = lost.max(meta(&conn, "lost_events")?.unwrap_or(0).max(0) as u64);
    let coverage_uncertain = meta(&conn, "uncertain_at")?.is_some_and(|at| at >= from && at <= now);
    let result = json!({"range": range.name(), "from": from, "to": now, "bucketMs": bucket_ms,
        "buckets": buckets, "accountBuckets": account_buckets, "errors": errors, "models": models,
        "modelsTruncated": models_truncated, "statuses": statuses,
        "oldestRequest": oldest, "oldestStatus": oldest_status, "lostEvents": lost,
        "coverageUncertain": coverage_uncertain});
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lost_records_and_unclean_restarts_remain_visible() {
        let dir = std::env::temp_dir().join(format!("native-history-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("history.sqlite");
        let lost = Arc::new(AtomicU64::new(0));
        {
            let history = History::start(path.clone(), lost.clone()).unwrap();
            history.record_event(&json!({"type":"inference","requestId":"one",
                "at":chrono::Utc::now().to_rfc3339(),"outcome":"success"}));
            history.flush();
            lost.fetch_add(2, Ordering::Relaxed);
        }
        let restarted = Arc::new(AtomicU64::new(0));
        {
            let history = History::start(path.clone(), restarted.clone()).unwrap();
            assert_eq!(restarted.load(Ordering::Relaxed), 2);
            let data = query(&path, Range::Day, None, epoch_ms(), 0).unwrap();
            assert_eq!(data["lostEvents"], 2);
            assert_eq!(data["coverageUncertain"], false);
            drop(history);
        }
        let conn = Connection::open(&path).unwrap();
        set_meta(&conn, "dirty", 1).unwrap();
        drop(conn);
        let history = History::start(path.clone(), Arc::new(AtomicU64::new(0))).unwrap();
        let data = query(&path, Range::Day, None, epoch_ms(), 0).unwrap();
        assert_eq!(data["lostEvents"], 2);
        assert_eq!(data["coverageUncertain"], true);
        drop(history);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn history_survives_restart_and_never_stores_session_or_body_fingerprint() {
        let dir = std::env::temp_dir().join(format!("native-history-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("history.sqlite");
        let lost = Arc::new(AtomicU64::new(0));
        let at = epoch_ms() - 60_000;
        let stamp = chrono::DateTime::from_timestamp_millis(at).unwrap().to_rfc3339();
        {
            let history = History::start(path.clone(), lost.clone()).unwrap();
            history.record_event(&json!({"type":"inference","requestId":"synthetic-1","at":stamp,
                "session":"private-session","bodySha256":"private-digest","account":"account-a",
                "model":"claude-sonnet-4-6","outcome":"success","status":200,
                "usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":10,
                    "cache_creation_input_tokens":5,"cache_creation":{"ephemeral_5m_input_tokens":5}}}));
            history.record_status(&json!({"observedAt":at,"model":"claude","accounts":[{
                "id":"account-a","availability":"included","includedQuota":{
                    "fiveHour":30,"weekly":45,"fiveHourResetsAt":stamp}}]}));
            history.flush();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                for suffix in ["sqlite-wal", "sqlite-shm"] {
                    let sidecar = path.with_extension(suffix);
                    if let Ok(meta) = std::fs::metadata(sidecar) {
                        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
                    }
                }
            }
        }
        let data = query(&path, Range::Day, None, epoch_ms(), 0).unwrap();
        assert_eq!(data["buckets"][0]["requests"], 1);
        assert_eq!(data["models"][0]["inputTokens"], 100);
        assert_eq!(data["models"][0]["reportedCacheWrite"], 1);
        assert_eq!(data["statuses"][0]["fiveHourUsed"], 30.0);
        let bytes = std::fs::read(&path).unwrap();
        assert!(!cpa_common::gostr::contains(&bytes, b"private-session"));
        assert!(!cpa_common::gostr::contains(&bytes, b"private-digest"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejected_requests_count_as_failures_and_account_filter_is_exact() {
        let dir = std::env::temp_dir().join(format!("native-history-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("history.sqlite");
        let history = History::start(path.clone(), Arc::new(AtomicU64::new(0))).unwrap();
        let stamp = chrono::Utc::now().to_rfc3339();
        for (id, account, outcome) in [("one", "a", "success"), ("two", "b", "quota_unavailable")] {
            history.record_event(&json!({"type":"inference","requestId":id,"at":stamp,
                "account":account,"model":"claude-sonnet-4-6","outcome":outcome}));
        }
        history.flush();
        let all = query(&path, Range::Day, None, epoch_ms(), 0).unwrap();
        assert_eq!(all["buckets"][0]["requests"], 2);
        assert_eq!(all["buckets"][0]["failures"], 1);
        assert_eq!(all["errors"][0]["outcome"], "quota_unavailable");
        let filtered = query(&path, Range::Day, Some("a"), epoch_ms(), 0).unwrap();
        assert_eq!(filtered["buckets"][0]["requests"], 1);
        assert_eq!(filtered["buckets"][0]["failures"], 0);
        drop(history);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn model_usage_counts_only_reported_counters() {
        let dir = std::env::temp_dir().join(format!("native-history-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("history.sqlite");
        let history = History::start(path.clone(), Arc::new(AtomicU64::new(0))).unwrap();
        let stamp = chrono::Utc::now().to_rfc3339();
        for (id, usage) in [
            ("one", json!({"input_tokens":10,"cache_creation_input_tokens":5})),
            ("two", json!({"input_tokens":20})),
        ] {
            history.record_event(&json!({"type":"inference","requestId":id,"at":stamp,
                "model":"claude-sonnet-4-6","outcome":"success","usage":usage}));
        }
        history.flush();
        let data = query(&path, Range::Day, None, epoch_ms(), 0).unwrap();
        assert_eq!(data["models"][0]["inputTokens"], 30);
        assert_eq!(data["models"][0]["reportedInput"], 2);
        assert_eq!(data["models"][0]["cacheWriteTokens"], 5);
        assert_eq!(data["models"][0]["reportedCacheWrite"], 1);
        drop(history);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retention_caps_keep_the_newest_records() {
        let dir = std::env::temp_dir().join(format!("native-history-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("history.sqlite");
        let history = History::start(path.clone(), Arc::new(AtomicU64::new(0))).unwrap();
        let now = epoch_ms();
        for i in 0..3 {
            let at = now - (3 - i) * 1_000_000;
            let stamp = chrono::DateTime::from_timestamp_millis(at).unwrap().to_rfc3339();
            history.record_event(&json!({"type":"inference","requestId":format!("r{i}"),
                "at":stamp,"outcome":"success"}));
            history.record_status(&json!({"observedAt":at,"model":"claude","accounts":[{
                "id":"a","availability":"included"}]}));
        }
        history.flush();
        drop(history);
        let conn = Connection::open(&path).unwrap();
        trim(&conn, 2, 2).unwrap();
        let requests: Vec<String> = conn
            .prepare("SELECT request_id FROM requests ORDER BY at_ms")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(requests, ["r1", "r2"]);
        let statuses: i64 = conn
            .query_row("SELECT COUNT(*) FROM account_status", [], |row| row.get(0))
            .unwrap();
        assert_eq!(statuses, 2);
        drop(conn);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
