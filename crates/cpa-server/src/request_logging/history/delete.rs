//! Deleting selected rows from history (`cliproxy history delete`). Finished files are
//! deleted whole or rewritten without the matching rows. Every delete also leaves a
//! tombstone in `<dir>/.deletes/`: a running server drops the matching rows it still
//! holds or has yet to receive, as long as they started before the delete, and the
//! tombstone expires after a day.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use super::dir::HistoryDir;
use super::file::{read_keys, rewrite_without};
use super::row::Row;

/// Which rows to delete: every given condition must hold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selector {
    /// Rows that started at or after this time.
    pub from: Option<DateTime<Utc>>,
    /// Rows that started before this time.
    pub to: Option<DateTime<Utc>>,
    pub session: Option<String>,
    pub account: Option<String>,
    /// The upstream model or the model the client asked for.
    pub model: Option<String>,
}

impl Selector {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    pub(crate) fn matches(&self, row: &Row) -> bool {
        let text = |want: &Option<String>, have: &Option<String>| want.is_none() || want == have;
        self.from
            .is_none_or(|from| row.started_at_us >= from.timestamp_micros())
            && self.to.is_none_or(|to| row.started_at_us < to.timestamp_micros())
            && text(&self.session, &row.session_id)
            && text(&self.account, &row.account)
            && (self.model.is_none() || self.model == row.model || self.model == row.model_requested)
    }
}

/// RFC 3339, or `YYYY-MM-DD` for midnight UTC.
pub fn parse_time(text: &str) -> anyhow::Result<DateTime<Utc>> {
    let text = text.trim();
    if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return Ok(date.and_time(chrono::NaiveTime::MIN).and_utc());
    }
    DateTime::parse_from_rfc3339(text)
        .map(|t| t.with_timezone(&Utc))
        .with_context(|| format!("{text:?} is neither YYYY-MM-DD nor an RFC 3339 time"))
}

/// What a delete removed, or would remove with `dry_run`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub files_deleted: usize,
    pub files_rewritten: usize,
    pub rows_deleted: u64,
    /// Bytes freed. A dry run counts only whole files.
    pub bytes_freed: u64,
    /// A tombstone was left, so a running server drops matching rows it has not
    /// written yet.
    pub pending_for_writer: bool,
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} row(s) in {} deleted and {} rewritten file(s), {} byte(s) freed",
            self.rows_deleted, self.files_deleted, self.files_rewritten, self.bytes_freed
        )?;
        if self.pending_for_writer {
            write!(f, "; a running server drops matching rows it has not written yet")?;
        }
        Ok(())
    }
}

/// Deletes the rows `selector` matches from the history under `dir`, among rows that
/// started before this call.
pub fn delete(dir: &Path, selector: &Selector, dry_run: bool) -> anyhow::Result<Report> {
    if selector.is_empty() {
        bail!("history delete needs at least one of --from, --to, --session, --account or --model");
    }
    if let (Some(from), Some(to)) = (selector.from, selector.to)
        && from >= to
    {
        bail!("history delete needs --from earlier than --to");
    }
    let mut report = Report::default();
    if dry_run && !dir.exists() {
        return Ok(report);
    }
    let asked = Utc::now();
    let cutoff = asked.timestamp_micros();
    // Earlier deletes still pending go too: a server that could not apply them may
    // have stopped since.
    let history = HistoryDir::new(dir);
    let pending = history.tombstones();
    let matches = |row: &Row| {
        (row.started_at_us < cutoff && selector.matches(row)) || pending.iter().any(|stone| stone.matches(row))
    };
    // A server may be buffering its first rows with nothing on disk yet.
    cpa_core::private_fs::create_dir_all(dir)?;
    let _lock = if dry_run {
        None
    } else {
        Some(
            history
                .lock()?
                .with_context(|| format!("{} is held by another process; try again", dir.join(".lock").display()))?,
        )
    };
    if !dry_run {
        // Always: a server may hold matching rows in memory, or have sends in flight,
        // with no file on disk yet.
        history.expire_tombstones(asked);
        history.write_tombstone(selector, cutoff)?;
        report.pending_for_writer = true;
    }
    for file in history.files()? {
        // A row lands in the partition of the file's first row, never an earlier one.
        if let (Some(from), Some(date)) = (selector.from, partition_date(&file.partition))
            && date
                .succ_opt()
                .is_some_and(|next| next.and_time(chrono::NaiveTime::MIN).and_utc() <= from)
        {
            continue;
        }
        let keys = read_keys(&file.path).with_context(|| format!("read {}", file.path.display()))?;
        let gone = keys.iter().filter(|row| matches(row)).count() as u64;
        if gone == 0 {
            continue;
        }
        report.rows_deleted += gone;
        if gone == keys.len() as u64 {
            report.files_deleted += 1;
            report.bytes_freed += file.len;
            if !dry_run {
                std::fs::remove_file(&file.path)?;
            }
        } else {
            report.files_rewritten += 1;
            if !dry_run {
                rewrite_without(&file.path, matches).with_context(|| format!("rewrite {}", file.path.display()))?;
                report.bytes_freed += file.len.saturating_sub(std::fs::metadata(&file.path)?.len());
            }
        }
    }
    if !dry_run {
        history.remove_empty_partitions();
    }
    Ok(report)
}

fn partition_date(name: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(name.strip_prefix("date=")?, "%Y-%m-%d").ok()
}

/// Removes the rows `stones` match from the finished file at `path`, reading only the
/// match columns unless a row matches.
pub(crate) fn apply_tombstones(path: &Path, stones: &[&Tombstone]) -> io::Result<()> {
    let matches = |row: &Row| stones.iter().any(|stone| stone.matches(row));
    if read_keys(path)?.iter().any(matches) {
        rewrite_without(path, matches)?;
    }
    Ok(())
}

const TOMBSTONES: &str = ".deletes";
/// Longer than any send can take to start, finish and wait in a writer's buffer.
const TOMBSTONE_TTL: chrono::TimeDelta = chrono::TimeDelta::hours(24);

/// A pending delete for rows a running server had not yet written to a finished file.
pub(crate) struct Tombstone {
    pub(crate) path: PathBuf,
    selector: Selector,
    /// Only rows that started before the delete was asked for.
    pub(crate) created_us: i64,
}

impl Tombstone {
    pub(crate) fn matches(&self, row: &Row) -> bool {
        row.started_at_us < self.created_us && self.selector.matches(row)
    }
}

/// A tombstone as stored: times in microseconds since the epoch.
#[derive(Serialize, Deserialize)]
struct TombstoneRecord {
    created: i64,
    from: Option<i64>,
    to: Option<i64>,
    session: Option<String>,
    account: Option<String>,
    model: Option<String>,
}

impl HistoryDir<'_> {
    fn write_tombstone(self, selector: &Selector, created_us: i64) -> anyhow::Result<()> {
        let folder = self.path().join(TOMBSTONES);
        // Tombstones name sessions and accounts: as private as the history itself.
        cpa_core::private_fs::create_dir_all(&folder)?;
        #[cfg(unix)]
        std::fs::set_permissions(&folder, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
        let time = |t: Option<DateTime<Utc>>| t.map(|t| t.timestamp_micros());
        let record = TombstoneRecord {
            created: created_us,
            from: time(selector.from),
            to: time(selector.to),
            session: selector.session.clone(),
            account: selector.account.clone(),
            model: selector.model.clone(),
        };
        let path = folder.join(format!("{}.json", uuid::Uuid::new_v4()));
        let partial = path.with_extension("tmp");
        std::io::Write::write_all(
            &mut cpa_core::private_fs::create_new(&partial)?,
            &serde_json::to_vec(&record)?,
        )?;
        std::fs::rename(&partial, &path)?;
        Ok(())
    }

    /// Removes tombstones older than a day.
    pub(crate) fn expire_tombstones(self, now: DateTime<Utc>) {
        let cutoff = (now - TOMBSTONE_TTL).timestamp_micros();
        for stone in self.tombstones() {
            if stone.created_us < cutoff {
                let _ = std::fs::remove_file(&stone.path);
            }
        }
    }

    /// Pending tombstones. Unreadable ones are skipped, never applied loosely.
    pub(crate) fn tombstones(self) -> Vec<Tombstone> {
        let Ok(entries) = std::fs::read_dir(self.path().join(TOMBSTONES)) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let Ok(record) = serde_json::from_slice::<TombstoneRecord>(&bytes) else {
                continue;
            };
            let time = |t: Option<i64>| t.and_then(DateTime::from_timestamp_micros);
            let selector = Selector {
                from: time(record.from),
                to: time(record.to),
                session: record.session,
                account: record.account,
                model: record.model,
            };
            if !selector.is_empty() {
                out.push(Tombstone {
                    path,
                    selector,
                    created_us: record.created,
                });
            }
        }
        out
    }
}

#[cfg(test)]
#[path = "delete_tests.rs"]
mod tests;
