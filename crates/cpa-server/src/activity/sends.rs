use rusqlite::{Connection, OpenFlags, params, types::ValueRef};
use serde_json::{Map, Value};
use std::time::Duration;

use super::{Activity, Command, bounded};
use crate::request_logging::history::row::{Outcome, Source};
use crate::usage_record::{Detail, ensure_breakdown, has_tokens};

/// Which measurement a send's time to first response is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TtftKind {
    Byte,
    Token,
    Event,
}

impl TtftKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Byte => "byte",
            Self::Token => "token",
            Self::Event => "event",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tokens {
    pub uncached_input: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    /// Includes reasoning.
    pub output: i64,
    pub reasoning: i64,
    /// False when the provider's counts made no complete breakdown.
    pub exact: bool,
}

impl Tokens {
    /// The provider's counts in one meaning: cache inside neither input nor output, reasoning
    /// inside output. Counts that make no complete breakdown are kept as reported and marked
    /// inexact.
    pub(crate) fn of(detail: &Detail, provider: &str, executor_type: &str) -> Option<Self> {
        if !has_tokens(detail) {
            return None;
        }
        let parts = ensure_breakdown(detail.clone(), provider, executor_type)
            .breakdown
            .and_then(|b| b.parts());
        Some(match parts {
            Some([uncached_input, cache_read, cache_write, output, reasoning]) => Self {
                uncached_input,
                cache_read,
                cache_write,
                output,
                reasoning,
                exact: true,
            },
            None => Self {
                uncached_input: detail.input,
                cache_read: detail.cache_read,
                cache_write: detail.cache_creation,
                output: detail.output,
                reasoning: detail.reasoning,
                exact: false,
            },
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SendRow {
    pub request_id: String,
    pub turn: i32,
    pub send: i32,
    /// The request this send serves in place of its own: a native request's quota fallback.
    pub parent_request: Option<String>,
    /// When the send started, Unix milliseconds.
    pub at_ms: i64,
    pub source: Source,
    pub provider: String,
    pub credential: Option<String>,
    pub model: Option<String>,
    pub outcome: Outcome,
    pub status: Option<i64>,
    pub stream: bool,
    /// From the request's arrival to this send's start.
    pub start_ms: i64,
    pub latency_ms: Option<i64>,
    pub ttft: Option<(i64, TtftKind)>,
    pub tokens: Option<Tokens>,
}

impl SendRow {
    /// Applies the store's string bound. A row without a storable request ID or provider
    /// cannot be written.
    fn bounded(mut self) -> Option<Self> {
        self.request_id = bounded(Some(&self.request_id))?;
        self.provider = bounded(Some(&self.provider))?;
        self.parent_request = bounded(self.parent_request.as_deref());
        self.credential = bounded(self.credential.as_deref());
        self.model = bounded(self.model.as_deref());
        Some(self)
    }
}

impl Activity {
    pub(crate) fn record_send(&self, row: SendRow) {
        match row.bounded() {
            Some(row) => self.send(Command::Send(Box::new(row))),
            None => {
                self.lost.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    /// Every `sends` row ordered by `at_ms, request_id, turn, send`, as objects keyed by
    /// column name.
    pub(crate) fn sends(&self) -> anyhow::Result<Vec<Value>> {
        let db = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        db.busy_timeout(Duration::from_secs(2))?;
        let mut statement = db.prepare("SELECT * FROM sends ORDER BY at_ms, request_id, turn, send")?;
        let names: Vec<String> = statement.column_names().into_iter().map(str::to_owned).collect();
        let rows = statement
            .query_map([], |row| {
                let mut object = Map::new();
                for (index, name) in names.iter().enumerate() {
                    let value = match row.get_ref(index)? {
                        ValueRef::Integer(n) => n.into(),
                        ValueRef::Real(n) => n.into(),
                        ValueRef::Text(text) => String::from_utf8_lossy(text).into_owned().into(),
                        ValueRef::Null | ValueRef::Blob(_) => Value::Null,
                    };
                    object.insert(name.clone(), value);
                }
                Ok(Value::Object(object))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

pub(super) fn write_send(conn: &Connection, row: &SendRow) -> rusqlite::Result<()> {
    let tokens = row.tokens.as_ref();
    conn.execute(
        "INSERT OR REPLACE INTO sends (
            request_id, turn, send, parent_request, at_ms, source, provider, credential, model,
            outcome, status, stream, start_ms, latency_ms, ttft_ms, ttft_kind,
            uncached_input_tokens, cache_read_tokens, cache_write_tokens,
            output_tokens, reasoning_tokens, tokens_exact
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
            ?17, ?18, ?19, ?20, ?21, ?22)",
        params![
            row.request_id,
            row.turn,
            row.send,
            row.parent_request,
            row.at_ms,
            row.source.as_str(),
            row.provider,
            row.credential,
            row.model,
            row.outcome.as_str(),
            row.status,
            row.stream,
            row.start_ms,
            row.latency_ms,
            row.ttft.map(|(ms, _)| ms),
            row.ttft.map(|(_, kind)| kind.as_str()),
            tokens.map(|t| t.uncached_input),
            tokens.map(|t| t.cache_read),
            tokens.map(|t| t.cache_write),
            tokens.map(|t| t.output),
            tokens.map(|t| t.reasoning),
            tokens.map(|t| t.exact),
        ],
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "sends_tests.rs"]
mod tests;
