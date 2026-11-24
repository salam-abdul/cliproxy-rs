//! How long completed requests took and where the time went, read from `sends`.
//!
//! A request is its sends under `COALESCE(parent_request, request_id)` and `turn`, so a
//! native request's quota fallback counts under the native request. Its last send decides
//! whether it counts (complete, started in range) and which provider, source, credential
//! and model it is filed under.

use rusqlite::{Connection, OpenFlags, Row, params};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::sends::TtftKind;
use super::{Activity, Range, epoch_ms, meta};

/// Earlier sends of a request are read from this long before the range, so a request
/// whose last send is just inside it still has its failed sends.
const EARLIER_MS: i64 = 3_600_000;

/// The columns [`Send::of`] reads, from 4 on; 0 to 3 key and order the send.
const COLUMNS: &str = "COALESCE(parent_request, request_id), turn, at_ms, parent_request IS NOT NULL, send,
    latency_ms, outcome = 'complete', source, provider, credential, model, stream, start_ms, ttft_ms, ttft_kind,
    tokens_exact IS 1, uncached_input_tokens, cache_read_tokens, cache_write_tokens, output_tokens";

impl Activity {
    /// Request speed over `range`, for requests whose last send matches `provider` and
    /// `credential` when given. `offset` is the viewer's minutes east of UTC, for the
    /// hour slice.
    pub(crate) async fn speed(
        &self,
        range: Range,
        provider: Option<String>,
        credential: Option<String>,
        offset: i32,
    ) -> anyhow::Result<Value> {
        let path = self.path.clone();
        let lost = self.lost.load(Ordering::Relaxed);
        tokio::task::spawn_blocking(move || {
            read(
                &path,
                range,
                provider.as_deref(),
                credential.as_deref(),
                offset,
                epoch_ms(),
                lost,
            )
        })
        .await?
    }
}

/// Strings stored once per read, named by index.
#[derive(Default)]
struct Names {
    ids: HashMap<Box<str>, u32>,
    names: Vec<Box<str>>,
}

impl Names {
    fn id(&mut self, name: &str) -> u32 {
        if let Some(id) = self.ids.get(name) {
            return *id;
        }
        let id = u32::try_from(self.names.len()).unwrap_or(u32::MAX);
        self.names.push(name.into());
        self.ids.insert(name.into(), id);
        id
    }

    fn name(&self, id: u32) -> &str {
        self.names.get(id as usize).map_or("", |name| name)
    }
}

/// A request's key: a 128-bit hash of its ID, with its turn. At a million sends this keeps
/// the map's keys small; two IDs share a hash with odds near 2^-89.
fn request_key(request: &str, turn: i64) -> (u128, i64) {
    let half = |salt: u8| {
        let mut hasher = DefaultHasher::new();
        salt.hash(&mut hasher);
        request.hash(&mut hasher);
        hasher.finish()
    };
    ((u128::from(half(0)) << 64) | u128::from(half(1)), turn)
}

/// Token counts from a send with an exact breakdown.
#[derive(Clone, Copy)]
struct Exact {
    /// Uncached input, cache read and cache write.
    input: i64,
    cache_read: i64,
    output: Option<i64>,
}

/// What a request's last send says about it.
#[derive(Clone, Copy)]
struct Send {
    /// `at_ms`, then a fallback after every native send of its request, then `send`.
    order: (i64, bool, i64),
    complete: bool,
    source: u32,
    provider: u32,
    credential: Option<u32>,
    model: Option<u32>,
    stream: bool,
    start: i64,
    latency: Option<i64>,
    ttft: Option<(i64, TtftKind)>,
    tokens: Option<Exact>,
}

impl Send {
    fn of(row: &Row, order: (i64, bool, i64), latency: Option<i64>, names: &mut Names) -> rusqlite::Result<Self> {
        let mut name = |index| -> rusqlite::Result<Option<u32>> {
            Ok(row.get_ref(index)?.as_str_or_null()?.map(|name| names.id(name)))
        };
        let (source, provider) = (name(7)?.unwrap_or_default(), name(8)?.unwrap_or_default());
        let (credential, model) = (name(9)?, name(10)?);
        let ttft_ms: Option<i64> = row.get(13)?;
        let kind = row.get_ref(14)?.as_str_or_null()?.and_then(TtftKind::parse);
        let tokens = if row.get::<_, bool>(15)? {
            let count = |index| row.get::<_, Option<i64>>(index).map(Option::unwrap_or_default);
            Some(Exact {
                input: count(16)? + count(17)? + count(18)?,
                cache_read: count(17)?,
                output: row.get(19)?,
            })
        } else {
            None
        };
        Ok(Self {
            order,
            complete: row.get(6)?,
            source,
            provider,
            credential,
            model,
            stream: row.get(11)?,
            start: row.get(12)?,
            latency,
            ttft: ttft_ms.zip(kind),
            tokens,
        })
    }
}

/// One request: its last send and the latency of all its sends.
struct Request {
    last: Send,
    latency: i64,
}

/// One timed request's figures.
struct Timed {
    server: i64,
    earlier: i64,
    first: i64,
    generation: i64,
    total: i64,
    tokens_per_second: Option<f64>,
    decode_speed: Option<f64>,
}

impl Timed {
    fn of(last: &Send, latency: i64, ttft: i64, earlier: i64) -> Self {
        let output = last.tokens.and_then(|tokens| tokens.output);
        let decoding = latency - ttft;
        let byte_or_token = matches!(last.ttft, Some((_, TtftKind::Byte | TtftKind::Token)));
        Self {
            server: (last.start - earlier).max(0),
            earlier,
            first: ttft,
            generation: decoding.max(0),
            total: last.start + latency,
            tokens_per_second: output
                .filter(|_| latency > 0)
                .map(|output| output as f64 * 1000.0 / latency as f64),
            decode_speed: output
                .filter(|_| last.stream && byte_or_token && decoding >= 250)
                .map(|output| output as f64 * 1000.0 / decoding as f64),
        }
    }
}

#[derive(Default)]
struct RowFigures {
    first: Vec<i64>,
    total: Vec<i64>,
    tokens_per_second: Vec<f64>,
    decode_speed: Vec<f64>,
}

#[derive(Default)]
struct PartFigures {
    requests: u64,
    server: f64,
    earlier: f64,
    first: f64,
    generation: f64,
}

/// Nearest rank: the `p`th percentile of ascending `sorted` is `sorted[ceil(p * n / 100) - 1]`.
fn percentile<T: Copy>(sorted: &[T], p: usize) -> Option<T> {
    let rank = (p * sorted.len()).div_ceil(100);
    sorted.get(rank.checked_sub(1)?).copied()
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    values.sort_unstable_by(f64::total_cmp);
    percentile(&values, 50)
}

fn read(
    path: &Path,
    range: Range,
    provider: Option<&str>,
    credential: Option<&str>,
    offset: i32,
    now: i64,
    lost: u64,
) -> anyhow::Result<Value> {
    let mut db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    db.busy_timeout(Duration::from_secs(2))?;
    let conn = db.transaction()?;
    let from = now - range.duration();
    // A range covering most of the table reads faster as a full scan than through
    // `sends_by_time`, which pays a row lookup per send.
    let scan = match range {
        Range::Day => "",
        Range::Week | Range::Month => " NOT INDEXED",
    };
    let mut names = Names::default();
    let mut requests: HashMap<(u128, i64), Request> = HashMap::new();
    let mut statement = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM sends{scan} WHERE at_ms >= ?1 AND at_ms <= ?2"
    ))?;
    let mut rows = statement.query(params![from - EARLIER_MS, now])?;
    while let Some(row) = rows.next()? {
        let key = request_key(row.get_ref(0)?.as_str()?, row.get(1)?);
        let order = (row.get(2)?, row.get(3)?, row.get(4)?);
        let latency: Option<i64> = row.get(5)?;
        match requests.entry(key) {
            Entry::Occupied(mut entry) => {
                let request = entry.get_mut();
                request.latency += latency.unwrap_or_default();
                if order > request.last.order {
                    request.last = Send::of(row, order, latency, &mut names)?;
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(Request {
                    last: Send::of(row, order, latency, &mut names)?,
                    latency: latency.unwrap_or_default(),
                });
            }
        }
    }
    drop(rows);
    drop(statement);
    // A filter naming nothing in range matches no request.
    let wanted = |filter: Option<&str>| filter.map(|name| names.ids.get(name).copied());
    let (provider_id, credential_id) = (wanted(provider), wanted(credential));
    let mut counted = 0_u64;
    let mut untimed = 0_u64;
    let mut row_figures: HashMap<(u32, u32, Option<u32>, TtftKind), RowFigures> = HashMap::new();
    let mut part_figures: HashMap<(u32, u32), PartFigures> = HashMap::new();
    for request in requests.into_values() {
        let last = request.last;
        let filtered = provider_id.is_some_and(|id| id != Some(last.provider))
            || credential_id.is_some_and(|id| id.is_none() || id != last.credential);
        if !last.complete || last.order.0 < from || filtered {
            continue;
        }
        counted += 1;
        let (Some(latency), Some((ttft, kind))) = (last.latency, last.ttft) else {
            untimed += 1;
            continue;
        };
        let timed = Timed::of(&last, latency, ttft, request.latency - latency);
        let figures = row_figures
            .entry((last.provider, last.source, last.model, kind))
            .or_default();
        figures.first.push(timed.first);
        figures.total.push(timed.total);
        figures.tokens_per_second.extend(timed.tokens_per_second);
        figures.decode_speed.extend(timed.decode_speed);
        let part = part_figures.entry((last.provider, last.source)).or_default();
        part.requests += 1;
        part.server += timed.server as f64;
        part.earlier += timed.earlier as f64;
        part.first += timed.first as f64;
        part.generation += timed.generation as f64;
    }
    let mut row_figures: Vec<_> = row_figures.into_iter().collect();
    row_figures.sort_unstable_by(|(a, _), (b, _)| {
        let key = |(provider, source, model, kind): &(u32, u32, Option<u32>, TtftKind)| {
            let model = model.map(|model| names.name(model));
            (names.name(*provider), names.name(*source), model, kind.as_str())
        };
        key(a).cmp(&key(b))
    });
    let out_rows: Vec<Value> = row_figures
        .into_iter()
        .map(|((provider, source, model, kind), mut figures)| {
            figures.first.sort_unstable();
            figures.total.sort_unstable();
            json!({
                "provider": names.name(provider), "source": names.name(source),
                "model": model.map(|model| names.name(model)), "ttftKind": kind.as_str(),
                "requests": figures.first.len(),
                "firstP50": percentile(&figures.first, 50), "firstP95": percentile(&figures.first, 95),
                "totalP50": percentile(&figures.total, 50), "totalP95": percentile(&figures.total, 95),
                "tokensPerSecond": median(figures.tokens_per_second),
                "decodeSpeed": median(figures.decode_speed),
            })
        })
        .collect();
    let mut part_figures: Vec<_> = part_figures.into_iter().collect();
    part_figures.sort_unstable_by_key(|((provider, source), _)| (names.name(*provider), names.name(*source)));
    let parts: Vec<Value> = part_figures
        .into_iter()
        .map(|((provider, source), part)| {
            let n = part.requests as f64;
            let means = [part.server / n, part.earlier / n, part.first / n, part.generation / n];
            json!({
                "provider": names.name(provider), "source": names.name(source), "requests": part.requests,
                "server": means[0], "earlier": means[1], "first": means[2], "generation": means[3],
                "total": means.iter().sum::<f64>(),
            })
        })
        .collect();
    let oldest: Option<i64> = conn.query_row("SELECT MIN(at_ms) FROM sends", [], |row| row.get(0))?;
    let lost = lost.max(meta(&conn, "lost_events")?.unwrap_or(0).max(0) as u64);
    let coverage_uncertain = meta(&conn, "uncertain_at")?.is_some_and(|at| at >= from && at <= now);
    Ok(json!({
        "range": range.name(), "from": from, "to": now, "offset": offset,
        "requests": counted, "untimed": untimed, "rows": out_rows, "parts": parts, "slow": [],
        "oldestSend": oldest, "lostEvents": lost, "coverageUncertain": coverage_uncertain,
    }))
}

#[cfg(test)]
#[path = "speed_tests.rs"]
mod tests;
