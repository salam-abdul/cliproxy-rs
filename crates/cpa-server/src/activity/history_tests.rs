use super::*;
use crate::activity::sends::{SendRow, Tokens};
use crate::activity::{Activity, DAY_MS, Range, epoch_ms};
use crate::request_logging::history::row::{Outcome, Source};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

const HOUR_MS: i64 = 3_600_000;

struct Store {
    dir: PathBuf,
    path: PathBuf,
    activity: Activity,
    /// On an hour boundary, so every record half an hour before it shares one bucket.
    now: i64,
}

impl Store {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("activity-history-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("activity.sqlite");
        let activity = Activity::start(path.clone(), Arc::new(AtomicU64::new(0))).unwrap();
        Self {
            dir,
            path,
            activity,
            now: epoch_ms() / HOUR_MS * HOUR_MS,
        }
    }

    fn read(&self, range: Range, provider: Option<&str>) -> Value {
        self.activity.flush();
        read(&self.path, range, provider, self.now, 0).unwrap()
    }

    /// A native request row on account `native-a`, `ago` milliseconds before `now`.
    fn event(&self, kind: &str, id: &str, outcome: &str, ago: i64) {
        self.event_on(Some("native-a"), kind, id, outcome, ago);
    }

    /// A native request row on `account`, or on none, as refusals and fallbacks are.
    fn event_on(&self, account: Option<&str>, kind: &str, id: &str, outcome: &str, ago: i64) {
        let at = chrono::DateTime::from_timestamp_millis(self.now - ago)
            .unwrap()
            .to_rfc3339();
        self.activity
            .record_event(&json!({"type": kind, "requestId": id, "at": at,
            "account": account, "model": "claude-test", "outcome": outcome}));
    }

    /// A proxy send half an hour before `now`, `send` milliseconds after the first.
    fn send(&self, request_id: &str, send: i32, provider: &str, outcome: Outcome, status: Option<i64>) -> SendRow {
        SendRow {
            request_id: request_id.into(),
            turn: 0,
            send,
            parent_request: None,
            at_ms: self.now - HOUR_MS / 2 + i64::from(send),
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

    /// A native send for request `request_id` from credential `cred-a`.
    fn native(&self, request_id: &str, send: i32, outcome: Outcome, status: Option<i64>) -> SendRow {
        let mut row = self.send(request_id, send, "claude", outcome, status);
        row.source = Source::Native;
        row.credential = Some("cred-a".into());
        row
    }

    fn record(&self, rows: impl IntoIterator<Item = SendRow>) {
        for row in rows {
            self.activity.record_send(row);
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Requests and failures summed over every bucket.
fn totals(history: &Value) -> (i64, i64) {
    history["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .fold((0, 0), |(requests, failures), bucket| {
            (
                requests + bucket["requests"].as_i64().unwrap(),
                failures + bucket["failures"].as_i64().unwrap(),
            )
        })
}

fn exact(uncached_input: i64, cache_read: i64, cache_write: i64, output: i64, exact: bool) -> Option<Tokens> {
    Some(Tokens {
        uncached_input,
        cache_read,
        cache_write,
        output,
        reasoning: 0,
        exact,
    })
}

#[test]
fn native_rows_before_the_first_send_are_not_read() {
    let store = Store::new();
    store.event("inference", "old", "attempt_limit", 2 * DAY_MS);
    let send = store.send("p1", 0, "codex", Outcome::Complete, None);
    let first = send.at_ms;
    store.record([send]);
    let history = store.read(Range::Week, None);
    assert_eq!(totals(&history), (1, 0), "{history}");
    assert_eq!(history["errors"], json!([]), "{history}");
    assert_eq!(history["oldest"], first, "{history}");
}

#[test]
fn a_native_request_keeps_its_reason_and_counts_once() {
    let store = Store::new();
    store.record([store.native("n1", 0, Outcome::Failed, Some(502))]);
    store.event("inference", "n1", "incomplete_stream", HOUR_MS / 2);
    let history = store.read(Range::Day, None);
    assert_eq!(totals(&history), (1, 1), "{history}");
    assert_eq!(
        history["errors"],
        json!([{"outcome": "incomplete_stream", "status": null, "requests": 1}]),
        "{history}"
    );
}

#[test]
fn a_quota_refusal_after_failover_sends_counts_once_as_claude() {
    let store = Store::new();
    store.record([
        store.native("n1", 0, Outcome::Failover, Some(429)),
        store.native("n1", 1, Outcome::Failover, Some(429)),
    ]);
    store.event("rejected", "n1", "quota_unavailable", HOUR_MS / 2);
    let history = store.read(Range::Day, None);
    assert_eq!(totals(&history), (1, 1), "{history}");
    assert_eq!(
        history["errors"],
        json!([{"outcome": "quota_unavailable", "status": null, "requests": 1}]),
        "{history}"
    );
    assert_eq!(history["providers"], json!(["claude"]), "{history}");
}

#[test]
fn fallbacks_count_once_under_the_native_row() {
    let codex_requests = |history: &Value| -> i64 {
        history["models"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|model| model["provider"] == "codex")
            .map(|model| model["requests"].as_i64().unwrap())
            .sum()
    };

    let served = Store::new();
    let mut fallback = served.send("p1", 0, "codex", Outcome::Complete, None);
    fallback.parent_request = Some("n1".into());
    served.record([served.native("n1", 0, Outcome::Failover, Some(429)), fallback]);
    served.event("fallback", "n1", "fallback_served", HOUR_MS / 2);
    let history = served.read(Range::Day, None);
    assert_eq!(totals(&history), (1, 0), "{history}");
    assert_eq!(history["providers"], json!(["claude"]), "{history}");
    assert_eq!(codex_requests(&history), 0, "{history}");

    let failed = Store::new();
    let mut fallback = failed.send("p2", 0, "codex", Outcome::Failed, Some(500));
    fallback.parent_request = Some("n2".into());
    failed.record([fallback]);
    failed.event("rejected", "n2", "quota_unavailable", HOUR_MS / 2);
    let history = failed.read(Range::Day, None);
    assert_eq!(totals(&history), (1, 1), "{history}");
    assert_eq!(
        history["errors"],
        json!([{"outcome": "quota_unavailable", "status": null, "requests": 1}]),
        "{history}"
    );
    assert_eq!(history["providers"], json!(["claude"]), "{history}");
    assert_eq!(codex_requests(&history), 0, "{history}");
}

#[test]
fn a_proxy_request_is_its_last_send() {
    let store = Store::new();
    store.record([
        store.send("r1", 0, "codex", Outcome::Failover, Some(503)),
        store.send("r1", 1, "codex", Outcome::Complete, None),
    ]);
    let history = store.read(Range::Day, None);
    assert_eq!(totals(&history), (1, 0), "{history}");
    assert_eq!(history["errors"], json!([]), "{history}");

    store.record([
        store.send("r2", 0, "codex", Outcome::Failed, Some(500)),
        store.send("r3", 0, "codex", Outcome::Failed, Some(429)),
    ]);
    let history = store.read(Range::Day, None);
    assert_eq!(totals(&history), (3, 2), "{history}");
    assert_eq!(
        history["errors"],
        json!([
            {"outcome": "failed", "status": 429, "requests": 1},
            {"outcome": "failed", "status": 500, "requests": 1}
        ]),
        "{history}"
    );
}

#[test]
fn tokens_sum_exact_sends_only() {
    let store = Store::new();
    let mut first = store.send("r1", 0, "codex", Outcome::Failover, Some(503));
    first.model = Some("model-a".into());
    first.tokens = exact(100, 10, 5, 20, true);
    let mut last = store.send("r1", 1, "codex", Outcome::Complete, None);
    last.model = Some("model-b".into());
    last.tokens = exact(999, 999, 999, 999, false);
    store.record([first, last]);
    let history = store.read(Range::Day, None);
    assert_eq!(
        history["tokens"],
        json!({"uncachedInputTokens": 100, "cacheReadTokens": 10, "cacheWriteTokens": 5,
            "outputTokens": 20, "sends": 2, "exactSends": 1}),
        "{history}"
    );
    assert_eq!(
        history["models"],
        json!([
            {"provider": "codex", "model": "model-b", "requests": 1, "uncachedInputTokens": 0,
                "cacheReadTokens": 0, "cacheWriteTokens": 0, "outputTokens": 0, "sends": 1, "exactSends": 0},
            {"provider": "codex", "model": "model-a", "requests": 0, "uncachedInputTokens": 100,
                "cacheReadTokens": 10, "cacheWriteTokens": 5, "outputTokens": 20, "sends": 1, "exactSends": 1}
        ]),
        "{history}"
    );
    assert_eq!(history["modelsTruncated"], false, "{history}");
}

#[test]
fn the_filter_narrows_requests_not_providers_accounts_or_statuses() {
    let store = Store::new();
    let mut codex = store.send("r1", 0, "codex", Outcome::Failed, Some(500));
    codex.tokens = exact(7, 0, 0, 3, true);
    let mut claude = store.native("n1", 0, Outcome::Failed, Some(502));
    claude.tokens = exact(50, 0, 0, 5, true);
    store.record([codex, claude]);
    store.event("inference", "n1", "incomplete_stream", HOUR_MS / 2);
    store
        .activity
        .record_status(&json!({"observedAt": store.now - HOUR_MS / 2, "model": "claude",
        "accounts": [{"id": "native-a", "availability": "included", "includedQuota": {"fiveHour": 30}}]}));
    let all = store.read(Range::Day, None);
    let history = store.read(Range::Day, Some("codex"));
    assert_eq!(totals(&all), (2, 2), "{all}");
    assert_eq!(totals(&history), (1, 1), "{history}");
    assert_eq!(
        history["errors"],
        json!([{"outcome": "failed", "status": 500, "requests": 1}]),
        "{history}"
    );
    assert_eq!(history["tokens"]["uncachedInputTokens"], 7, "{history}");
    assert_eq!(history["tokens"]["sends"], 1, "{history}");
    let models = history["models"].as_array().unwrap();
    assert!(!models.is_empty(), "{history}");
    assert!(models.iter().all(|model| model["provider"] == "codex"), "{history}");
    assert_eq!(history["providers"], json!(["claude", "codex"]), "{history}");
    assert_eq!(history["accountBuckets"], all["accountBuckets"], "{history}");
    assert_eq!(history["accountBuckets"].as_array().unwrap().len(), 1, "{history}");
    assert_eq!(history["statuses"], all["statuses"], "{history}");
    assert_eq!(history["statuses"].as_array().unwrap().len(), 1, "{history}");
}

#[test]
fn account_buckets_use_the_send_credential() {
    let store = Store::new();
    let mut proxy = store.send("p1", 0, "claude", Outcome::Complete, None);
    proxy.credential = Some("cred-a".into());
    store.record([store.native("n1", 0, Outcome::Complete, None), proxy]);
    store.event("inference", "n1", "success", HOUR_MS / 2);
    let history = store.read(Range::Day, None);
    assert_eq!(
        history["accountBuckets"],
        json!([{"credential": "cred-a", "at": store.now - HOUR_MS, "requests": 2, "failures": 0}]),
        "{history}"
    );
}

#[test]
fn sends_without_token_counts_leave_totals_whole() {
    let store = Store::new();
    let refused = store.send("r1", 0, "codex", Outcome::Failed, Some(429));
    let mut served = store.send("r2", 0, "codex", Outcome::Complete, None);
    served.tokens = exact(40, 0, 0, 8, true);
    store.record([refused, served]);
    let history = store.read(Range::Day, None);
    assert_eq!(
        history["tokens"],
        json!({"uncachedInputTokens": 40, "cacheReadTokens": 0, "cacheWriteTokens": 0,
            "outputTokens": 8, "sends": 1, "exactSends": 1}),
        "{history}"
    );
    assert_eq!(
        history["models"],
        json!([{"provider": "codex", "model": "model-test", "requests": 2, "uncachedInputTokens": 40,
            "cacheReadTokens": 0, "cacheWriteTokens": 0, "outputTokens": 8, "sends": 1, "exactSends": 1}]),
        "{history}"
    );
}

#[test]
fn refusals_and_fallbacks_without_an_account_stay_off_account_cards() {
    let refused = Store::new();
    refused.record([refused.native("n1", 0, Outcome::Failover, Some(429))]);
    refused.event_on(None, "rejected", "n1", "quota_unavailable", HOUR_MS / 2);
    let history = refused.read(Range::Day, None);
    assert_eq!(history["accountBuckets"], json!([]), "{history}");
    assert_eq!(totals(&history), (1, 1), "{history}");

    let served = Store::new();
    served.record([served.native("n2", 0, Outcome::Failover, Some(429))]);
    served.event_on(None, "fallback", "n2", "fallback_served", HOUR_MS / 2);
    let history = served.read(Range::Day, None);
    assert_eq!(history["accountBuckets"], json!([]), "{history}");
    assert_eq!(totals(&history), (1, 0), "{history}");
}

#[test]
fn a_native_request_counts_under_its_last_send_credential() {
    let store = Store::new();
    let mut first = store.native("n1", 0, Outcome::Failover, Some(429));
    first.credential = Some("cred-a".into());
    let mut last = store.native("n1", 1, Outcome::Complete, None);
    last.credential = Some("cred-b".into());
    store.record([first, last]);
    store.event("inference", "n1", "success", HOUR_MS / 2);
    let history = store.read(Range::Day, None);
    assert_eq!(
        history["accountBuckets"],
        json!([{"credential": "cred-b", "at": store.now - HOUR_MS, "requests": 1, "failures": 0}]),
        "{history}"
    );
}

#[test]
fn the_read_starts_at_the_first_request_when_sends_go_back_further() {
    let store = Store::new();
    let mut early = store.send("p1", 0, "codex", Outcome::Failed, Some(500));
    early.at_ms = store.now - 3 * HOUR_MS;
    early.tokens = exact(11, 0, 0, 2, true);
    let mut late = store.send("p2", 0, "codex", Outcome::Complete, None);
    late.tokens = exact(5, 0, 0, 1, true);
    store.record([early, late]);
    store.event("inference", "n1", "success", HOUR_MS);
    let history = store.read(Range::Day, None);
    assert_eq!(history["oldest"], store.now - HOUR_MS, "{history}");
    assert_eq!(totals(&history), (2, 0), "{history}");
    assert_eq!(history["errors"], json!([]), "{history}");
    assert_eq!(history["tokens"]["uncachedInputTokens"], 5, "{history}");
    assert_eq!(history["tokens"]["sends"], 1, "{history}");
}
