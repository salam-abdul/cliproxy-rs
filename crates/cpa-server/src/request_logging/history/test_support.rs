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

/// A Claude send whose request carries a thread start with the tool `tool` defined and
/// whose buffered response calls it as `call`.
pub(crate) fn claude_row(id: &str, session: &str, started_us: i64, tool: &str, call: &str) -> Row {
    Row {
        exchange_id: id.into(),
        session_id: Some(session.into()),
        started_at_us: started_us,
        provider: Some("claude".into()),
        request_body: Some(format!(
            r#"{{"tools":[{{"name":"{tool}"}}],"messages":[{{"role":"user","content":"start {id}"}}]}}"#
        )),
        response_body: Some(format!(
            r#"{{"id":"msg_{id}","content":[{{"type":"tool_use","id":"{call}","name":"{tool}","input":{{}}}}]}}"#
        )),
        ..Row::default()
    }
}

/// The tool calls in the tool index under `dir`, as (call ID, tool), sorted; none
/// without an index.
pub(crate) fn indexed_calls(dir: &Path) -> Vec<(String, String)> {
    let path = dir.join(super::tools::store::INDEX_FILE);
    if !path.is_file() {
        return Vec::new();
    }
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut stmt = conn.prepare("SELECT id, tool FROM calls").unwrap();
    let mut named: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((call_name(r.get(0)?), r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    named.sort();
    named
}

/// The call ID among the synthetic ones the history tests use whose hash is `id`.
fn call_name(id: i64) -> String {
    (0..100)
        .flat_map(|n| ["a", "b", "c", "d"].map(|p| format!("toolu_{p}{n}")))
        .find(|name| super::tools::hash(name.as_bytes()) == id)
        .unwrap_or_else(|| format!("#{id}"))
}
