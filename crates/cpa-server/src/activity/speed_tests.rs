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
