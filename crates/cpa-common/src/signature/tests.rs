//! Signature size and media passthrough regressions.
use super::*;

#[test]
fn length_caps_reject_before_decoding() {
    let huge = format!("E{}", "A".repeat(MAX_CLAUDE_THINKING_SIGNATURE_LEN));
    assert_eq!(
        normalize_claude_thinking_signature(&huge, ClaudeValidation::default())
            .unwrap_err()
            .0,
        "signature exceeds maximum length (33554432 bytes)"
    );
    assert_eq!(
        inspect_gemini_thought_signature(&huge, GeminiValidation::default())
            .unwrap_err()
            .0,
        "Gemini thought signature exceeds maximum length (33554432 bytes)"
    );
    let exact = "A".repeat(MAX_CLAUDE_THINKING_SIGNATURE_LEN - 4) + "AAA=";
    assert!(inspect_gemini_thought_signature(&exact, GeminiValidation::default()).is_ok());
}

/// Large inline media passes through unchanged.
#[test]
fn large_inline_data_passes_through_unchanged() {
    let cases = [
        (
            r#"{"project":"","request":{"contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"image/png","data":""#,
            r#""}},{"text":"describe"}]}]},"model":"gemini-3-flash"}"#,
            4_194_464,
            "request.contents",
        ),
        (
            r#"{"contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"video/mp4","data":""#,
            r#""}}]}],"safetySettings":[]}"#,
            20_971_630,
            "contents",
        ),
    ];
    for (prefix, suffix, total, path) in cases {
        let body = format!("{prefix}{}{suffix}", "A".repeat(total - prefix.len() - suffix.len()));
        assert_eq!(body.len(), total);
        assert_eq!(
            sanitize_gemini_request_thought_signatures(body.as_bytes(), path),
            body.as_bytes()
        );
    }
}
