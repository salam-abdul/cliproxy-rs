use super::*;
use crate::activity::sends::{SendRow, Tokens, TtftKind};
use crate::activity::{Activity, DAY_MS, Range, epoch_ms};
use crate::request_logging::history::row::{Outcome, Source};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

struct Store {
    dir: PathBuf,
    path: PathBuf,
    activity: Activity,
    now: i64,
}

impl Store {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("activity-speed-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("activity.sqlite");
        let activity = Activity::start(path.clone(), Arc::new(AtomicU64::new(0))).unwrap();
        Self {
            dir,
            path,
            activity,
            now: epoch_ms(),
        }
    }

    fn add(&self, rows: impl IntoIterator<Item = SendRow>) {
        for row in rows {
            self.activity.record_send(row);
        }
    }

    fn speed(&self, provider: Option<&str>, credential: Option<&str>) -> Value {
        self.activity.flush();
        read(&self.path, Range::Day, provider, credential, 0, self.now, 0).unwrap()
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A complete proxy Codex send a minute before `now`: started at the request's arrival,
/// 1,000 ms long, first byte at 200 ms, no tokens.
fn send(now: i64, request_id: &str, send: i32) -> SendRow {
    SendRow {
        request_id: request_id.into(),
        turn: 0,
        send,
        parent_request: None,
        at_ms: now - 60_000,
        source: Source::Proxy,
        provider: "codex".into(),
        credential: Some("codex-a.json".into()),
        model: Some("model-test".into()),
        outcome: Outcome::Complete,
        status: None,
        stream: true,
        start_ms: 0,
        latency_ms: Some(1000),
        ttft: Some((200, TtftKind::Byte)),
        tokens: None,
    }
}

fn exact(output: i64) -> Option<Tokens> {
    Some(Tokens {
        uncached_input: 10,
        cache_read: 0,
        cache_write: 0,
        output,
        reasoning: 0,
        exact: true,
    })
}

fn rows(speed: &Value) -> &Vec<Value> {
    speed["rows"].as_array().unwrap()
}

fn row<'a>(speed: &'a Value, model: &str) -> &'a Value {
    rows(speed)
        .iter()
        .find(|r| r["model"] == model)
        .unwrap_or_else(|| panic!("no {model} row in {speed}"))
}

fn part<'a>(speed: &'a Value, provider: &str, source: &str) -> &'a Value {
    speed["parts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider"] == provider && p["source"] == source)
        .unwrap_or_else(|| panic!("no {provider} {source} part in {speed}"))
}

#[test]
fn the_four_parts_add_up_to_the_total() {
    let store = Store::new();
    let now = store.now;
    let r1 = SendRow {
        start_ms: 5,
        ..send(now, "r1", 0)
    };
    let r2_failed = SendRow {
        outcome: Outcome::Failed,
        status: Some(500),
        start_ms: 10,
        latency_ms: Some(300),
        ttft: None,
        ..send(now, "r2", 0)
    };
    let r2_last = SendRow {
        at_ms: now - 59_000,
        start_ms: 320,
        latency_ms: Some(500),
        ttft: Some((100, TtftKind::Byte)),
        ..send(now, "r2", 1)
    };
    let r3 = SendRow {
        outcome: Outcome::Failed,
        status: Some(500),
        ..send(now, "r3", 0)
    };
    let r4 = SendRow {
        ttft: None,
        ..send(now, "r4", 0)
    };
    store.add([r1, r2_failed, r2_last, r3, r4]);
    let speed = store.speed(None, None);
    assert_eq!(speed["requests"], 3, "{speed}");
    assert_eq!(speed["untimed"], 1, "{speed}");
    assert_eq!(
        *part(&speed, "codex", "proxy"),
        json!({"provider": "codex", "source": "proxy", "requests": 2, "server": 12.5, "earlier": 150.0,
            "first": 150.0, "generation": 600.0, "total": 912.5})
    );
    assert_eq!(speed["parts"].as_array().unwrap().len(), 1);
}

#[test]
fn a_fallback_counts_under_the_native_request() {
    let store = Store::new();
    let now = store.now;
    let native = SendRow {
        source: Source::Native,
        provider: "claude".into(),
        credential: Some("claude-a.json".into()),
        outcome: Outcome::Failed,
        status: Some(429),
        latency_ms: Some(400),
        ttft: None,
        ..send(now, "n1", 0)
    };
    // The same millisecond and send number as the native send it follows.
    let fallback = SendRow {
        parent_request: Some("n1".into()),
        start_ms: 450,
        latency_ms: Some(600),
        ttft: Some((50, TtftKind::Byte)),
        ..send(now, "f1", 0)
    };
    store.add([native, fallback]);
    let speed = store.speed(None, None);
    assert_eq!(speed["requests"], 1, "{speed}");
    assert_eq!(speed["untimed"], 0, "{speed}");
    assert_eq!(
        speed["parts"],
        json!([{"provider": "codex", "source": "proxy", "requests": 1, "server": 50.0, "earlier": 400.0,
            "first": 50.0, "generation": 550.0, "total": 1050.0}])
    );
}

#[test]
fn percentiles_are_nearest_rank() {
    for (values, p50, p95) in [
        ((10..=30).step_by(10).collect::<Vec<i64>>(), 20, 30),
        ((1..=100).collect(), 50, 95),
    ] {
        let store = Store::new();
        let now = store.now;
        store.add(values.iter().map(|ttft| SendRow {
            ttft: Some((*ttft, TtftKind::Byte)),
            latency_ms: Some(1000 + ttft),
            ..send(now, &format!("r{ttft}"), 0)
        }));
        let speed = store.speed(None, None);
        let row = row(&speed, "model-test");
        assert_eq!(row["requests"], values.len(), "{speed}");
        assert_eq!(
            (&row["firstP50"], &row["firstP95"]),
            (&json!(p50), &json!(p95)),
            "{speed}"
        );
        assert_eq!(
            (&row["totalP50"], &row["totalP95"]),
            (&json!(1000 + p50), &json!(1000 + p95)),
            "{speed}"
        );
    }
}

#[test]
fn kinds_and_sources_stay_in_separate_rows() {
    let store = Store::new();
    let now = store.now;
    let claude = |id: &str, source| SendRow {
        source,
        provider: "claude".into(),
        credential: Some("claude-a.json".into()),
        model: Some("claude-test".into()),
        ..send(now, id, 0)
    };
    store.add([
        SendRow {
            ttft: Some((100, TtftKind::Event)),
            ..send(now, "event", 0)
        },
        SendRow {
            ttft: Some((300, TtftKind::Byte)),
            ..send(now, "byte", 0)
        },
        claude("native", Source::Native),
        claude("proxy", Source::Proxy),
    ]);
    let speed = store.speed(None, None);
    let keys: Vec<(Value, Value, Value, Value, Value)> = rows(&speed)
        .iter()
        .map(|r| {
            (
                r["provider"].clone(),
                r["source"].clone(),
                r["model"].clone(),
                r["ttftKind"].clone(),
                r["firstP50"].clone(),
            )
        })
        .collect();
    assert_eq!(
        keys,
        [
            (
                "claude".into(),
                "native".into(),
                "claude-test".into(),
                "byte".into(),
                200.into()
            ),
            (
                "claude".into(),
                "proxy".into(),
                "claude-test".into(),
                "byte".into(),
                200.into()
            ),
            (
                "codex".into(),
                "proxy".into(),
                "model-test".into(),
                "byte".into(),
                300.into()
            ),
            (
                "codex".into(),
                "proxy".into(),
                "model-test".into(),
                "event".into(),
                100.into()
            ),
        ]
    );
    part(&speed, "claude", "native");
    part(&speed, "claude", "proxy");
    assert_eq!(speed["parts"].as_array().unwrap().len(), 3, "{speed}");
}

#[test]
fn token_speeds_follow_their_rules() {
    let store = Store::new();
    let now = store.now;
    let timed = |model: &str, ttft: i64| SendRow {
        model: Some(model.into()),
        latency_ms: Some(2000),
        ttft: Some((ttft, TtftKind::Byte)),
        tokens: exact(1000),
        ..send(now, model, 0)
    };
    store.add([
        timed("decoded", 1000),
        timed("short", 1800),
        SendRow {
            tokens: Some(Tokens {
                exact: false,
                ..exact(1000).unwrap()
            }),
            ..timed("inexact", 1000)
        },
        SendRow {
            stream: false,
            ..timed("buffered", 1000)
        },
    ]);
    let speed = store.speed(None, None);
    let speeds = |model| {
        let row = row(&speed, model);
        (row["tokensPerSecond"].clone(), row["decodeSpeed"].clone())
    };
    assert_eq!(speeds("decoded"), (json!(500.0), json!(1000.0)));
    assert_eq!(speeds("short"), (json!(500.0), Value::Null));
    assert_eq!(speeds("inexact"), (Value::Null, Value::Null));
    assert_eq!(speeds("buffered"), (json!(500.0), Value::Null));
}

#[test]
fn filters_and_range_apply_to_the_last_send() {
    let store = Store::new();
    let now = store.now;
    let from = now - DAY_MS;
    let claude = |row: SendRow| SendRow {
        provider: "claude".into(),
        credential: Some("claude-a.json".into()),
        ..row
    };
    store.add([
        // Its only send began before the range.
        SendRow {
            at_ms: from - 1000,
            ..send(now, "before", 0)
        },
        // Its earlier send, on another provider, began 30 minutes before the range.
        claude(SendRow {
            at_ms: from - 1_800_000,
            outcome: Outcome::Failed,
            status: Some(500),
            latency_ms: Some(700),
            ttft: None,
            ..send(now, "straddles", 0)
        }),
        SendRow {
            at_ms: from + 1000,
            start_ms: 800,
            ttft: Some((100, TtftKind::Byte)),
            ..send(now, "straddles", 1)
        },
        claude(send(now, "claude", 0)),
    ]);
    let all = store.speed(None, None);
    assert_eq!(all["requests"], 2, "{all}");
    let codex = store.speed(Some("codex"), None);
    assert_eq!(codex["requests"], 1, "{codex}");
    assert_eq!(
        *part(&codex, "codex", "proxy"),
        json!({"provider": "codex", "source": "proxy", "requests": 1, "server": 100.0, "earlier": 700.0,
            "first": 100.0, "generation": 900.0, "total": 1800.0})
    );
    let claude = store.speed(None, Some("claude-a.json"));
    assert_eq!(claude["requests"], 1, "{claude}");
    assert_eq!(rows(&claude).len(), 1);
    assert_eq!(rows(&claude)[0]["provider"], "claude");
    assert_eq!(store.speed(Some("codex"), Some("claude-a.json"))["requests"], 0);
}

#[test]
fn an_empty_store_reads_empty() {
    let store = Store::new();
    let speed = store.speed(None, None);
    let mut keys: Vec<&str> = speed.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "coverageUncertain",
            "from",
            "lostEvents",
            "offset",
            "oldestSend",
            "parts",
            "range",
            "requests",
            "rows",
            "slow",
            "to",
            "untimed"
        ]
    );
    assert_eq!(
        (&speed["requests"], &speed["untimed"], &speed["range"], &speed["offset"]),
        (&json!(0), &json!(0), &json!("24h"), &json!(0))
    );
    assert_eq!(
        (&speed["rows"], &speed["parts"], &speed["slow"]),
        (&json!([]), &json!([]), &json!([]))
    );
    assert_eq!(speed["oldestSend"], Value::Null);
}

/// Writes `rows` in one transaction on a connection of its own, past the writer's queue.
fn write_all(store: &Store, rows: impl IntoIterator<Item = SendRow>) {
    let mut conn = rusqlite::Connection::open(&store.path).unwrap();
    conn.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    let transaction = conn.transaction().unwrap();
    for row in rows {
        crate::activity::sends::write_send(&transaction, &row).unwrap();
    }
    transaction.commit().unwrap();
}

/// `count` requests on `model` with first response `ttft`, named `{model}-{index}`.
fn requests(now: i64, model: &str, count: usize, ttft: i64) -> impl Iterator<Item = SendRow> + use<> {
    let model = model.to_owned();
    (0..count).map(move |index| SendRow {
        model: Some(model.clone()),
        ttft: Some((ttft, TtftKind::Byte)),
        ..send(now, &format!("{model}-{index}"), 0)
    })
}

#[test]
fn a_slow_model_is_reported_against_the_rest() {
    let store = Store::new();
    let now = store.now;
    store.add(requests(now, "a", 60, 100).chain(requests(now, "b", 30, 400)));
    let speed = store.speed(None, None);
    assert_eq!(
        speed["slow"],
        json!([{"slice": "model", "value": "b", "provider": "codex", "source": "proxy", "ttftKind": "byte",
            "requests": 30, "p95": 400, "baseline": 100}])
    );
}

#[test]
fn a_slice_without_30_others_is_not_judged() {
    let store = Store::new();
    let now = store.now;
    store.add(requests(now, "a", 29, 100).chain(requests(now, "b", 30, 400)));
    let speed = store.speed(None, None);
    assert_eq!(speed["requests"], 59);
    assert_eq!(speed["slow"], json!([]));
}

#[test]
fn slices_bucket_size_cache_and_hour() {
    let sizes: Vec<&str> = [9_999, 10_000, 49_999, 50_000, 199_999, 200_000]
        .map(|input| SIZES[usize::from(size(input))])
        .to_vec();
    assert_eq!(sizes, ["<10k", "10k-50k", "10k-50k", "50k-200k", "50k-200k", ">200k"]);
    let names = Names::default();
    assert_eq!(Dimension::Cache.value(0, &names), "miss");
    assert_eq!(Dimension::Cache.value(1, &names), "hit");
    let half_past_eleven = 1_800_000_000_000 / DAY_MS * DAY_MS + 23 * 3_600_000 + 30 * 60_000;
    assert_eq!(hour(half_past_eleven, 0), 23);
    assert_eq!(hour(half_past_eleven, 60), 0);
    assert_eq!(hour(half_past_eleven, -90), 22);

    // Exact counts: 30 slow requests over 200k tokens with no cache read, 60 fast small
    // ones that read the cache. 30 fast requests as large but inexact have no size or
    // cache value and only count among the others.
    let store = Store::new();
    let now = store.now;
    let tokens = |input, cache_read, exact| {
        Some(Tokens {
            uncached_input: input,
            cache_read,
            cache_write: 0,
            output: 10,
            reasoning: 0,
            exact,
        })
    };
    let with = |rows: Vec<SendRow>, t: Option<Tokens>| rows.into_iter().map(move |row| SendRow { tokens: t, ..row });
    store.add(
        with(requests(now, "large", 30, 400).collect(), tokens(250_000, 0, true))
            .chain(with(
                requests(now, "small", 60, 100).collect(),
                tokens(1_000, 500, true),
            ))
            .chain(with(
                requests(now, "inexact", 30, 100).collect(),
                tokens(250_000, 0, false),
            )),
    );
    let speed = store.speed(None, None);
    let slow = speed["slow"].as_array().unwrap();
    let entry = |slice: &str| slow.iter().find(|s| s["slice"] == slice).cloned();
    assert_eq!(
        entry("size"),
        Some(
            json!({"slice": "size", "value": ">200k", "provider": "codex", "source": "proxy",
            "ttftKind": "byte", "requests": 30, "p95": 400, "baseline": 100})
        ),
        "{speed}"
    );
    assert_eq!(
        entry("cache").map(|e| e["value"].clone()),
        Some(json!("miss")),
        "{speed}"
    );
    assert_eq!(
        entry("cache").map(|e| e["requests"].clone()),
        Some(json!(30)),
        "{speed}"
    );
}

#[test]
fn at_most_20_slices_sorted_by_ratio() {
    let store = Store::new();
    let now = store.now;
    let rows = (0..22_i64).flat_map(|i| {
        let provider = format!("provider-{i:02}");
        requests(now, &format!("fast-{i}"), 30, 100)
            .chain(requests(now, &format!("slow-{i}"), 30, 200 + 10 * i))
            .map(move |row| SendRow {
                provider: provider.clone(),
                ..row
            })
    });
    write_all(&store, rows);
    let speed = store.speed(None, None);
    let slow = speed["slow"].as_array().unwrap();
    assert_eq!(slow.len(), 20, "{speed}");
    let ratios: Vec<f64> = slow
        .iter()
        .map(|s| s["p95"].as_f64().unwrap() / s["baseline"].as_f64().unwrap())
        .collect();
    assert!(ratios.windows(2).all(|w| w[0] >= w[1]), "{ratios:?}");
    assert_eq!(slow[0]["provider"], "provider-21");
    assert_eq!(slow[19]["provider"], "provider-02");
    assert!(slow.iter().all(|s| s["slice"] == "model"), "{speed}");
}

/// A send without a provider cannot be filed under one: the read fails rather than
/// lending it another row's name.
#[test]
fn a_send_without_a_provider_fails_the_read() {
    let dir = std::env::temp_dir().join(format!("activity-speed-null-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("activity.sqlite");
    let conn = rusqlite::Connection::open(&path).unwrap();
    // The store's columns without its NOT NULL constraints.
    conn.execute_batch(
        "CREATE TABLE history_meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
        CREATE TABLE sends (
            request_id TEXT, turn INTEGER, send INTEGER, parent_request TEXT, at_ms INTEGER, source TEXT,
            provider TEXT, credential TEXT, model TEXT, outcome TEXT, status INTEGER, stream INTEGER,
            start_ms INTEGER, latency_ms INTEGER, ttft_ms INTEGER, ttft_kind TEXT,
            uncached_input_tokens INTEGER, cache_read_tokens INTEGER, cache_write_tokens INTEGER,
            output_tokens INTEGER, reasoning_tokens INTEGER, tokens_exact INTEGER
        );",
    )
    .unwrap();
    let now = epoch_ms();
    conn.execute(
        "INSERT INTO sends (request_id, turn, send, at_ms, source, provider, outcome, stream, start_ms,
            latency_ms, ttft_ms, ttft_kind) VALUES ('r1', 0, 0, ?1, 'proxy', NULL, 'complete', 1, 0, 100, 10, 'byte')",
        [now - 60_000],
    )
    .unwrap();
    drop(conn);
    let read = read(&path, Range::Day, None, None, 0, now, 0);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(read.is_err(), "{read:?}");
}

/// The walk for the others' p95 agrees with sorting the others and taking the nearest
/// rank, over distinct values, ties at the rank, members among and above the others, and
/// others counts on both sides of each rank step.
#[test]
fn the_others_p95_walk_matches_a_full_sort() {
    let item = |first: i32, member: bool| Item {
        first,
        model: if member { Id::new(1) } else { Id::new(2) },
        credential: None,
        size: None,
        cache: None,
        stream: true,
        hour: 0,
    };
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut cases = 0;
    for others in [30, 31, 39, 40, 41, 100] {
        for members in [1, 30, 45] {
            // Distinct values, few values (ties at every rank), and members all above.
            for layout in 0..3 {
                for _ in 0..4 {
                    let marks = (0..others).map(|_| false).chain((0..members).map(|_| true));
                    let mut items: Vec<Item> = marks
                        .map(|member| {
                            let r = next();
                            let first = match layout {
                                0 => (r % 100_000) as i32,
                                1 => (r % 4) as i32 * 10,
                                _ => (r % 1000) as i32 + if member { 10_000 } else { 0 },
                            };
                            item(first, member)
                        })
                        .collect();
                    items.sort_by_key(|item| item.first);
                    let is_member = |item: &Item| item.model == Id::new(1);
                    let rest: Vec<i32> = items.iter().filter(|i| !is_member(i)).map(|i| i.first).collect();
                    assert_eq!(rest.len(), others);
                    assert_eq!(
                        others_p95(&items, others, is_member),
                        percentile(&rest, 95),
                        "others {others}, members {members}, layout {layout}"
                    );
                    cases += 1;
                }
            }
        }
    }
    assert_eq!(cases, 216);
}

/// Proxy first responses written before the store counted them from the attempt's start
/// were timed from the round trip's start, so they are left out; native ones were always
/// timed from the send's start.
#[test]
fn proxy_first_responses_from_before_the_origin_change_are_untimed() {
    let store = Store::new();
    let now = store.now;
    let changed = now - 30_000;
    let conn = rusqlite::Connection::open(&store.path).unwrap();
    crate::activity::set_meta(&conn, "ttft_from_attempt", changed).unwrap();
    drop(conn);
    store.add([
        send(now, "proxy-before", 0),
        SendRow {
            source: Source::Native,
            ..send(now, "native-before", 0)
        },
        SendRow {
            at_ms: changed,
            ..send(now, "proxy-after", 0)
        },
    ]);
    let speed = store.speed(None, None);
    assert_eq!(
        (&speed["requests"], &speed["untimed"]),
        (&json!(3), &json!(1)),
        "{speed}"
    );
    let counts: Vec<(Value, Value)> = rows(&speed)
        .iter()
        .map(|r| (r["source"].clone(), r["requests"].clone()))
        .collect();
    assert_eq!(counts, [("native".into(), 1.into()), ("proxy".into(), 1.into())]);
}

/// A store records when it began counting proxy first responses from the attempt's start
/// once, when that key is missing: 0 with no sends yet, else the time it opened. A later
/// start keeps it.
#[test]
fn reopening_a_store_keeps_its_origin_change_time() {
    let store = Store::new();
    store.activity.finish();
    let first = {
        let conn = rusqlite::Connection::open(&store.path).unwrap();
        crate::activity::meta(&conn, "ttft_from_attempt").unwrap()
    };
    assert!(first.is_some_and(|at| at <= epoch_ms()), "{first:?}");
    std::thread::sleep(std::time::Duration::from_millis(5));
    let again = Activity::start(store.path.clone(), Arc::new(AtomicU64::new(0))).unwrap();
    again.finish();
    let conn = rusqlite::Connection::open(&store.path).unwrap();
    assert_eq!(crate::activity::meta(&conn, "ttft_from_attempt").unwrap(), first);
    // A store written before the key existed, with sends, gets the time it was opened.
    conn.execute("DELETE FROM history_meta WHERE key = 'ttft_from_attempt'", [])
        .unwrap();
    crate::activity::sends::write_send(&conn, &send(store.now, "old", 0)).unwrap();
    drop(conn);
    let opened = epoch_ms();
    let upgraded = Activity::start(store.path.clone(), Arc::new(AtomicU64::new(0))).unwrap();
    upgraded.finish();
    let conn = rusqlite::Connection::open(&store.path).unwrap();
    let at = crate::activity::meta(&conn, "ttft_from_attempt").unwrap();
    assert!(at.is_some_and(|at| at >= opened && at <= epoch_ms()), "{at:?}");
}
