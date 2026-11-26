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
use std::num::NonZeroU32;
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
    latency_ms, outcome = 'complete', source, provider, credential, model, stream, start_ms,
    CASE WHEN source = 'proxy' AND at_ms < ?3 THEN NULL ELSE ttft_ms END, ttft_kind,
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

/// A string stored once per read in [`Names`]; non-zero so an absent one costs nothing.
type Id = NonZeroU32;

/// Strings stored once per read, named by their place from 1.
#[derive(Default)]
struct Names {
    ids: HashMap<Box<str>, Id>,
    names: Vec<Box<str>>,
}

impl Names {
    fn id(&mut self, name: &str) -> Id {
        if let Some(id) = self.ids.get(name) {
            return *id;
        }
        self.names.push(name.into());
        let id = u32::try_from(self.names.len())
            .ok()
            .and_then(Id::new)
            .unwrap_or(Id::MAX);
        self.ids.insert(name.into(), id);
        id
    }

    fn name(&self, id: Id) -> &str {
        self.text(id.get())
    }

    /// The name of an [`Id`] held as a plain number.
    fn text(&self, code: u32) -> &str {
        let index = code.checked_sub(1).map(|index| index as usize);
        index.and_then(|index| self.names.get(index)).map_or("", |name| name)
    }
}

/// A request's key: a 128-bit hash of its ID and turn. A million sends make about a
/// million entries, so the key is kept small rather than the ID stored; two requests share
/// a hash with odds near 2^-89.
fn request_key(request: &str, turn: i64) -> [u64; 2] {
    [0_u8, 1].map(|salt| {
        let mut hasher = DefaultHasher::new();
        (salt, request, turn).hash(&mut hasher);
        hasher.finish()
    })
}

/// Milliseconds held in 32 bits, saturating past 24 days, to keep [`Send`] small.
fn ms(value: i64) -> i32 {
    i32::try_from(value).unwrap_or(if value < 0 { i32::MIN } else { i32::MAX })
}

/// Token figures from a send with an exact breakdown.
#[derive(Clone, Copy)]
struct Exact {
    /// The [`SIZES`] index of its uncached input, cache read and cache write.
    size: u8,
    /// A cache read happened.
    cache: bool,
    output: Option<u32>,
}

/// What a request's last send says about it.
#[derive(Clone, Copy)]
struct Send {
    /// `at_ms`, then a fallback after every native send of its request, then `send`.
    order: (i64, bool, i32),
    complete: bool,
    source: Id,
    provider: Id,
    credential: Option<Id>,
    model: Option<Id>,
    stream: bool,
    start: i32,
    latency: Option<i32>,
    ttft: Option<(i32, TtftKind)>,
    tokens: Option<Exact>,
}

impl Send {
    fn of(row: &Row, order: (i64, bool, i32), latency: Option<i64>, names: &mut Names) -> rusqlite::Result<Self> {
        let source = names.id(row.get_ref(7)?.as_str()?);
        let provider = names.id(row.get_ref(8)?.as_str()?);
        let mut name = |index| -> rusqlite::Result<Option<Id>> {
            Ok(row.get_ref(index)?.as_str_or_null()?.map(|name| names.id(name)))
        };
        let (credential, model) = (name(9)?, name(10)?);
        let ttft_ms: Option<i64> = row.get(13)?;
        let kind = row.get_ref(14)?.as_str_or_null()?.and_then(TtftKind::parse);
        let tokens = if row.get::<_, bool>(15)? {
            let count = |index| row.get::<_, Option<i64>>(index).map(Option::unwrap_or_default);
            let cache_read = count(17)?;
            let output: Option<i64> = row.get(19)?;
            Some(Exact {
                size: size(count(16)? + cache_read + count(18)?),
                cache: cache_read > 0,
                output: output.map(|n| u32::try_from(n.max(0)).unwrap_or(u32::MAX)),
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
            start: ms(row.get(12)?),
            latency: latency.map(ms),
            ttft: ttft_ms.map(ms).zip(kind),
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
        let start = i64::from(last.start);
        let decoding = latency - ttft;
        let byte_or_token = matches!(last.ttft, Some((_, TtftKind::Byte | TtftKind::Token)));
        Self {
            server: (start - earlier).max(0),
            earlier,
            first: ttft,
            generation: decoding.max(0),
            total: start + latency,
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
    first: Vec<i32>,
    total: Vec<i32>,
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

/// A slice dimension judged against the rest of its provider, source and kind.
#[derive(Clone, Copy)]
enum Dimension {
    Model,
    Credential,
    Size,
    Cache,
    Stream,
    Hour,
}

impl Dimension {
    const ALL: [Self; 6] = [
        Self::Model,
        Self::Credential,
        Self::Size,
        Self::Cache,
        Self::Stream,
        Self::Hour,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Credential => "credential",
            Self::Size => "size",
            Self::Cache => "cache",
            Self::Stream => "stream",
            Self::Hour => "hour",
        }
    }

    /// `item`'s value in this dimension, as a code; none when it has no value here.
    fn code(self, item: &Item) -> Option<u32> {
        match self {
            Self::Model => item.model.map(Id::get),
            Self::Credential => item.credential.map(Id::get),
            Self::Size => item.size.map(u32::from),
            Self::Cache => item.cache.map(u32::from),
            Self::Stream => Some(u32::from(item.stream)),
            Self::Hour => Some(u32::from(item.hour)),
        }
    }

    fn value(self, code: u32, names: &Names) -> Value {
        match self {
            Self::Model | Self::Credential => names.text(code).into(),
            Self::Size => SIZES.get(code as usize).copied().unwrap_or_default().into(),
            Self::Cache => if code == 1 { "hit" } else { "miss" }.into(),
            Self::Stream => (code == 1).into(),
            Self::Hour => code.into(),
        }
    }
}

const SIZES: [&str; 4] = ["<10k", "10k-50k", "50k-200k", ">200k"];

/// The index in [`SIZES`] of a request with `input` tokens of input, cached or not.
fn size(input: i64) -> u8 {
    match input {
        ..10_000 => 0,
        10_000..50_000 => 1,
        50_000..200_000 => 2,
        _ => 3,
    }
}

/// The hour of the day, 0 to 23, at `at_ms` for a viewer `offset` minutes east of UTC.
fn hour(at_ms: i64, offset: i32) -> u8 {
    let minute = (at_ms.div_euclid(60_000) + i64::from(offset)).rem_euclid(1440);
    u8::try_from(minute / 60).unwrap_or_default()
}

/// A timed request's first response and its value in each dimension.
struct Item {
    first: i32,
    model: Option<Id>,
    credential: Option<Id>,
    size: Option<u8>,
    /// A cache read happened.
    cache: Option<bool>,
    stream: bool,
    hour: u8,
}

impl Item {
    fn of(last: &Send, first: i32, offset: i32) -> Self {
        Self {
            first,
            model: last.model,
            credential: last.credential,
            size: last.tokens.map(|tokens| tokens.size),
            cache: last.tokens.map(|tokens| tokens.cache),
            stream: last.stream,
            hour: hour(last.order.0, offset),
        }
    }
}

/// A value judged within one provider, source and kind needs this many requests, and so
/// do the others it is judged against.
const MIN_REQUESTS: usize = 30;
/// A value is slow when its p95 first response is at least this many times the others'.
const SLOW_RATIO: f64 = 1.5;
const MAX_SLOW: usize = 20;

struct Slow {
    dimension: Dimension,
    code: u32,
    group: (Id, Id, TtftKind),
    requests: usize,
    p95: i32,
    baseline: i32,
}

impl Slow {
    fn ratio(&self) -> f64 {
        self.p95 as f64 / self.baseline as f64
    }
}

/// The slow values of one provider, source and kind's requests.
fn slow_in(group: (Id, Id, TtftKind), mut items: Vec<Item>, out: &mut Vec<Slow>) {
    items.sort_unstable_by_key(|item| item.first);
    let n = items.len();
    for dimension in Dimension::ALL {
        // Each value's first responses, ascending, since `items` is.
        let mut values: HashMap<u32, Vec<i32>> = HashMap::new();
        for item in &items {
            if let Some(code) = dimension.code(item) {
                values.entry(code).or_default().push(item.first);
            }
        }
        for (code, firsts) in values {
            let others = n - firsts.len();
            if firsts.len() < MIN_REQUESTS || others < MIN_REQUESTS {
                continue;
            }
            let Some(p95) = percentile(&firsts, 95) else {
                continue;
            };
            let Some(baseline) = others_p95(&items, others, |item| dimension.code(item) == Some(code)) else {
                continue;
            };
            if p95 > baseline && p95 as f64 >= SLOW_RATIO * baseline as f64 {
                out.push(Slow {
                    dimension,
                    code,
                    group,
                    requests: firsts.len(),
                    p95,
                    baseline,
                });
            }
        }
    }
}

/// The nearest-rank p95 first response of the `others` items of ascending `items` that are
/// not members. That is their (m - ceil(0.95 m) + 1)th largest, so this walks down from the
/// top past the members rather than sorting the others again.
fn others_p95(items: &[Item], others: usize, member: impl Fn(&Item) -> bool) -> Option<i32> {
    let from_top = (others + 1).checked_sub((95 * others).div_ceil(100))?;
    items
        .iter()
        .rev()
        .filter(|item| !member(item))
        .nth(from_top.checked_sub(1)?)
        .map(|item| item.first)
}

/// Nearest rank: the `p`th percentile of ascending `sorted` is `sorted[ceil(p * n / 100) - 1]`.
pub(crate) fn percentile<T: Copy>(sorted: &[T], p: usize) -> Option<T> {
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
    // Sized up front from the first sends: a map that grows leaves its outgrown tables to
    // the allocator, which can keep them resident.
    let first_sends: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM sends{scan} WHERE at_ms >= ?1 AND at_ms <= ?2 AND send = 0"),
        params![from - EARLIER_MS, now],
        |row| row.get(0),
    )?;
    let mut requests: HashMap<[u64; 2], Request> =
        HashMap::with_capacity(usize::try_from(first_sends).unwrap_or_default());
    let mut statement = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM sends{scan} WHERE at_ms >= ?1 AND at_ms <= ?2"
    ))?;
    // Proxy first responses before this were timed from another start; see `Activity::start`.
    let ttft_from_attempt = meta(&conn, "ttft_from_attempt")?.unwrap_or_default();
    let mut rows = statement.query(params![from - EARLIER_MS, now, ttft_from_attempt])?;
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
    let mut row_figures: HashMap<(Id, Id, Option<Id>, TtftKind), RowFigures> = HashMap::new();
    let mut part_figures: HashMap<(Id, Id), PartFigures> = HashMap::new();
    let mut groups: HashMap<(Id, Id, TtftKind), Vec<Item>> = HashMap::new();
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
        let latency = i64::from(latency);
        let timed = Timed::of(&last, latency, i64::from(ttft), request.latency - latency);
        let figures = row_figures
            .entry((last.provider, last.source, last.model, kind))
            .or_default();
        figures.first.push(ms(timed.first));
        figures.total.push(ms(timed.total));
        figures.tokens_per_second.extend(timed.tokens_per_second);
        figures.decode_speed.extend(timed.decode_speed);
        let part = part_figures.entry((last.provider, last.source)).or_default();
        part.requests += 1;
        part.server += timed.server as f64;
        part.earlier += timed.earlier as f64;
        part.first += timed.first as f64;
        part.generation += timed.generation as f64;
        groups
            .entry((last.provider, last.source, kind))
            .or_default()
            .push(Item::of(&last, ms(timed.first), offset));
    }
    let mut slow = Vec::new();
    for (group, items) in groups {
        slow_in(group, items, &mut slow);
    }
    let order = |entry: &Slow| {
        let (provider, source, kind) = entry.group;
        let value = entry.dimension.value(entry.code, &names).to_string();
        (
            names.name(provider),
            names.name(source),
            kind.as_str(),
            entry.dimension.name(),
            value,
        )
    };
    slow.sort_by(|a, b| b.ratio().total_cmp(&a.ratio()).then_with(|| order(a).cmp(&order(b))));
    let slow: Vec<Value> = slow
        .iter()
        .take(MAX_SLOW)
        .map(|entry| {
            let (provider, source, kind) = entry.group;
            json!({
                "slice": entry.dimension.name(), "value": entry.dimension.value(entry.code, &names),
                "provider": names.name(provider), "source": names.name(source), "ttftKind": kind.as_str(),
                "requests": entry.requests, "p95": entry.p95, "baseline": entry.baseline,
            })
        })
        .collect();
    let mut row_figures: Vec<_> = row_figures.into_iter().collect();
    row_figures.sort_unstable_by(|(a, _), (b, _)| {
        let key = |(provider, source, model, kind): &(Id, Id, Option<Id>, TtftKind)| {
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
        "requests": counted, "untimed": untimed, "rows": out_rows, "parts": parts, "slow": slow,
        "oldestSend": oldest, "lostEvents": lost, "coverageUncertain": coverage_uncertain,
    }))
}

#[cfg(test)]
#[path = "speed_tests.rs"]
mod tests;
