//! The tool index file, `<history dir>/tool-index.sqlite`: the facts of every indexed
//! history file, kept by the file's path relative to the history folder so a file
//! removed or rewritten by a delete can lose exactly its facts.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::Path;
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::scan::{Conv, SendFacts};

pub(crate) const INDEX_FILE: &str = "tool-index.sqlite";

/// How long a file the size cap removed keeps its facts: 31 days.
pub(crate) const RETAIN_CAPPED_MS: i64 = 2_678_400_000;

/// What a history file looked like when it was read: its length, modification time and
/// inode (0 off Unix). A delete rewrites a file under the same name, so the name alone
/// cannot tell a rewritten file from the one that was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fingerprint {
    /// The values exactly as stored: the length and inode bit for bit, the modification
    /// time in nanoseconds since the epoch.
    len: i64,
    modified_ns: i64,
    ino: i64,
}

impl Fingerprint {
    /// The fingerprint of an open handle, so it describes exactly the file being read.
    pub(crate) fn from_file(file: &File) -> std::io::Result<Self> {
        Ok(Self::of(&file.metadata()?))
    }

    pub(crate) fn from_path(path: &Path) -> std::io::Result<Self> {
        Ok(Self::of(&std::fs::metadata(path)?))
    }

    fn of(meta: &std::fs::Metadata) -> Self {
        // Outside 1677 to 2262 the time reads as 0, the same on every read.
        let modified_ns = meta
            .modified()
            .ok()
            .and_then(|at| match at.duration_since(SystemTime::UNIX_EPOCH) {
                Ok(after) => i64::try_from(after.as_nanos()).ok(),
                Err(before) => i64::try_from(before.duration().as_nanos()).ok().map(|ns| -ns),
            })
            .unwrap_or(0);
        #[cfg(unix)]
        let ino = std::os::unix::fs::MetadataExt::ino(meta);
        #[cfg(not(unix))]
        let ino = 0u64;
        Self {
            len: i64::from_ne_bytes(meta.len().to_ne_bytes()),
            modified_ns,
            ino: i64::from_ne_bytes(ino.to_ne_bytes()),
        }
    }
}

/// The version of the facts' rules and tables. An index written under another version
/// is emptied and every file indexed again.
const VERSION: i64 = 2;

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS files (
        rel TEXT PRIMARY KEY, len INTEGER NOT NULL, modified_ns INTEGER NOT NULL, ino INTEGER NOT NULL,
        capped_ms INTEGER, unreadable INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS sends (
        rel TEXT NOT NULL, at_ms INTEGER NOT NULL, provider TEXT, conv INTEGER,
        skipped_format INTEGER NOT NULL, cut INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS sends_by_rel ON sends(rel);
    CREATE INDEX IF NOT EXISTS sends_by_time ON sends(at_ms);
    CREATE TABLE IF NOT EXISTS messages (
        message INTEGER NOT NULL, rel TEXT NOT NULL, conv INTEGER NOT NULL,
        PRIMARY KEY(message, rel)
    );
    CREATE INDEX IF NOT EXISTS messages_by_rel ON messages(rel);
    CREATE TABLE IF NOT EXISTS defs (
        rel TEXT NOT NULL, conv INTEGER NOT NULL, provider TEXT NOT NULL, tool TEXT NOT NULL,
        bytes INTEGER NOT NULL,
        PRIMARY KEY(rel, conv, provider, tool)
    );
    CREATE INDEX IF NOT EXISTS defs_by_conv ON defs(conv);
    CREATE TABLE IF NOT EXISTS calls (
        rel TEXT NOT NULL, id INTEGER NOT NULL, at_ms INTEGER NOT NULL, provider TEXT, conv INTEGER,
        tool TEXT NOT NULL, skill TEXT,
        PRIMARY KEY(rel, id)
    );
    CREATE INDEX IF NOT EXISTS calls_by_time ON calls(at_ms);
    CREATE INDEX IF NOT EXISTS calls_by_id ON calls(id);
    CREATE TABLE IF NOT EXISTS results (
        rel TEXT NOT NULL, id INTEGER NOT NULL, at_ms INTEGER NOT NULL, is_error INTEGER NOT NULL,
        bytes INTEGER NOT NULL, loaded INTEGER NOT NULL,
        PRIMARY KEY(rel, id)
    );
    CREATE INDEX IF NOT EXISTS results_by_id ON results(id);
";

/// Every table that holds a file's facts, `files` last.
const TABLES: [&str; 6] = ["sends", "messages", "defs", "calls", "results", "files"];

/// The open index.
pub(crate) struct Index {
    conn: Connection,
    /// Run inside the next commit's transaction, after the file was checked.
    #[cfg(test)]
    hold: Option<Box<dyn FnOnce() + Send>>,
}

impl Index {
    /// Opens the index in the history folder `dir`, creating the file private (`0600`)
    /// when it does not exist. The folder must exist.
    pub(crate) fn open(dir: &Path) -> anyhow::Result<Index> {
        let path = dir.join(INDEX_FILE);
        cpa_core::private_fs::secure_file(&path)?;
        let mut conn = Connection::open(&path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if version(&tx)? != VERSION {
            for table in TABLES {
                tx.execute(&format!("DROP TABLE IF EXISTS {table}"), [])?;
            }
            tx.pragma_update(None, "user_version", VERSION)?;
        }
        tx.execute_batch(SCHEMA)?;
        tx.commit()?;
        Ok(Index {
            conn,
            #[cfg(test)]
            hold: None,
        })
    }

    /// Every indexed file by its path relative to the history folder, with the
    /// fingerprint it was read with and when the size cap removed it.
    pub(crate) fn indexed(&self) -> rusqlite::Result<HashMap<String, (Fingerprint, Option<i64>)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT rel, len, modified_ns, ino, capped_ms FROM files")?;
        stmt.query_map([], |r| {
            let fingerprint = Fingerprint {
                len: r.get(1)?,
                modified_ns: r.get(2)?,
                ino: r.get(3)?,
            };
            Ok((r.get(0)?, (fingerprint, r.get(4)?)))
        })?
        .collect()
    }

    /// Replaces the facts of the file `rel` at `abs` with `sends`, in file order. Writes
    /// nothing and returns false when the file on disk no longer has the fingerprint
    /// `read_as` of the handle that was read, or is gone: a delete changed it after the
    /// read, and its facts may hold deleted rows. The check and the write share one
    /// immediate transaction, so a delete's forget cannot slip between them.
    pub(crate) fn commit(
        &mut self,
        rel: &str,
        read_as: &Fingerprint,
        abs: &Path,
        sends: &[SendFacts],
        unreadable: bool,
    ) -> rusqlite::Result<bool> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if Fingerprint::from_path(abs).ok().as_ref() != Some(read_as) {
            return Ok(false);
        }
        #[cfg(test)]
        if let Some(hold) = self.hold.take() {
            hold();
        }
        for table in TABLES {
            tx.execute(&format!("DELETE FROM {table} WHERE rel = ?1"), [rel])?;
        }
        tx.execute(
            "INSERT INTO files VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
            params![rel, read_as.len, read_as.modified_ns, read_as.ino, unreadable],
        )?;
        {
            let mut parent = tx.prepare("SELECT conv FROM messages WHERE message = ?1 ORDER BY rel LIMIT 1")?;
            let mut send_row = tx.prepare("INSERT INTO sends VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?;
            let mut message = tx.prepare("INSERT OR IGNORE INTO messages VALUES (?1, ?2, ?3)")?;
            let mut def = tx.prepare(
                "INSERT INTO defs VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(rel, conv, provider, tool) DO UPDATE SET bytes = max(bytes, excluded.bytes)",
            )?;
            let mut call = tx.prepare("INSERT OR IGNORE INTO calls VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")?;
            let mut result = tx.prepare("INSERT OR IGNORE INTO results VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?;
            for send in sends {
                let conv: Option<i64> = match send.conv {
                    Conv::Start(key) => Some(key),
                    Conv::Continue(of) => parent.query_row([of], |r| r.get(0)).optional()?,
                    Conv::None => None,
                };
                send_row.execute(params![
                    rel,
                    send.at_ms,
                    send.provider,
                    conv,
                    send.skipped_format,
                    send.cut
                ])?;
                if let Some(conv) = conv {
                    if let Some(id) = send.message {
                        message.execute(params![id, rel, conv])?;
                    }
                    let provider = send.provider.as_deref().unwrap_or("");
                    for d in &send.defs {
                        def.execute(params![rel, conv, provider, d.tool, d.bytes])?;
                    }
                }
                for c in &send.calls {
                    call.execute(params![rel, c.id, send.at_ms, send.provider, conv, c.tool, c.skill])?;
                }
                for r in &send.results {
                    result.execute(params![rel, r.id, send.at_ms, r.is_error, r.bytes, r.loaded])?;
                }
            }
        }
        tx.commit()?;
        Ok(true)
    }

    /// Drops every fact of the file `rel`.
    pub(crate) fn forget(&mut self, rel: &str) -> rusqlite::Result<()> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for table in TABLES {
            tx.execute(&format!("DELETE FROM {table} WHERE rel = ?1"), [rel])?;
        }
        tx.commit()
    }

    /// Every indexed file the size cap removed.
    pub(crate) fn capped_files(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT rel FROM files WHERE capped_ms IS NOT NULL")?;
        stmt.query_map([], |r| r.get(0))?.collect()
    }

    /// Marks the file `rel` as removed by the size cap at `now_ms`: its facts stay for
    /// [`RETAIN_CAPPED_MS`].
    pub(crate) fn capped(&mut self, rel: &str, now_ms: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE files SET capped_ms = ?2 WHERE rel = ?1 AND capped_ms IS NULL",
            params![rel, now_ms],
        )?;
        Ok(())
    }

    /// Forgets every capped file removed [`RETAIN_CAPPED_MS`] or more ago, and every
    /// indexed file that is not in `on_disk`, was not removed by the size cap, and was
    /// already missing at the previous sweep (`missing`): a cap removal reaches the index
    /// one pass late when it happens during a pass. Returns the files missing now that
    /// were not forgotten, for the next sweep.
    pub(crate) fn sweep(
        &mut self,
        on_disk: &HashSet<String>,
        now_ms: i64,
        missing: &HashSet<String>,
    ) -> rusqlite::Result<HashSet<String>> {
        let mut gone = Vec::new();
        let mut still = HashSet::new();
        for (rel, (_, capped)) in self.indexed()? {
            match capped {
                None if on_disk.contains(&rel) => {}
                None if missing.contains(&rel) => gone.push(rel),
                None => {
                    still.insert(rel);
                }
                Some(at) if now_ms.saturating_sub(at) >= RETAIN_CAPPED_MS => gone.push(rel),
                Some(_) => {}
            }
        }
        for rel in gone {
            self.forget(&rel)?;
        }
        Ok(still)
    }
}

#[cfg(test)]
impl Index {
    pub(crate) fn set_hold(&mut self, hold: Option<Box<dyn FnOnce() + Send>>) {
        self.hold = hold;
    }
}

/// The path of the history file `file` relative to the history folder `dir`, with `/`
/// between its parts, as the index keys it.
pub(crate) fn rel_path(dir: &Path, file: &Path) -> Option<String> {
    let parts: Option<Vec<&str>> = file
        .strip_prefix(dir)
        .ok()?
        .components()
        .map(|part| part.as_os_str().to_str())
        .collect();
    Some(parts?.join("/"))
}

/// The tool index of the history folder `dir` for `cliproxy history delete`, which runs
/// while the server may be stopped; `None` when there is no index yet, so nothing is
/// created.
pub(crate) fn open_existing(dir: &Path) -> anyhow::Result<Option<Index>> {
    if !dir.join(INDEX_FILE).exists() {
        return Ok(None);
    }
    Ok(Some(Index::open(dir)?))
}

/// The `user_version` of an open index: [`VERSION`] once its tables are made.
fn version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.pragma_query_value(None, "user_version", |r| r.get(0))
}

/// Whether the open index `conn` was written under this version's rules. A file the
/// indexer is still creating, or one from another version, holds no facts to read.
pub(crate) fn is_current(conn: &Connection) -> rusqlite::Result<bool> {
    Ok(version(conn)? == VERSION)
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
