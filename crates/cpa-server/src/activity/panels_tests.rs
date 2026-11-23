use super::*;
use crate::activity::sends::{SendRow, Tokens};
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
}

impl Store {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("activity-panels-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("activity.sqlite");
        let activity = Activity::start(path.clone(), Arc::new(AtomicU64::new(0))).unwrap();
        Self { dir, path, activity }
    }

    fn panels(&self, provider: Option<&str>, credential: Option<&str>) -> Value {
        self.activity.flush();
        read(&self.path, Range::Day, provider, credential, epoch_ms(), 0).unwrap()
    }

    fn over(&self, range: Range, now: i64) -> Value {
        self.activity.flush();
        read(&self.path, range, None, None, now, 0).unwrap()
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn send(request_id: &str, send: i32, provider: &str, outcome: Outcome, status: Option<i64>) -> SendRow {
    SendRow {
        request_id: request_id.into(),
        turn: 0,
        send,
        parent_request: None,
        at_ms: epoch_ms() - 60_000 + i64::from(send),
        source: Source::Proxy,
        provider: provider.into(),
        credential: Some(format!("{provider}-a.json")),
        model: Some("model-test".into()),
        outcome,
        status,
        stream: true,
        start_ms: 0,
        latency_ms: Some(10),
        ttft: None,
        tokens: None,
    }
}

fn provider<'a>(panels: &'a Value, name: &str) -> &'a Value {
    panels["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider"] == name)
        .unwrap_or_else(|| panic!("no {name} row in {panels}"))
}

#[test]
fn a_provider_row_counts_requests_sends_and_each_kind_of_failure_once() {
    let store = Store::new();
    for row in [
        send("r1", 0, "codex", Outcome::Failed, Some(500)),
        send("r1", 1, "codex", Outcome::Complete, None),
        send("r2", 0, "codex", Outcome::Failed, Some(429)),
        send("r3", 0, "codex", Outcome::Cancelled, None),
    ] {
        store.activity.record_send(row);
    }
    let panels = store.panels(None, None);
    let codex = provider(&panels, "codex");
    assert_eq!(
        [
            &codex["requests"],
            &codex["sends"],
            &codex["failed"],
            &codex["rateLimited"],
            &codex["failedOver"]
        ],
        [3, 4, 1, 1, 1],
        "{panels}"
    );
    assert_eq!([&codex["cancelled"], &codex["exactSends"]], [1, 0], "{panels}");
}

#[test]
fn buckets_count_rate_limited_and_cancelled_sends() {
    let store = Store::new();
    for row in [
        send("r1", 0, "codex", Outcome::Failed, Some(500)),
        send("r1", 1, "codex", Outcome::Complete, None),
        send("r2", 0, "codex", Outcome::Failed, Some(429)),
        send("r3", 0, "codex", Outcome::Cancelled, None),
    ] {
        store.activity.record_send(row);
    }
    let panels = store.panels(None, None);
    let buckets = panels["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 1, "{panels}");
    let bucket = &buckets[0];
    assert_eq!(
        [
            &bucket["sends"],
            &bucket["failed"],
            &bucket["rateLimited"],
            &bucket["cancelled"]
        ],
        [4, 1, 1, 1],
        "{panels}"
    );
}

#[test]
fn a_fallback_send_counts_under_the_request_it_serves() {
    let store = Store::new();
    let mut native = send("n1", 0, "claude", Outcome::Failover, Some(429));
    native.source = Source::Native;
    let mut fallback = send("p1", 0, "gemini", Outcome::Complete, None);
    fallback.parent_request = Some("n1".into());
    fallback.at_ms = native.at_ms + 5;
    store.activity.record_send(native);
    store.activity.record_send(fallback);
    let panels = store.panels(None, None);
    assert_eq!(provider(&panels, "claude")["requests"], 1, "{panels}");
    assert_eq!(provider(&panels, "gemini")["requests"], 1, "{panels}");
    assert_eq!(provider(&panels, "claude")["failedOver"], 1, "{panels}");
    assert_eq!(provider(&panels, "gemini")["failedOver"], 0, "{panels}");
}

#[test]
fn inexact_tokens_stay_out_of_the_sums() {
    let store = Store::new();
    let tokens = |output, exact| Tokens {
        uncached_input: 0,
        cache_read: 0,
        cache_write: 0,
        output,
        reasoning: 0,
        exact,
    };
    let mut exact = send("r1", 0, "codex", Outcome::Complete, None);
    exact.tokens = Some(tokens(20, true));
    let mut inexact = send("r2", 0, "codex", Outcome::Complete, None);
    inexact.tokens = Some(tokens(99, false));
    store.activity.record_send(exact);
    store.activity.record_send(inexact);
    let panels = store.panels(None, None);
    assert_eq!(provider(&panels, "codex")["outputTokens"], 20, "{panels}");
    assert_eq!(panels["credentials"][0]["outputTokens"], 20, "{panels}");
}

#[test]
fn the_provider_and_credential_filters_narrow_every_list() {
    let store = Store::new();
    let mut b = send("r2", 0, "codex", Outcome::Complete, None);
    b.credential = Some("b.json".into());
    let mut a = send("r1", 0, "codex", Outcome::Complete, None);
    a.credential = Some("a.json".into());
    let mut c = send("r3", 0, "claude", Outcome::Complete, None);
    c.credential = Some("c.json".into());
    for row in [a, b, c] {
        store.activity.record_send(row);
    }
    let reading = |long| crate::scheduler::Reading {
        long,
        used_percent: 10.0,
        reset: None,
    };
    let at = epoch_ms() - 60_000;
    store.activity.record_readings("codex", "a.json", at, &[reading(true)]);
    store.activity.record_readings("codex", "b.json", at, &[reading(true)]);
    store.activity.record_readings("claude", "c.json", at, &[reading(true)]);
    let counts = |panels: &Value| {
        ["providers", "credentials", "buckets", "cycles"].map(|list| panels[list].as_array().unwrap().len())
    };
    assert_eq!(counts(&store.panels(None, None)), [2, 3, 2, 3]);
    assert_eq!(counts(&store.panels(Some("codex"), None)), [1, 2, 1, 2]);
    let narrowed = store.panels(None, Some("a.json"));
    assert_eq!(counts(&narrowed), [1, 1, 1, 1]);
    assert_eq!(narrowed["cycles"][0]["credential"], "a.json");
}

#[test]
fn a_range_excludes_older_sends_and_reports_the_oldest_one() {
    let store = Store::new();
    let mut old = send("old", 0, "codex", Outcome::Complete, None);
    old.at_ms = epoch_ms() - 2 * DAY_MS;
    let oldest = old.at_ms;
    store.activity.record_send(old);
    store
        .activity
        .record_send(send("new", 0, "codex", Outcome::Complete, None));
    let panels = store.panels(None, None);
    assert_eq!(provider(&panels, "codex")["sends"], 1, "{panels}");
    assert_eq!(panels["oldestSend"], oldest);
}

fn weekly(used_percent: f64, reset_ms: i64) -> crate::scheduler::Reading {
    crate::scheduler::Reading {
        long: true,
        used_percent,
        reset: Some(std::time::UNIX_EPOCH + std::time::Duration::from_millis(reset_ms as u64)),
    }
}

/// Two weekly cycles on `codex-a.json`, the first ended by a reset three days before `now`,
/// with exact sends worth 2000 tokens and one inexact send inside the first.
fn two_cycles(store: &Store, now: i64) {
    let hour = 3_600_000;
    let first_reset = now - 3 * DAY_MS;
    let second_reset = now + 4 * DAY_MS;
    let record = |at, reading| store.activity.record_readings("codex", "codex-a.json", at, &[reading]);
    record(now - 6 * DAY_MS, weekly(8.0, first_reset));
    record(now - 4 * DAY_MS, weekly(48.0, first_reset));
    record(now - 3 * DAY_MS + 20 * 60_000, weekly(2.0, second_reset));
    record(now - hour, weekly(30.0, second_reset));
    let tokens = |uncached_input, output, exact| Tokens {
        uncached_input,
        cache_read: 0,
        cache_write: 0,
        output,
        reasoning: 0,
        exact,
    };
    for (id, at, tokens) in [
        ("r1", now - 5 * DAY_MS, tokens(1000, 0, true)),
        ("r2", now - 5 * DAY_MS + hour, tokens(0, 1000, true)),
        ("r3", now - 5 * DAY_MS + 2 * hour, tokens(500, 500, false)),
    ] {
        let mut row = send(id, 0, "codex", Outcome::Complete, None);
        row.at_ms = at;
        row.tokens = Some(tokens);
        store.activity.record_send(row);
    }
}

#[test]
fn cycles_report_unused_partial_and_tokens_per_percent() {
    let store = Store::new();
    let now = epoch_ms();
    two_cycles(&store, now);
    let panels = store.over(Range::Week, now);
    let cycles = panels["cycles"].as_array().unwrap();
    assert_eq!(cycles.len(), 2, "{panels}");
    let first = &cycles[0];
    assert_eq!(
        [
            &first["first"],
            &first["peak"],
            &first["last"],
            &first["ended"],
            &first["unused"],
            &first["partial"],
            &first["inRange"],
            &first["tokens"],
            &first["tokensPerPercent"]
        ],
        [
            &json!(8.0),
            &json!(48.0),
            &json!(48.0),
            &json!(true),
            &json!(52.0),
            &json!(true),
            &json!(true),
            &json!(2000),
            &json!(50.0)
        ],
        "{panels}"
    );
    let second = &cycles[1];
    assert_eq!(
        [&second["ended"], &second["unused"], &second["partial"]],
        [&json!(false), &Value::Null, &json!(false)],
        "{panels}"
    );
}

#[test]
fn the_day_range_keeps_the_latest_ended_cycle_from_before_it() {
    let store = Store::new();
    let now = epoch_ms();
    two_cycles(&store, now);
    let panels = store.over(Range::Day, now);
    let cycles = panels["cycles"].as_array().unwrap();
    assert_eq!(cycles.len(), 2, "{panels}");
    assert_eq!(
        [&cycles[0]["inRange"], &cycles[0]["unused"]],
        [&json!(false), &json!(52.0)],
        "{panels}"
    );
    assert_eq!(cycles[1]["inRange"], true, "{panels}");
}

#[test]
fn short_window_readings_make_no_cycles() {
    let store = Store::new();
    let now = epoch_ms();
    let short = |used_percent| crate::scheduler::Reading {
        long: false,
        used_percent,
        reset: None,
    };
    store
        .activity
        .record_readings("codex", "codex-a.json", now - 7_200_000, &[short(10.0)]);
    store
        .activity
        .record_readings("codex", "codex-a.json", now - 60_000, &[short(20.0)]);
    let panels = store.over(Range::Day, now);
    assert_eq!(panels["cycles"], json!([]), "{panels}");
}

#[test]
fn an_ended_cycle_in_range_leaves_out_the_one_before_it() {
    let store = Store::new();
    let now = epoch_ms();
    let hour = 3_600_000;
    let record = |at, reading| store.activity.record_readings("codex", "codex-a.json", at, &[reading]);
    record(now - 3 * DAY_MS, weekly(10.0, now - 2 * DAY_MS));
    record(now - 2 * DAY_MS + 20 * 60_000, weekly(2.0, now - hour));
    record(now - 5 * hour, weekly(40.0, now - hour));
    let panels = store.over(Range::Day, now);
    let cycles = panels["cycles"].as_array().unwrap();
    assert_eq!(cycles.len(), 1, "{panels}");
    assert_eq!(
        [&cycles[0]["inRange"], &cycles[0]["ended"]],
        [&json!(true), &json!(true)],
        "{panels}"
    );
}
