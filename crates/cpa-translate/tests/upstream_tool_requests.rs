use cpa_common::json as gj;
use cpa_core::format::Format;
use cpa_translate::{RequestCtx, ResponseCtx, go_stream, pair, translate_request};

#[test]
fn codex_preserves_explicit_tool_concurrency() {
    for client in [Format::OpenAI, Format::OpenAIResponse] {
        for (field, expected) in [
            ("", true),
            (",\"parallel_tool_calls\":false", false),
            (",\"parallel_tool_calls\":true", true),
            (",\"parallel_tool_calls\":null", true),
            (",\"parallel_tool_calls\":\"false\"", true),
            (",\"parallel_tool_calls\":0", true),
        ] {
            let raw = format!("{{\"messages\":[],\"input\":[]{field}}}");
            let out = translate_request(
                client,
                Format::Codex,
                &RequestCtx {
                    model: "m",
                    stream: true,
                },
                raw.as_bytes(),
            )
            .unwrap();
            assert_eq!(
                gj::get(&out, "parallel_tool_calls").bool(),
                expected,
                "{client:?}: {field}"
            );
        }
    }
    let raw = br#"{ "input": [], "stream": true, "store": false, "parallel_tool_calls": false, "include": ["reasoning.encrypted_content"], "opaque": 1e+02 }"#;
    let out = translate_request(
        Format::OpenAIResponse,
        Format::Codex,
        &RequestCtx {
            model: "m",
            stream: true,
        },
        raw,
    )
    .unwrap();
    assert_eq!(out, raw);
}

fn truncation_events(stop: &str) -> Vec<Vec<u8>> {
    [r#"{"type":"message_start","message":{"id":"msg_1","model":"m","usage":{"input_tokens":7,"output_tokens":0}}}"#.to_owned(),
    r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#.to_owned(),
    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}"#.to_owned(),
    r#"{"type":"content_block_stop","index":0}"#.to_owned(),
    r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_1","name":"lookup","input":{}}}"#.to_owned(),
    r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"query\":"}}"#.to_owned(),
    r#"{"type":"content_block_stop","index":1}"#.to_owned(),
    format!(r#"{{"type":"message_delta","delta":{{"stop_reason":"{stop}"}},"usage":{{"output_tokens":5}}}}"#),
    r#"{"type":"message_stop"}"#.to_owned()]
    .into_iter().map(|v| format!("data: {v}\n\n").into_bytes()).collect()
}

#[test]
fn claude_terminal_context_window_preserves_partial_output() {
    let ctx = ResponseCtx {
        model: "m",
        original_request: b"{}",
        translated_request: b"{}",
    };
    for stop in ["max_tokens", "model_context_window_exceeded"] {
        let events = truncation_events(stop);
        for client in [Format::OpenAI, Format::OpenAIResponse] {
            let output = (pair(client, Format::Claude).unwrap().non_stream)(&ctx, &events.concat()).unwrap();
            let mut stream = go_stream(client, Format::Claude).unwrap()(&ctx);
            let streamed = events
                .iter()
                .flat_map(|e| stream.line(e).unwrap())
                .flatten()
                .collect::<Vec<_>>();
            if client == Format::OpenAI {
                assert_eq!(gj::get(&output, "choices.0.finish_reason").str(), "length", "{stop}");
                assert_eq!(gj::get(&output, "choices.0.message.content").str(), "partial");
                assert_eq!(
                    gj::get(&output, "choices.0.message.tool_calls.0.function.name").str(),
                    "lookup"
                );
                assert_eq!(gj::get(&output, "usage.total_tokens").int(), 12);
                assert!(String::from_utf8_lossy(&streamed).contains("\"finish_reason\":\"length\""));
            } else {
                assert_eq!(gj::get(&output, "status").str(), "incomplete", "{stop}");
                assert_eq!(gj::get(&output, "incomplete_details.reason").str(), "max_output_tokens");
                assert_eq!(gj::get(&output, "usage.total_tokens").int(), 12);
                assert!(String::from_utf8_lossy(&output).contains("partial"));
                assert!(String::from_utf8_lossy(&output).contains("lookup"));
                assert!(String::from_utf8_lossy(&streamed).contains("response.incomplete"));
            }
        }
    }
}
