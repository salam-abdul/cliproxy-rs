//! Reasoning configuration and request regressions.
use super::*;
use serde_json::Value;

fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

#[test]
fn kimi_thread_request_vectors() {
    let vectors: Vec<Value> = serde_json::from_str(include_str!(
        "../../../cpa-exec/tests/device_fixtures/kimi/vectors.json"
    ))
    .unwrap();
    let mut count = 0;
    for v in vectors.iter().filter(|v| v["fn"] == "thinking") {
        let input = s(&v["in"]).as_bytes();
        let (from, to) = (s(&v["from"]), s(&v["to"]));
        // Go's registry: only the Codex target registers translators from these formats.
        let has_request_transformer = to == "codex" && matches!(from, "openai" | "openai-response");
        let got = apply_request_thinking(&RequestThinking {
            body: input,
            payload: input,
            original: input,
            model: s(&v["model"]),
            from,
            to,
            provider: "kimi",
            resolved: None,
            has_request_transformer,
            updates_changed: false,
        });
        match v["err"].as_str() {
            Some(want) => assert_eq!(got.unwrap_err().message, want, "{v}"),
            None => assert_eq!(got.unwrap(), s(&v["out"]).as_bytes(), "{v}"),
        }
        count += 1;
    }
    assert_eq!(count, 16);
}

#[test]
fn suffix_and_conversion_edges() {
    assert_eq!(parse_suffix("a(b)(c)").model_name, "a(b)");
    assert!(!parse_suffix("a(b").has_suffix);
    assert_eq!(parse_numeric_suffix("08192"), Some(8192));
    assert_eq!(parse_numeric_suffix("+5"), Some(5), "strconv.Atoi accepts a plus sign");
    assert_eq!(parse_numeric_suffix("-1"), None);
    assert_eq!(parse_numeric_suffix("9223372036854775808"), None);
    assert_eq!(convert_budget_to_level(-2), None);
    assert_eq!(convert_budget_to_level(512), Some(LEVEL_MINIMAL));
    assert_eq!(convert_budget_to_level(513), Some(LEVEL_LOW));
    assert_eq!(convert_budget_to_level(24577), Some(LEVEL_XHIGH));
    assert_eq!(map_to_claude_effort("xhigh", false), Some("high"));
    assert_eq!(map_to_claude_effort("", true), None);
}

/// `reasoning_effort: "none"` (thinking off) on models that refuse
/// `thinking: {"type": "disabled"}` becomes the lowest effort; other models keep
/// Go's `disabled`.
#[test]
fn thinking_off_uses_the_lowest_effort_where_disabled_is_refused() {
    let off = |model: &str, body: &str| {
        let out = apply_thinking(body.as_bytes(), model, "claude", "claude", "claude").unwrap();
        let out: Value = serde_json::from_slice(&out).unwrap();
        (out["thinking"].clone(), out["output_config"].clone())
    };
    let disabled = |model: &str| {
        format!(r#"{{"model":"{model}","max_tokens":64,"thinking":{{"type":"disabled"}},"messages":[]}}"#)
    };
    for model in ["claude-opus-5-5", "claude-fable-5-1", "claude-sonnet-5-5"] {
        assert_eq!(
            off(model, &disabled(model)),
            (
                serde_json::json!({"type": "adaptive"}),
                serde_json::json!({"effort": "low"})
            ),
            "{model}"
        );
    }
    // An OpenAI Chat `reasoning_effort: "none"`, as the executor applies it: the
    // translated body carries `disabled`, the source carries the effort.
    let source = br#"{"model":"claude-opus-5-5","reasoning_effort":"none","messages":[]}"#;
    let out = apply_thinking_with_source_and_summary(
        disabled("claude-opus-5-5").as_bytes(),
        source,
        "claude-opus-5-5",
        "openai",
        "claude",
        "claude",
        SummaryConfig::default(),
        false,
    )
    .unwrap();
    let out: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(out["thinking"], serde_json::json!({"type": "adaptive"}), "{out}");
    assert_eq!(out["output_config"], serde_json::json!({"effort": "low"}), "{out}");
    for model in ["claude-opus-4-8", "claude-sonnet-5"] {
        let (thinking, output) = off(model, &disabled(model));
        assert_eq!(thinking, serde_json::json!({"type": "disabled"}), "{model}");
        assert!(output.is_null(), "{model}");
    }
}
