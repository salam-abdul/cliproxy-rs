use cpa_common::json as gj;
use cpa_core::format::Format;
use cpa_translate::{ResponseCtx, pair};

#[test]
fn schema_arguments_closed_empty_tools_drop_injected_values() {
    for upstream in [Format::Gemini, Format::Antigravity] {
        let ctx = ResponseCtx {
            model: "claude-test",
            original_request:
                br#"{"tools":[{"name":"t","input_schema":{"type":"object","additionalProperties":false}}]}"#,
            translated_request: b"{}",
        };
        let raw =
            br#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"t","args":{"reason":"because"}}}]}}]}"#;
        let raw = if upstream == Format::Antigravity {
            [b"{\"response\":".as_slice(), raw, b"}"].concat()
        } else {
            raw.to_vec()
        };
        let output = (pair(Format::Claude, upstream).unwrap().non_stream)(&ctx, &raw).unwrap();
        assert_eq!(gj::get(&output, "content.0.input").raw(), b"{}", "{upstream:?}");
    }
}

#[test]
fn schema_arguments_implicitly_open_objects_keep_real_values() {
    for upstream in [Format::Gemini, Format::Antigravity] {
        let ctx = ResponseCtx {
            model: "claude-test",
            original_request: br#"{"tools":[{"name":"t","input_schema":{"type":"object","properties":{}}}]}"#,
            translated_request: b"{}",
        };
        let raw = br#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"t","args":{"value":7}}}]}}]}"#;
        let raw = if upstream == Format::Antigravity {
            [b"{\"response\":".as_slice(), raw, b"}"].concat()
        } else {
            raw.to_vec()
        };
        let output = (pair(Format::Claude, upstream).unwrap().non_stream)(&ctx, &raw).unwrap();
        assert_eq!(gj::get(&output, "content.0.input.value").int(), 7, "{upstream:?}");
    }
}

#[test]
fn schema_arguments_stream_defers_empty_input_until_continuation() {
    for upstream in [Format::Gemini, Format::Antigravity] {
        let ctx = ResponseCtx {
            model: "claude-test",
            original_request: br#"{"tools":[{"name":"t","input_schema":{"type":"object","properties":{}}}]}"#,
            translated_request: b"{}",
        };
        let mut stream = cpa_translate::go_stream(Format::Claude, upstream).unwrap()(&ctx);
        let mut output = Vec::new();
        for raw in [br#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"t","args":{}}}]}}]}"#.as_slice(), br#"{"candidates":[{"content":{"parts":[{"functionCall":{"args":{"value":7}}}]},"finishReason":"STOP"}],"usageMetadata":{}}"#.as_slice()] {
            let raw = if upstream == Format::Antigravity { [b"{\"response\":".as_slice(),raw,b"}"].concat() } else { raw.to_vec() };
            output.extend(stream.line(&raw).unwrap().concat());
        }
        output.extend(stream.line(b"[DONE]").unwrap().concat());
        let partials: Vec<_> = output
            .split(|b| *b == b'\n')
            .filter_map(|line| line.strip_prefix(b"data: "))
            .filter(|line| gj::get(line, "delta.type").str() == "input_json_delta")
            .map(|line| gj::get(line, "delta.partial_json").bytes().into_owned())
            .collect();
        assert_eq!(partials, vec![br#"{"value":7}"#.to_vec()], "{upstream:?}");
    }
}

#[test]
fn schema_arguments_stream_flushes_deferred_empty_at_end() {
    for upstream in [Format::Gemini, Format::Antigravity] {
        let ctx = ResponseCtx {
            model: "claude-test",
            original_request: br#"{"tools":[{"name":"t","input_schema":{"type":"object"}}]}"#,
            translated_request: b"{}",
        };
        let mut stream = cpa_translate::go_stream(Format::Claude, upstream).unwrap()(&ctx);
        let raw = br#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"t","args":{}}}]}}]}"#;
        let raw = if upstream == Format::Antigravity {
            [b"{\"response\":".as_slice(), raw, b"}"].concat()
        } else {
            raw.to_vec()
        };
        let first = stream.line(&raw).unwrap().concat();
        assert!(!first.windows(16).any(|w| w == b"input_json_delta"));
        let output = stream.line(b"[DONE]").unwrap().concat();
        let partials: Vec<_> = output
            .split(|b| *b == b'\n')
            .filter_map(|line| line.strip_prefix(b"data: "))
            .filter(|line| gj::get(line, "delta.type").str() == "input_json_delta")
            .map(|line| gj::get(line, "delta.partial_json").bytes().into_owned())
            .collect();
        assert_eq!(partials, vec![b"{}".to_vec()], "{upstream:?}");
    }
}
