use super::*;
use crate::activity::{Activity, DAY_MS, MAX_READINGS, MAX_REQUESTS, MAX_STATUSES, QUEUE, epoch_ms, prune, trim};
use crate::usage_record::Detail;
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

struct Store {
    dir: PathBuf,
    path: PathBuf,
}

impl Store {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("activity-sends-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("activity.sqlite");
        Self { dir, path }
    }

    fn start(&self) -> (Activity, Arc<AtomicU64>) {
        let lost = Arc::new(AtomicU64::new(0));
        (Activity::start(self.path.clone(), lost.clone()).unwrap(), lost)
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn row(request_id: &str, send: i32) -> SendRow {
    SendRow {
        request_id: request_id.into(),
        turn: 0,
        send,
        parent_request: None,
        at_ms: epoch_ms(),
        source: Source::Proxy,
        provider: "codex".into(),
        credential: Some("codex-account.json".into()),
        model: Some("gpt-test".into()),
        outcome: Outcome::Complete,
        status: Some(200),
        stream: true,
        start_ms: 3,
        latency_ms: Some(250),
        ttft: None,
        tokens: None,
    }
}

#[test]
fn two_sends_of_one_request_are_two_rows() {
    let store = Store::new();
    let (activity, _) = store.start();
    activity.record_send(row("r1", 0));
    activity.record_send(row("r1", 1));
    activity.flush();
    let rows = activity.sends().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["send"], 0);
    assert_eq!(rows[1]["send"], 1);
    assert_eq!(rows[0]["request_id"], "r1");
    assert_eq!(rows[1]["request_id"], "r1");
}

#[test]
fn a_send_keeps_every_field_it_was_given() {
    let store = Store::new();
    let (activity, _) = store.start();
    let mut sent = row("r1", 0);
    sent.turn = 2;
    sent.parent_request = Some("parent".into());
    sent.ttft = Some((120, TtftKind::Token));
    sent.tokens = Some(Tokens {
        uncached_input: 100,
        cache_read: 10,
        cache_write: 5,
        output: 20,
        reasoning: 3,
        exact: true,
    });
    let at = sent.at_ms;
    activity.record_send(sent);
    activity.flush();
    let rows = activity.sends().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0],
        serde_json::json!({
            "request_id": "r1", "turn": 2, "send": 0, "parent_request": "parent", "at_ms": at,
            "source": "proxy", "provider": "codex", "credential": "codex-account.json",
            "model": "gpt-test", "outcome": "complete", "status": 200, "stream": 1,
            "start_ms": 3, "latency_ms": 250, "ttft_ms": 120, "ttft_kind": "token",
            "uncached_input_tokens": 100, "cache_read_tokens": 10, "cache_write_tokens": 5,
            "output_tokens": 20, "reasoning_tokens": 3, "tokens_exact": 1
        })
    );
}

#[test]
fn a_send_without_a_first_response_has_neither_time_nor_kind() {
    let store = Store::new();
    let (activity, _) = store.start();
    activity.record_send(row("r1", 0));
    activity.flush();
    let rows = activity.sends().unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0]["ttft_ms"].is_null());
    assert!(rows[0]["ttft_kind"].is_null());
}

#[test]
fn a_file_from_before_sends_existed_keeps_its_requests() {
    let store = Store::new();
    {
        let conn = Connection::open(&store.path).unwrap();
        conn.execute_batch(
            "CREATE TABLE requests (
                request_id TEXT PRIMARY KEY, at_ms INTEGER NOT NULL, account TEXT, model TEXT,
                outcome TEXT NOT NULL, status INTEGER, input_tokens INTEGER, output_tokens INTEGER,
                cache_read_tokens INTEGER, cache_write_tokens INTEGER, attempts INTEGER
            );
            CREATE INDEX requests_by_time ON requests(at_ms);
            CREATE INDEX requests_by_account_time ON requests(account, at_ms);
            CREATE TABLE account_status (
                account TEXT NOT NULL, model TEXT NOT NULL, bucket_ms INTEGER NOT NULL,
                at_ms INTEGER NOT NULL, availability TEXT NOT NULL,
                five_hour REAL, weekly REAL, five_hour_reset INTEGER, weekly_reset INTEGER,
                PRIMARY KEY(account, model, bucket_ms, availability)
            );
            CREATE INDEX account_status_by_time ON account_status(at_ms);
            CREATE TABLE history_meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO requests (request_id, at_ms, outcome) VALUES ('old', ?1, 'success')",
            [epoch_ms()],
        )
        .unwrap();
    }
    let (activity, _) = store.start();
    activity.record_send(row("r1", 0));
    activity.flush();
    assert_eq!(activity.sends().unwrap().len(), 1);
    let conn = Connection::open(&store.path).unwrap();
    let requests: Vec<String> = conn
        .prepare("SELECT request_id FROM requests")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(requests, ["old"]);
}

#[test]
fn sends_past_31_days_or_the_cap_are_removed_oldest_first() {
    let store = Store::new();
    let (activity, _) = store.start();
    let now = epoch_ms();
    let mut old = row("old", 0);
    old.at_ms = now - 32 * DAY_MS;
    let mut recent = row("recent", 0);
    recent.at_ms = now;
    activity.record_send(old);
    activity.record_send(recent);
    activity.flush();
    let conn = Connection::open(&store.path).unwrap();
    prune(&conn, now).unwrap();
    let ids = |rows: Vec<serde_json::Value>| -> Vec<String> {
        rows.iter()
            .map(|r| r["request_id"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(ids(activity.sends().unwrap()), ["recent"]);
    // Recorded after the newer row, so trimming by time and not by insertion keeps "recent".
    let mut earlier = row("earlier", 0);
    earlier.at_ms = now - 1_000;
    activity.record_send(earlier);
    activity.flush();
    assert_eq!(ids(activity.sends().unwrap()), ["earlier", "recent"]);
    trim(&conn, MAX_REQUESTS, MAX_STATUSES, 1, MAX_READINGS).unwrap();
    assert_eq!(ids(activity.sends().unwrap()), ["recent"]);
}

#[test]
fn a_send_whose_request_id_cannot_be_stored_is_counted_as_lost() {
    let store = Store::new();
    let (activity, lost) = store.start();
    activity.record_send(row(&"r".repeat(257), 0));
    activity.flush();
    assert_eq!(lost.load(Ordering::Relaxed), 1);
    assert!(activity.sends().unwrap().is_empty());
}

#[test]
fn a_full_queue_counts_the_send_as_lost_and_returns() {
    let store = Store::new();
    let (activity, lost) = store.start();
    let release = activity.hold();
    for i in 0..QUEUE + 10 {
        activity.record_send(row(&format!("r{i}"), 0));
    }
    assert_eq!(lost.load(Ordering::Relaxed), 10);
    drop(release);
    activity.flush();
    assert_eq!(activity.sends().unwrap().len(), QUEUE);
}

#[test]
fn claude_input_is_already_uncached() {
    let d = Detail {
        input: 100,
        cache_read: 10,
        cache_creation: 5,
        output: 20,
        ..Detail::default()
    };
    assert_eq!(
        Tokens::of(&d, "claude", "claude"),
        Some(Tokens {
            uncached_input: 100,
            cache_read: 10,
            cache_write: 5,
            output: 20,
            reasoning: 0,
            exact: true
        })
    );
}

#[test]
fn gemini_output_gains_the_thinking_it_reports_apart() {
    let d = Detail {
        input: 100,
        output: 20,
        reasoning: 7,
        ..Detail::default()
    };
    let tokens = Tokens::of(&d, "gemini", "gemini").unwrap();
    assert_eq!((tokens.output, tokens.reasoning, tokens.exact), (27, 7, true));
}

#[test]
fn counts_that_do_not_add_up_are_kept_and_marked_inexact() {
    // cache larger than the input it is part of
    let d = Detail {
        input: 5,
        cache_read: 10,
        output: 1,
        ..Detail::default()
    };
    let tokens = Tokens::of(&d, "openai-compatibility", "openaicompatexecutor").unwrap();
    assert_eq!((tokens.uncached_input, tokens.cache_read, tokens.exact), (5, 10, false));
}

#[test]
fn a_send_with_no_usage_has_no_tokens() {
    assert_eq!(Tokens::of(&Detail::default(), "claude", "claude"), None);
}
