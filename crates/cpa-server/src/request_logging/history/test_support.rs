//! Helpers shared by the history tests.

use std::path::{Path, PathBuf};

use super::body::{Bodies, Budget, HttpBodies, PendingRow};
use super::dir::HistoryDir;
use super::file::read_rows;
use super::row::Row;

/// A fresh directory under the system temp dir, removed on drop.
pub(crate) struct TempDir(pub(crate) PathBuf);

impl TempDir {
    pub(crate) fn new() -> Self {
        Self(std::env::temp_dir().join(format!("cpa-history-{}", uuid::Uuid::new_v4())))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The finished history files under `dir`, oldest first; none when it does not exist.
pub(crate) fn files(dir: &Path) -> Vec<PathBuf> {
    HistoryDir::new(dir)
        .files()
        .map(|files| files.into_iter().map(|f| f.path).collect())
        .unwrap_or_default()
}

/// Every row in the finished files under `dir`, file by file.
pub(crate) fn rows(dir: &Path) -> Vec<Row> {
    files(dir).iter().flat_map(|path| read_rows(path).unwrap()).collect()
}

/// `row` as an HTTP send with empty bodies.
pub(crate) fn http(row: Row, budget: &Budget) -> PendingRow {
    PendingRow {
        row,
        bodies: Bodies::Http(HttpBodies::new(budget)),
    }
}

/// The HTTP bodies of `pending`, which [`http`] made.
pub(crate) fn http_bodies(pending: &mut PendingRow) -> &mut HttpBodies {
    match &mut pending.bodies {
        Bodies::Http(http) => http,
        Bodies::Websocket { .. } => panic!("not an HTTP row"),
    }
}

/// Logs on this thread, captured while it lives.
pub(crate) struct Logs {
    text: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    _guard: tracing::subscriber::DefaultGuard,
}

impl Logs {
    pub(crate) fn capture() -> Self {
        #[derive(Clone)]
        struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let text = std::sync::Arc::default();
        let sink = Sink(std::sync::Arc::clone(&text));
        let guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(move || sink.clone())
                .finish(),
        );
        Self { text, _guard: guard }
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.text.lock().unwrap()).into_owned()
    }
}
