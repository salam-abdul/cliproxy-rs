//! WebSocket request normalization and retained-state regressions.

use super::*;
use crate::websocket_tools::{Retained, prepare_fallback_turn};
use serde_json::Value;
use std::sync::LazyLock;

static VECTORS: LazyLock<Vec<Value>> = LazyLock::new(|| {
    serde_json::from_str::<Value>(include_str!("../tests/fixtures/websocket_requests.json"))
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
});

fn vectors(fun: &str) -> Vec<&'static Value> {
    let out: Vec<&Value> = VECTORS.iter().filter(|v| v["fn"] == fun).collect();
    assert!(!out.is_empty(), "no {fun} vectors");
    out
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or_default()
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    v[key]
        .as_array()
        .into_iter()
        .flatten()
        .map(|x| x.as_str().unwrap().to_owned())
        .collect()
}

fn expected_error(v: &Value) -> Option<WsError> {
    v["err_status"].as_u64().map(|status| WsError {
        status: status as u16,
        message: s(v, "err").to_owned(),
    })
}

#[test]
fn normalize_matches_go() {
    for v in vectors("normalize") {
        let got = normalize(
            s(v, "raw"),
            s(v, "last"),
            s(v, "output"),
            s(v, "last_id"),
            &strings(v, "pending"),
            false,
            v["bypass"].as_bool().unwrap_or(false),
        );
        match expected_error(v) {
            Some(error) => assert_eq!(got, Err(error), "raw {}", s(v, "raw")),
            None => assert_eq!(
                got,
                Ok((s(v, "out").to_owned(), s(v, "updated").to_owned())),
                "raw {} last {}",
                s(v, "raw"),
                s(v, "last")
            ),
        }
    }
}

#[test]
fn passthrough_matches_go() {
    for v in vectors("passthrough") {
        let got = normalize_passthrough(s(v, "raw"), s(v, "model"));
        match expected_error(v) {
            Some(error) => assert_eq!(got, Err(error), "raw {}", s(v, "raw")),
            None => assert_eq!(got, Ok(s(v, "out").to_owned()), "raw {}", s(v, "raw")),
        }
    }
}

#[test]
fn prewarm_followup_matches_go() {
    for v in vectors("prewarm_followup") {
        let got = prewarm_followup(s(v, "raw"), s(v, "last"));
        match expected_error(v) {
            Some(error) => assert_eq!(got, Err(error), "raw {}", s(v, "raw")),
            None => assert_eq!(
                got,
                Ok((s(v, "out").to_owned(), s(v, "updated").to_owned())),
                "raw {}",
                s(v, "raw")
            ),
        }
    }
}

#[test]
fn local_prewarm_detection_matches_go() {
    for v in vectors("prewarm_local") {
        assert_eq!(
            is_local_prewarm(s(v, "raw")),
            v["bool"].as_bool().unwrap_or(false),
            "{}",
            s(v, "raw")
        );
    }
}

#[test]
fn prewarm_payloads_match_go() {
    let v = vectors("prewarm_payloads")[0];
    let go = strings(v, "list");
    let id = gjson::get(&go[0], "response.id").str().to_owned();
    let created_at = gjson::get(&go[0], "response.created_at").i64();
    assert!(id.starts_with("resp_prewarm_"));
    assert_eq!(prewarm_payloads(s(v, "raw"), &id, created_at).to_vec(), go);
}

#[test]
fn chunk_payloads_match_go() {
    for v in vectors("chunk") {
        assert_eq!(
            payloads_from_chunk(s(v, "raw").as_bytes()),
            strings(v, "list"),
            "{:?}",
            s(v, "raw")
        );
    }
}

#[test]
fn error_payloads_match_go() {
    for v in vectors("error_payload") {
        let status = v["status"].as_u64().unwrap_or(0) as u16;
        assert_eq!(
            error_payload(status, s(v, "raw"), None),
            s(v, "out"),
            "{status} {:?}",
            s(v, "raw")
        );
    }
}

#[test]
fn exposure_matches_go() {
    for v in vectors("expose") {
        let status = v["status"].as_u64().unwrap() as u16;
        assert_eq!(
            crate::classify::is_request_fault(status, s(v, "raw")),
            v["bool"].as_bool().unwrap_or(false),
            "{status} {}",
            s(v, "raw")
        );
    }
}

#[test]
fn completion_restore_and_pending_calls_match_go() {
    for v in vectors("completion") {
        let mut turn = Turn::default();
        let mut got = Vec::new();
        for event in strings(v, "events") {
            turn.collect(&event);
            let kind = gjson::get(&event, "type").str().to_owned();
            let event = if kind == "response.completed" || kind == "response.done" {
                let restored = turn.restore_completion(event);
                got.push(restored.clone());
                got.push(turn.completed_output(&restored));
                restored
            } else {
                event
            };
            turn.track_pending(&event);
        }
        assert_eq!(got, strings(v, "list"), "events {:?}", v["events"]);
        assert_eq!(turn.pending().join(","), s(v, "out"));
    }
}

/// The Go sequence runs against shared caches: `bypass` marks a committed turn and
/// `events` are upstream events recorded into it first.
#[test]
fn tool_call_repair_sequence_matches_go() {
    let all = vectors("repair");
    let (keyed, unkeyed): (Vec<_>, Vec<_>) = all.into_iter().partition(|v| !s(v, "key").is_empty());
    let key = s(keyed[0], "key");
    let retained = Retained::new(key.to_owned());
    for v in keyed {
        let (out, turn) = prepare_fallback_turn(key, s(v, "raw").to_owned());
        assert_eq!(out, s(v, "out"), "raw {}", s(v, "raw"));
        let mut turn = turn.expect("a session key records the turn");
        for event in strings(v, "events") {
            turn.record_response(&event);
        }
        if v["bypass"].as_bool().unwrap_or(false) {
            turn.commit();
        }
    }
    drop(retained);
    for v in unkeyed {
        assert_eq!(prepare_fallback_turn("", s(v, "raw").to_owned()).0, s(v, "out"));
    }
}

#[test]
fn close_reason_truncation_matches_go() {
    for v in vectors("truncate") {
        let max = v["max"].as_u64().unwrap_or(0) as usize;
        assert_eq!(truncate_reason(s(v, "raw"), max), s(v, "out"));
    }
}

const PENDING_LAST: &str =
    r#"{"model":"test-model","stream":true,"instructions":"be helpful","input":[{"type":"message","id":"msg-1"}]}"#;
const PENDING_OUTPUT: &str = r#"[{"type":"function_call","id":"fc-1","call_id":"call-1"}]"#;

fn input_ids(json: &str) -> Vec<String> {
    gjson::get(json, "input")
        .array()
        .iter()
        .map(|item| item.get("id").str().to_owned())
        .collect()
}

#[test]
fn unanswered_tool_calls_replace_the_transcript_without_incremental_upstream() {
    let raw = r#"{"type":"response.create","input":[{"type":"message","role":"user","id":"summary-1","content":"compacted summary"}]}"#;
    let (normalized, next) = normalize(
        raw,
        PENDING_LAST,
        PENDING_OUTPUT,
        "resp-1",
        &["call-1".into()],
        false,
        false,
    )
    .expect("a replacement is a valid request");
    assert!(
        !gjson::get(&normalized, "previous_response_id").exists(),
        "{normalized}"
    );
    assert_eq!(
        input_ids(&normalized),
        ["summary-1"],
        "the stale call must not be merged"
    );
    assert_eq!(gjson::get(&normalized, "model").str(), "test-model");
    assert_eq!(gjson::get(&normalized, "instructions").str(), "be helpful");
    assert_eq!(next, normalized);
}

#[test]
fn answered_tool_calls_keep_the_merged_transcript_without_incremental_upstream() {
    let raw =
        r#"{"type":"response.create","input":[{"type":"function_call_output","call_id":"call-1","id":"tool-out-1"}]}"#;
    let (normalized, _) = normalize(
        raw,
        PENDING_LAST,
        PENDING_OUTPUT,
        "resp-1",
        &["call-1".into()],
        false,
        false,
    )
    .expect("an answered call merges");
    assert_eq!(input_ids(&normalized), ["msg-1", "fc-1", "tool-out-1"]);
}

#[test]
fn replayed_items_drop_only_output_only_token_counts() {
    // Escaped key spelling, duplicate members and an emptied passthrough.
    assert_eq!(
        replay_sanitized(r#"{"type":"message","\u0069nternal_chat_message_metadata_passthrough":{"content_token_count":1,"turn_id":"t","content_token_count":2},"id":"m"}"#).as_deref(),
        Some(r#"{"type":"message","\u0069nternal_chat_message_metadata_passthrough":{"turn_id":"t"},"id":"m"}"#)
    );
    assert_eq!(
        replay_sanitized(
            r#"{"type":"message", "internal_chat_message_metadata_passthrough" : {"content_token_count":1}, "id":"m"}"#
        )
        .as_deref(),
        Some(r#"{"type":"message","id":"m"}"#)
    );
    for unchanged in [
        r#"{"type":"message","internal_chat_message_metadata_passthrough":{"turn_id":"t"}}"#,
        r#"{"type":"message","internal_chat_message_metadata_passthrough":{}}"#,
        r#"{"type":"message","content_token_count":3}"#,
        r#"{"internal_chat_message_metadata_passthrough":{"content_token_count":1},"content":"#,
        r#"{"internal_chat_message_metadata_passthrough":{"content_token_count":1}} trailing"#,
    ] {
        assert_eq!(replay_sanitized(unchanged), None, "{unchanged}");
    }
}

#[test]
fn merged_transcripts_sanitize_replayed_output_but_not_client_input() {
    let last = r#"{"model":"m","input":[{"type":"message","id":"msg-1","internal_chat_message_metadata_passthrough":{"content_token_count":5}}]}"#;
    let output = r#"[{"type":"function_call","id":"fc-1","call_id":"call-1","internal_chat_message_metadata_passthrough":{"content_token_count":7,"turn_id":"u"}}]"#;
    let append = r#"[{"type":"function_call_output","call_id":"call-1","id":"out-1","internal_chat_message_metadata_passthrough":{"content_token_count":9}}]"#;
    assert_eq!(
        merge_input(last, output, append).unwrap(),
        concat!(
            r#"[{"type":"message","id":"msg-1","internal_chat_message_metadata_passthrough":{"content_token_count":5}},"#,
            r#"{"type":"function_call","id":"fc-1","call_id":"call-1","internal_chat_message_metadata_passthrough":{"turn_id":"u"}},"#,
            r#"{"type":"function_call_output","call_id":"call-1","id":"out-1","internal_chat_message_metadata_passthrough":{"content_token_count":9}}]"#
        )
    );
}
