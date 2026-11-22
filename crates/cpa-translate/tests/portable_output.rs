//! Portable output: what the Claude-facing Gemini and OpenAI response translators emit
//! when the client will replay the turn to another provider. Anthropic rejects thinking
//! blocks it did not sign and needs tool-use IDs that are unique in a conversation.

use cpa_core::format::Format;
use cpa_translate::{ResponseCtx, go_stream, pair};
use serde_json::Value;

const STREAMING: &[u8] = br#"{"stream":true,"tools":[{"name":"Bash","input_schema":{"type":"object"}}]}"#;
const BUFFERED: &[u8] = br#"{"tools":[{"name":"Bash","input_schema":{"type":"object"}}]}"#;

fn ctx(original_request: &[u8], portable: bool) -> ResponseCtx<'_> {
    ResponseCtx {
        model: "m",
        original_request,
        translated_request: b"{}",
        portable,
    }
}

fn gemini_lines() -> Vec<&'static [u8]> {
    vec![
        br#"{"responseId":"r","modelVersion":"g","candidates":[{"content":{"parts":[{"text":"pondering","thought":true,"thoughtSignature":"c2ln"}]}}]}"#,
        br#"{"candidates":[{"content":{"parts":[{"text":"visible","thoughtSignature":"c2ln"}]}}]}"#,
        br#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"Bash","args":{"command":"ls"}},"thoughtSignature":"c2ln"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2}}"#,
        b"[DONE]",
    ]
}

const GEMINI_BODY: &[u8] = br#"{"responseId":"r","modelVersion":"g","candidates":[{"content":{"parts":[{"text":"pondering","thought":true,"thoughtSignature":"c2ln"},{"text":"visible","thoughtSignature":"c2ln"},{"functionCall":{"name":"Bash","args":{"command":"ls"}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2}}"#;

fn openai_lines() -> Vec<&'static [u8]> {
    vec![
        br#"data: {"id":"c","model":"local","choices":[{"index":0,"delta":{"reasoning_content":"pondering"}}]}"#,
        br#"data: {"id":"c","model":"local","choices":[{"index":0,"delta":{"content":"visible"}}]}"#,
        br#"data: {"id":"c","model":"local","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"Bash","arguments":"{\"command\":\"ls\"}"}}]}}]}"#,
        br#"data: {"id":"c","model":"local","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#,
        b"data: [DONE]",
    ]
}

const OPENAI_BODY: &[u8] = br#"{"id":"c","model":"local","choices":[{"index":0,"message":{"role":"assistant","reasoning_content":"pondering","content":"visible","tool_calls":[{"id":"call_1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"ls\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#;

/// The `data:` payloads of a translated Claude stream, in order.
fn stream(upstream: Format, lines: &[&[u8]], portable: bool) -> Vec<Value> {
    let ctx = ctx(STREAMING, portable);
    let mut translator = go_stream(Format::Claude, upstream).unwrap()(&ctx);
    let out: Vec<u8> = lines
        .iter()
        .flat_map(|line| translator.line(line).unwrap())
        .flatten()
        .collect();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

fn buffered(upstream: Format, body: &[u8], portable: bool) -> Value {
    let out = (pair(Format::Claude, upstream).unwrap().non_stream)(&ctx(BUFFERED, portable), body).unwrap();
    serde_json::from_slice(&out).unwrap()
}

fn has_thinking(value: &Value) -> bool {
    let text = value.to_string();
    text.contains("thinking") || text.contains("signature")
}

fn block_starts(events: &[Value]) -> Vec<(i64, String)> {
    events
        .iter()
        .filter(|e| e["type"] == "content_block_start")
        .map(|e| {
            (
                e["index"].as_i64().unwrap(),
                e["content_block"]["type"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn text_deltas(events: &[Value]) -> String {
    events
        .iter()
        .filter(|e| e["delta"]["type"] == "text_delta")
        .map(|e| e["delta"]["text"].as_str().unwrap())
        .collect()
}

fn tool_ids(message: &Value) -> Vec<String> {
    message["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|block| block["type"] == "tool_use")
        .map(|block| block["id"].as_str().unwrap().to_owned())
        .collect()
}

fn valid_tool_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[test]
fn portable_gemini_stream_has_no_thinking_and_keeps_signed_text() {
    let events = stream(Format::Gemini, &gemini_lines(), true);
    assert!(!events.iter().any(has_thinking), "{events:?}");
    assert_eq!(text_deltas(&events), "visible");
    assert_eq!(
        block_starts(&events),
        [(0, "text".to_owned()), (1, "tool_use".to_owned())]
    );
    assert_eq!(events.last().unwrap()["type"], "message_stop");
}

#[test]
fn portable_gemini_buffered_has_no_thinking_and_keeps_signed_text() {
    let message = buffered(Format::Gemini, GEMINI_BODY, true);
    assert!(!has_thinking(&message), "{message}");
    let kinds: Vec<&str> = message["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|block| block["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["text", "tool_use"]);
    assert_eq!(message["content"][0]["text"], "visible");
}

#[test]
fn portable_openai_stream_drops_reasoning() {
    let events = stream(Format::OpenAI, &openai_lines(), true);
    assert!(!events.iter().any(has_thinking), "{events:?}");
    assert_eq!(text_deltas(&events), "visible");
    assert_eq!(
        block_starts(&events),
        [(0, "text".to_owned()), (1, "tool_use".to_owned())]
    );
    assert_eq!(events.last().unwrap()["type"], "message_stop");
}

#[test]
fn portable_openai_buffered_drops_reasoning() {
    let message = buffered(Format::OpenAI, OPENAI_BODY, true);
    assert!(!has_thinking(&message), "{message}");
    assert_eq!(
        message["content"][0],
        serde_json::json!({"type":"text","text":"visible"})
    );
    assert_eq!(message["content"][1]["type"], "tool_use");
}

#[test]
fn portable_thought_only_stream_still_terminates() {
    let lines: Vec<&[u8]> = vec![
        br#"{"responseId":"r","candidates":[{"content":{"parts":[{"text":"pondering","thought":true}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":0}}"#,
        b"[DONE]",
    ];
    let events = stream(Format::Gemini, &lines, true);
    assert!(!events.iter().any(has_thinking), "{events:?}");
    let kinds: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["message_start", "message_delta", "message_stop"]);
}

#[test]
fn portable_tool_ids_differ_across_responses() {
    for (upstream, body) in [(Format::Gemini, GEMINI_BODY), (Format::OpenAI, OPENAI_BODY)] {
        let first = tool_ids(&buffered(upstream, body, true));
        let second = tool_ids(&buffered(upstream, body, true));
        assert_eq!(first.len(), 1);
        assert_ne!(first, second, "{upstream:?}");
        assert!(
            valid_tool_id(&first[0]) && valid_tool_id(&second[0]),
            "{first:?} {second:?}"
        );
    }
    let id = |events: &[Value]| {
        events
            .iter()
            .find(|e| e["content_block"]["type"] == "tool_use")
            .unwrap()["content_block"]["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let first = id(&stream(Format::OpenAI, &openai_lines(), true));
    let second = id(&stream(Format::OpenAI, &openai_lines(), true));
    assert_ne!(first, second);
    assert!(first.starts_with("call_1-") && valid_tool_id(&first), "{first}");
}

#[test]
fn non_portable_output_is_unchanged() {
    let gemini = buffered(Format::Gemini, GEMINI_BODY, false);
    assert_eq!(gemini["content"][0]["type"], "thinking");
    assert_eq!(gemini["content"][0]["signature"], "c2ln");
    assert_eq!(tool_ids(&gemini), ["Bash-1"]);
    let openai = buffered(Format::OpenAI, OPENAI_BODY, false);
    assert_eq!(tool_ids(&openai), ["call_1"]);
    assert!(has_thinking(&openai));
    let events = stream(Format::OpenAI, &openai_lines(), false);
    assert_eq!(block_starts(&events)[0], (0, "thinking".to_owned()));
    let events = stream(Format::Gemini, &gemini_lines(), false);
    assert_eq!(block_starts(&events)[0], (0, "thinking".to_owned()));
}
