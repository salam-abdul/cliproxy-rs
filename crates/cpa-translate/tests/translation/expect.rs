//! What each scenario's outputs must still say after translation, and the known cases
//! where a pair cannot say it.

use serde_json::Value;

use crate::facts::{Facts, Kind, Stop};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fact {
    Text,
    Thinking,
    ToolCall,
    ToolResult,
    Image,
    Usage,
    Stop,
}

#[derive(Default)]
pub struct Expected {
    pub text: Vec<&'static str>,
    pub thinking: Vec<&'static str>,
    pub tool_calls: Vec<(&'static str, Value)>,
    pub tool_results: Vec<&'static str>,
    pub images: usize,
    pub usage: Option<(i64, i64)>,
    pub stop: Option<Stop>,
}

/// The scenario's facts for an output of `kind`.
pub fn expected(scenario: &str, kind: Kind) -> Expected {
    let reply = |text: Vec<&'static str>, stop| Expected {
        text,
        usage: Some((12, 3)),
        stop: Some(stop),
        ..Expected::default()
    };
    match (scenario, kind) {
        ("text", Kind::Request) => Expected {
            text: vec!["You are terse.", "Say hello."],
            ..Expected::default()
        },
        ("text", _) => reply(vec!["Hello there."], Stop::End),
        _ => panic!("no expectations for scenario {scenario}"),
    }
}

/// A fact a pair's output does not carry, and why: (pair directory, scenario, output
/// kind, fact, reason). `executor` means the executor adds the fact after translation.
/// An entry whose fact the output does carry fails, so this list cannot go stale.
pub const LOSSY: &[(&str, &str, Kind, Fact, &str)] = &[];

/// Each fact `expected` names that `actual` lacks, with a description.
pub fn missing(expected: &Expected, actual: &Facts) -> Vec<(Fact, String)> {
    let mut out = vec![];
    for t in &expected.text {
        if !actual.text.contains(t) {
            out.push((Fact::Text, format!("text {t:?} not in {:?}", actual.text)));
        }
    }
    for t in &expected.thinking {
        if !actual.thinking.contains(t) {
            out.push((Fact::Thinking, format!("thinking {t:?} not in {:?}", actual.thinking)));
        }
    }
    for (name, args) in &expected.tool_calls {
        if !actual.tool_calls.iter().any(|(n, a)| n == name && a == args) {
            out.push((
                Fact::ToolCall,
                format!("tool call {name} {args} not in {:?}", actual.tool_calls),
            ));
        }
    }
    for t in &expected.tool_results {
        if !actual.tool_results.iter().any(|r| r.contains(t)) {
            out.push((
                Fact::ToolResult,
                format!("tool result {t:?} not in {:?}", actual.tool_results),
            ));
        }
    }
    if actual.images < expected.images {
        out.push((
            Fact::Image,
            format!("{} images, want {}", actual.images, expected.images),
        ));
    }
    if expected.usage.is_some() && actual.usage != expected.usage {
        out.push((
            Fact::Usage,
            format!("usage {:?}, want {:?}", actual.usage, expected.usage),
        ));
    }
    if expected.stop.is_some() && actual.stop != expected.stop {
        out.push((Fact::Stop, format!("stop {:?}, want {:?}", actual.stop, expected.stop)));
    }
    out
}

/// Content-check failures for one output: missing facts not in [`LOSSY`], and
/// [`LOSSY`] entries for facts that are present.
pub fn check(dir: &str, scenario: &str, kind: Kind, actual: &Facts) -> Vec<String> {
    let missing = missing(&expected(scenario, kind), actual);
    let lossy: Vec<_> = LOSSY
        .iter()
        .filter(|(d, s, k, _, _)| *d == dir && *s == scenario && *k == kind)
        .collect();
    let mut out = vec![];
    for (fact, why) in &missing {
        if !lossy.iter().any(|(_, _, _, f, _)| f == fact) {
            out.push(format!("{dir}/{scenario} {kind:?}: {why}"));
        }
    }
    for (_, _, _, fact, reason) in lossy {
        if !missing.iter().any(|(f, _)| f == fact) {
            out.push(format!(
                "{dir}/{scenario} {kind:?}: LOSSY lists {fact:?} ({reason}), but the output carries it"
            ));
        }
    }
    out
}
