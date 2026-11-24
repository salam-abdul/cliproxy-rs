//! What each executor hands a translator. Fixtures hold the upstream's wire form (an SSE
//! stream, a JSON body); these functions reshape them the way the executor driving that
//! upstream does before translating, so a golden records translator output for input the
//! translator really sees.

use cpa_core::format::Format;
use cpa_translate::stream::StreamOptions;

/// The model family a request is routed with. Only Antigravity serves both, and its
/// translators branch on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family {
    Default,
    Claude,
}

impl Family {
    pub fn for_upstream(upstream: Format) -> &'static [Family] {
        if upstream == Format::Antigravity {
            &[Family::Default, Family::Claude]
        } else {
            &[Family::Default]
        }
    }

    pub fn suffix(self) -> &'static str {
        match self {
            Family::Default => "",
            Family::Claude => "__claude",
        }
    }
}

/// A catalog model for the upstream, so translators that look a model up (thinking
/// budgets, Claude-over-Antigravity handling) take their real paths.
pub fn model_for(upstream: Format, family: Family) -> &'static str {
    match (upstream, family) {
        (Format::Antigravity, Family::Claude) | (Format::Claude, _) => "claude-sonnet-4-6",
        (Format::Antigravity, Family::Default) => "gemini-3-flash",
        (Format::Gemini | Format::Interactions, _) => "gemini-2.5-pro",
        (Format::Codex, _) => "gpt-5.5",
        (Format::OpenAI | Format::OpenAIResponse, _) => "kimi-k2.5",
    }
}

/// Blank-line separated SSE events, each without its trailing blank line.
fn sse_events(sse: &[u8]) -> Vec<Vec<u8>> {
    let text = String::from_utf8_lossy(sse).replace("\r\n", "\n");
    text.split("\n\n")
        .map(str::trim_end)
        .filter(|e| !e.is_empty())
        .map(|e| e.as_bytes().to_vec())
        .collect()
}

/// The `data:` payload of an SSE event.
fn data(event: &[u8]) -> Option<Vec<u8>> {
    let text = String::from_utf8_lossy(event);
    let lines: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(str::trim)
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n").into_bytes())
}

/// How the stream is driven: the options, the events, and whether the executor feeds a
/// synthetic `[DONE]` after finalizing tool input.
pub struct StreamDelivery {
    pub options: StreamOptions,
    pub events: Vec<Vec<u8>>,
    pub done: bool,
    /// Whether the executor finalizes tool input when the upstream ends; the Codex
    /// executor's `translate_stream` does not.
    pub finalize: bool,
}

/// The events an executor passes to the stream translator for `sse`.
///
/// - Claude (`cpa-exec/src/translate.rs`) and Codex (`codex.rs` `translate_stream`): each
///   SSE event as it arrives.
/// - OpenAI chat (`openai_compat.rs`) and Responses (`kimi.rs`): each line, with a blank
///   line appended.
/// - Interactions (`gemini.rs` `InteractionsLines`): each scanned line, blank ones
///   included, as a `\r\n`-terminated event.
/// - Gemini (`gemini.rs` `GeminiLines`): each line's bare JSON payload, then `[DONE]`. The
///   executor also drops usage from chunks before the last; fixtures carry usage only there.
/// - Antigravity (`antigravity.rs`, through `gemini_lines`): each whole payload with
///   `whole_events`, then `[DONE]`.
pub fn stream(upstream: Format, sse: &[u8]) -> StreamDelivery {
    let events = sse_events(sse);
    let mut options = StreamOptions::default();
    let (events, done) = match upstream {
        Format::Claude | Format::Codex => (
            events
                .into_iter()
                .map(|mut e| {
                    e.extend_from_slice(b"\n\n");
                    e
                })
                .collect(),
            false,
        ),
        Format::OpenAI | Format::OpenAIResponse => {
            let text = String::from_utf8_lossy(sse).replace("\r\n", "\n");
            (
                text.lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| [l.as_bytes(), b"\n\n"].concat())
                    .collect(),
                false,
            )
        }
        Format::Interactions => {
            let text = String::from_utf8_lossy(sse).replace("\r\n", "\n");
            (text.lines().map(|l| [l.as_bytes(), b"\r\n"].concat()).collect(), false)
        }
        Format::Gemini | Format::Antigravity => {
            if upstream == Format::Antigravity {
                options.whole_events = true;
            }
            (
                events
                    .iter()
                    .filter_map(|e| data(e))
                    .filter(|d| d.as_slice() != b"[DONE]")
                    .collect(),
                true,
            )
        }
    };
    StreamDelivery {
        options,
        events,
        done,
        finalize: upstream != Format::Codex,
    }
}

/// The body an executor passes to the non-stream translator.
///
/// - Claude (`translate.rs`): the whole buffered SSE stream.
/// - Codex (`codex.rs` `non_stream_output`): the terminal `response.completed` or
///   `response.incomplete` event's payload.
/// - Others: the upstream's JSON body.
pub fn non_stream(upstream: Format, sse: &[u8], json: Option<&[u8]>) -> Option<Vec<u8>> {
    match upstream {
        Format::Claude => Some(sse.to_vec()),
        Format::Codex => sse_events(sse).iter().filter_map(|e| data(e)).find(|d| {
            let v: serde_json::Value = serde_json::from_slice(d).unwrap_or_default();
            matches!(v["type"].as_str(), Some("response.completed" | "response.incomplete"))
        }),
        _ => json.map(<[u8]>::to_vec),
    }
}
