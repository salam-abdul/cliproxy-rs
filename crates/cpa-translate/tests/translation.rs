//! Golden tests over every registered translator pair. Each scenario is a synthetic turn
//! written once per format under `fixtures/translation/input/<format>/`: a client request
//! (`<scenario>.request.json`) and an upstream reply as a stream (`<scenario>.stream.sse`)
//! and, where the executor reads a JSON body, as one (`<scenario>.response.json`). Every
//! pair translates its client format's request and its upstream format's replies, and the
//! normalized outputs are compared with `fixtures/translation/golden/<client>__<upstream>/`.
//!
//! `UPDATE_GOLDEN=1` rewrites the goldens. A golden changes only in the commit that
//! changes that behavior, with the reason in the commit message.

#[path = "translation/deliver.rs"]
mod deliver;
#[path = "translation/expect.rs"]
mod expect;
#[path = "translation/facts.rs"]
mod facts;
#[path = "translation/golden.rs"]
mod golden;
#[path = "translation/normalize.rs"]
mod normalize;

use cpa_core::format::Format;
use cpa_translate::{
    RequestCtx, ResponseCtx, pair, stream_with, token_count, translate_request, translate_token_count,
};
use deliver::Family;
use facts::{Kind, facts};
use golden::Goldens;
use normalize::normalize;

/// Scenario names, in the order they run.
const SCENARIOS: &[&str] = &[
    "text",
    "tool_call",
    "tool_result",
    "thinking",
    "image",
    "multi_turn",
    "max_tokens",
    "stream_error",
];

/// Inputs a format cannot express: (format, scenario, file kind, why). Any other missing
/// input fails, so a scenario cannot quietly skip a format.
const NO_ERROR_EVENT: &str =
    "its streams carry no error event: a failed reply arrives as an HTTP error, which the executor handles";
const FAILED_BODY: &str = "a failed reply arrives as an HTTP error, which the executor handles before translation";

const NOT_EXPRESSIBLE: &[(Format, &str, &str, &str)] = &[
    (Format::OpenAI, "stream_error", "stream.sse", NO_ERROR_EVENT),
    (Format::Gemini, "stream_error", "stream.sse", NO_ERROR_EVENT),
    (Format::Antigravity, "stream_error", "stream.sse", NO_ERROR_EVENT),
    (Format::OpenAI, "stream_error", "response.json", FAILED_BODY),
    (Format::Gemini, "stream_error", "response.json", FAILED_BODY),
    (Format::Antigravity, "stream_error", "response.json", FAILED_BODY),
    (Format::OpenAIResponse, "stream_error", "response.json", FAILED_BODY),
    (Format::Interactions, "stream_error", "response.json", FAILED_BODY),
];

/// Every input a pair would read exists, or is listed in [`NOT_EXPRESSIBLE`].
fn missing_inputs() -> Vec<String> {
    let mut failures = vec![];
    for scenario in SCENARIOS {
        for format in sorted_formats() {
            let client = sorted_formats().iter().any(|&u| pair(format, u).is_some());
            let upstream = sorted_formats().iter().any(|&c| pair(c, format).is_some());
            let mut kinds = vec![];
            if client {
                kinds.push("request.json");
            }
            if upstream {
                kinds.push("stream.sse");
                if !matches!(format, Format::Claude | Format::Codex) {
                    kinds.push("response.json");
                }
            }
            for kind in kinds {
                let listed = NOT_EXPRESSIBLE
                    .iter()
                    .any(|(f, s, k, _)| *f == format && s == scenario && *k == kind);
                let file = format!("{scenario}.{kind}");
                match (input(format, &file).is_some(), listed) {
                    (false, false) => failures.push(format!("input/{}/{file}: missing", format.as_str())),
                    (true, true) => failures.push(format!(
                        "input/{}/{file}: exists but listed as not expressible",
                        format.as_str()
                    )),
                    _ => {}
                }
            }
        }
    }
    failures
}

fn sorted_formats() -> Vec<Format> {
    let mut all = Format::ALL.to_vec();
    all.sort_by_key(|f| f.as_str());
    all
}

fn input(format: Format, file: &str) -> Option<Vec<u8>> {
    std::fs::read(golden::root().join("input").join(format.as_str()).join(file)).ok()
}

fn pair_dir(client: Format, upstream: Format, family: Family) -> String {
    format!("{}__{}{}", client.as_str(), upstream.as_str(), family.suffix())
}

/// One translation, run twice: a normalized output that differs between runs holds a
/// generated value the normalizer does not know.
fn twice(label: &str, failures: &mut Vec<String>, f: impl Fn() -> Vec<u8>) -> Vec<u8> {
    let first = normalize(&f());
    let second = normalize(&f());
    if first != second {
        failures.push(format!(
            "{label}: output changes between runs; add its generated value to normalize.rs\n--- first\n{}\n--- second\n{}",
            String::from_utf8_lossy(&first),
            String::from_utf8_lossy(&second)
        ));
    }
    if first.is_empty() {
        failures.push(format!("{label}: empty output"));
    }
    first
}

/// The client's request as it is sent for a streamed reply: formats that ask for a stream
/// in the body carry `"stream":true`; Gemini asks in the URL.
fn with_stream_flag(client: Format, request: &[u8]) -> Vec<u8> {
    if client == Format::Gemini {
        return request.to_vec();
    }
    let mut value: serde_json::Value = serde_json::from_slice(request).expect("fixture request is JSON");
    value["stream"] = serde_json::Value::Bool(true);
    serde_json::to_vec(&value).unwrap()
}

fn shown(result: Result<Vec<u8>, cpa_translate::Error>) -> Vec<u8> {
    result.unwrap_or_else(|e| format!("<error: {e}>").into_bytes())
}

/// The stream translator driven the way the upstream's executor drives it.
fn run_stream(client: Format, upstream: Format, ctx: &ResponseCtx<'_>, sse: &[u8]) -> Vec<u8> {
    let delivery = deliver::stream(upstream, sse);
    let mut t = stream_with(client, upstream, ctx, delivery.options).expect("registered pair");
    let mut out = Vec::new();
    let push = |frames: Result<Vec<bytes::Bytes>, cpa_translate::Error>, out: &mut Vec<u8>| match frames {
        Ok(frames) => frames.iter().for_each(|f| out.extend_from_slice(f)),
        Err(e) => out.extend_from_slice(format!("<error: {e}>\n").as_bytes()),
    };
    for event in &delivery.events {
        push(t.event(event), &mut out);
    }
    push(Ok(t.finalize_tool_input()), &mut out);
    if delivery.done {
        push(t.event(b"[DONE]"), &mut out);
    }
    push(t.finish(), &mut out);
    out
}

#[test]
fn translations_match_their_goldens() {
    cpa_translate::set_antigravity_signature_cache_config(false, false);
    let mut goldens = Goldens::new();
    let mut failures = missing_inputs();
    for client in sorted_formats() {
        for upstream in sorted_formats() {
            let Some(p) = pair(client, upstream) else { continue };
            for &family in Family::for_upstream(upstream) {
                let dir = pair_dir(client, upstream, family);
                let model = deliver::model_for(upstream, family);
                if token_count(client, upstream).is_some() {
                    let out = translate_token_count(client, upstream, 42, b"{}");
                    goldens.check(&format!("{dir}/token_count.json"), &normalize(&out));
                }
                for scenario in SCENARIOS {
                    let Some(request) = input(client, &format!("{scenario}.request.json")) else {
                        continue;
                    };
                    let label = format!("{dir}/{scenario}");
                    let streamed_request = with_stream_flag(client, &request);
                    let translate = |stream: bool| {
                        let body = if stream { &streamed_request } else { &request };
                        shown(translate_request(client, upstream, &RequestCtx { model, stream }, body))
                    };
                    let buffered = twice(&format!("{label}.request"), &mut failures, || translate(false));
                    let streamed = twice(&format!("{label}.request.stream"), &mut failures, || translate(true));
                    goldens.check(&format!("{label}.request.json"), &buffered);
                    for out in [&buffered, &streamed] {
                        failures.extend(expect::check(
                            &dir,
                            scenario,
                            Kind::Request,
                            &facts(upstream, Kind::Request, out),
                        ));
                    }
                    if streamed != buffered {
                        goldens.check(&format!("{label}.request.stream.json"), &streamed);
                    }
                    let sse = input(upstream, &format!("{scenario}.stream.sse"));
                    let json = input(upstream, &format!("{scenario}.response.json"));
                    let raw_buffered = translate(false);
                    let raw_streamed = translate(true);
                    if let Some(body) = sse
                        .as_deref()
                        .and_then(|sse| deliver::non_stream(upstream, sse, json.as_deref()))
                        .or_else(|| json.clone())
                    {
                        let ctx = ResponseCtx {
                            model,
                            original_request: &request,
                            translated_request: &raw_buffered,
                            portable: false,
                        };
                        let out = twice(&format!("{label}.response"), &mut failures, || {
                            shown((p.non_stream)(&ctx, &body))
                        });
                        goldens.check(&format!("{label}.response.json"), &out);
                        failures.extend(expect::check(
                            &dir,
                            scenario,
                            Kind::Response,
                            &facts(client, Kind::Response, &out),
                        ));
                    }
                    if let Some(sse) = &sse {
                        let ctx = ResponseCtx {
                            model,
                            original_request: &streamed_request,
                            translated_request: &raw_streamed,
                            portable: false,
                        };
                        let out = twice(&format!("{label}.stream"), &mut failures, || {
                            run_stream(client, upstream, &ctx, sse)
                        });
                        goldens.check(&format!("{label}.stream.sse"), &out);
                        failures.extend(expect::check(
                            &dir,
                            scenario,
                            Kind::Stream,
                            &facts(client, Kind::Stream, &out),
                        ));
                    }
                }
            }
        }
    }
    goldens.orphans();
    failures.extend(goldens.failures);
    assert!(
        failures.is_empty(),
        "{} failures:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn every_registered_pair_has_goldens_and_no_other_pair_does() {
    let mut failures = vec![];
    for client in sorted_formats() {
        for upstream in sorted_formats() {
            let registered = pair(client, upstream).is_some();
            for &family in Family::for_upstream(upstream) {
                let dir = golden::root().join("golden").join(pair_dir(client, upstream, family));
                if registered != dir.is_dir() {
                    failures.push(format!(
                        "{}: registered {registered}, goldens {}",
                        pair_dir(client, upstream, family),
                        dir.is_dir()
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn ctx<'a>(original: &'a [u8]) -> ResponseCtx<'a> {
    ResponseCtx {
        model: "m",
        original_request: original,
        translated_request: b"{}",
        portable: false,
    }
}

/// Claude's other token-limit stop reason reads as a limit for every client, as the
/// Responses translator already treats it.
#[test]
fn a_claude_context_window_stop_is_a_limit_for_gemini_and_interactions_clients() {
    let body = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"c\",\"usage\":{\"input_tokens\":1}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"model_context_window_exceeded\"},\"usage\":{\"output_tokens\":1}}\n\n";
    let gemini = (pair(Format::Gemini, Format::Claude).unwrap().non_stream)(&ctx(b"{}"), body).unwrap();
    let gemini: serde_json::Value = serde_json::from_slice(&gemini).unwrap();
    assert_eq!(gemini["candidates"][0]["finishReason"], "MAX_TOKENS");
    let interactions = (pair(Format::Interactions, Format::Claude).unwrap().non_stream)(&ctx(b"{}"), body).unwrap();
    let interactions: serde_json::Value = serde_json::from_slice(&interactions).unwrap();
    assert_eq!(interactions["status"], "incomplete");
}

/// Arguments streamed for two calls are not sent again when their items finish, whatever
/// order the items finish in.
#[test]
fn codex_arguments_streamed_for_two_calls_reach_interactions_clients_once() {
    let events: Vec<&[u8]> = vec![
        b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\",\"model\":\"m\"}}\n\n",
        b"data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_a\",\"type\":\"function_call\",\"call_id\":\"a\",\"name\":\"f\"}}\n\n",
        b"data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_a\",\"delta\":\"{\\\"n\\\":1}\"}\n\n",
        b"data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_b\",\"type\":\"function_call\",\"call_id\":\"b\",\"name\":\"g\"}}\n\n",
        b"data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_b\",\"delta\":\"{\\\"n\\\":2}\"}\n\n",
        b"data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"fc_a\",\"type\":\"function_call\",\"call_id\":\"a\",\"name\":\"f\",\"arguments\":\"{\\\"n\\\":1}\"}}\n\n",
        b"data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"fc_b\",\"type\":\"function_call\",\"call_id\":\"b\",\"name\":\"g\",\"arguments\":\"{\\\"n\\\":2}\"}}\n\n",
    ];
    let request = br#"{"stream":true}"#;
    let mut t = stream_with(Format::Interactions, Format::Codex, &ctx(request), Default::default()).unwrap();
    let mut out = Vec::new();
    for event in events {
        t.event(event).unwrap().iter().for_each(|f| out.extend_from_slice(f));
    }
    let out = String::from_utf8(out).unwrap();
    assert_eq!(out.matches(r#"\"n\":1"#).count(), 1, "{out}");
    assert_eq!(out.matches(r#"\"n\":2"#).count(), 1, "{out}");
}

#[test]
fn an_interactions_output_limit_in_camel_case_reaches_responses() {
    let body = br#"{"input":"hi","generation_config":{"maxOutputTokens":7}}"#;
    let out = translate_request(
        Format::Interactions,
        Format::OpenAIResponse,
        &RequestCtx {
            model: "m",
            stream: false,
        },
        body,
    )
    .unwrap();
    let out: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(out["max_output_tokens"], 7);
}
