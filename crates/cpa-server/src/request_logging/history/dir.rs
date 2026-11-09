//! A history folder's layout and retention: `<dir>/date=YYYY-MM-DD/<HHMMSS>-<pid>-<seq>.parquet`
//! (UTC), in-progress files as `.partial`.

use std::io;
use std::path::{Path, PathBuf};

use super::lock::{DirLock, is_abandoned};

/// A finished history file under a history folder.
pub(crate) struct HistoryFile {
    pub(crate) partition: String,
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) len: u64,
}

/// A history folder, and the operations on its files.
#[derive(Clone, Copy)]
pub(crate) struct HistoryDir<'a>(&'a Path);

impl<'a> HistoryDir<'a> {
    pub(crate) fn new(path: &'a Path) -> Self {
        Self(path)
    }

    pub(crate) fn path(self) -> &'a Path {
        self.0
    }

    /// Its lock, `None` while another holder has it.
    pub(crate) fn lock(self) -> io::Result<Option<DirLock>> {
        DirLock::try_acquire(self.0)
    }

    /// The finished history files, oldest first by partition date and name.
    pub(crate) fn files(self) -> io::Result<Vec<HistoryFile>> {
        let mut out = Vec::new();
        for partition in std::fs::read_dir(self.0)? {
            let partition = partition?;
            let partition_name = partition.file_name().to_string_lossy().into_owned();
            if !partition_name.starts_with("date=") || !partition.file_type()?.is_dir() {
                continue;
            }
            for file in std::fs::read_dir(partition.path())? {
                let file = file?;
                let name = file.file_name().to_string_lossy().into_owned();
                if !is_history_file(&name) {
                    continue;
                }
                let len = file.metadata()?.len();
                out.push(HistoryFile {
                    partition: partition_name.clone(),
                    name,
                    path: file.path(),
                    len,
                });
            }
        }
        out.sort_by(|a, b| (&a.partition, &a.name).cmp(&(&b.partition, &b.name)));
        Ok(out)
    }

    /// Removes `date=` partitions that hold nothing.
    pub(crate) fn remove_empty_partitions(self) {
        let Ok(entries) = std::fs::read_dir(self.0) else { return };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("date=") {
                // Fails while the partition still holds anything, which is the point.
                let _ = std::fs::remove_dir(entry.path());
            }
        }
    }

    /// [`Self::enforce_cap`] under the lock; skipped this round while a delete holds it.
    pub(crate) fn apply_cap(self, cap: u64) {
        match self.lock() {
            Ok(Some(_lock)) => {
                if let Err(error) = self.enforce_cap(cap) {
                    tracing::warn!("history: cannot apply max-total-size-mb: {error}");
                }
            }
            Ok(None) => {}
            Err(error) => tracing::warn!("history: cannot lock {}: {error}", self.0.display()),
        }
    }

    /// Deletes the oldest history files, by partition date then name, until the files
    /// total at most `cap` bytes. Returns the bytes freed.
    pub(crate) fn enforce_cap(self, cap: u64) -> io::Result<u64> {
        let files = self.files()?;
        let mut total: u64 = files.iter().map(|f| f.len).sum();
        let mut freed = 0;
        for file in files {
            if total <= cap {
                break;
            }
            if std::fs::remove_file(&file.path).is_ok() {
                total -= file.len;
                freed += file.len;
            }
        }
        self.remove_empty_partitions();
        Ok(freed)
    }

    /// Removes in-progress files no writer holds, except `open`, this writer's own:
    /// probing it would release its lock where the OS emulates flock with process-wide
    /// locks.
    pub(crate) fn remove_dead_partials(self, open: Option<&Path>) {
        let Ok(partitions) = std::fs::read_dir(self.0) else {
            return;
        };
        let mut removed = 0u64;
        for partition in partitions.flatten() {
            if !partition.file_name().to_string_lossy().starts_with("date=") {
                continue;
            }
            let Ok(files) = std::fs::read_dir(partition.path()) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                if is_partial_file(&file.file_name().to_string_lossy())
                    && open != Some(path.as_path())
                    && is_abandoned(&path).unwrap_or(false)
                    && std::fs::remove_file(&path).is_ok()
                {
                    removed += 1;
                }
            }
        }
        if removed > 0 {
            tracing::warn!("history: removed {removed} unfinished file(s) left by a writer that stopped");
        }
    }
}

/// Whether `name` is an in-progress history file this module wrote.
fn is_partial_file(name: &str) -> bool {
    name.strip_suffix(".partial").is_some_and(is_stem)
}

/// Whether `name` is a finished history file this module wrote.
pub(crate) fn is_history_file(name: &str) -> bool {
    name.strip_suffix(".parquet").is_some_and(is_stem)
}

/// `HHMMSS-<pid>-<seq>`, with an optional `-<n>` suffix from a name collision.
fn is_stem(stem: &str) -> bool {
    let mut parts = stem.split('-');
    let (Some(time), Some(pid), Some(seq)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    time.len() == 6
        && [time, pid, seq]
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        && parts.all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
#[path = "dir_tests.rs"]
mod tests;
