//! What a translated body says, read per format: its text, thinking, tool calls, tool
//! results, usage and stop reason. The content check compares these with the scenario's
//! expectations, so a golden cannot record a conversion that lost them.

use std::collections::BTreeMap;

use cpa_core::format::Format;
use serde_json::Value;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Request,
    Response,
    Stream,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stop {
    End,
    ToolUse,
    Length,
    Error,
}

#[derive(Default, Debug)]
pub struct Facts {
    /// Every text, concatenated in order.
    pub text: String,
    pub thinking: String,
    /// Name and parsed arguments.
    pub tool_calls: Vec<(String, Value)>,
    /// Result content as text.
    pub tool_results: Vec<String>,
    /// Names of the tools a request declares.
    pub tool_defs: Vec<String>,
    /// Whether a request asks for reasoning.
    pub reasoning: bool,
    pub images: usize,
    /// Input and output tokens, the largest seen.
    pub usage: Option<(i64, i64)>,
    pub stop: Option<Stop>,
}

impl Facts {
    fn usage(&mut self, input: Option<i64>, output: Option<i64>) {
        if input.is_none() && output.is_none() {
            return;
        }
        let (i, o) = self.usage.unwrap_or((0, 0));
        self.usage = Some((i.max(input.unwrap_or(0)), o.max(output.unwrap_or(0))));
    }

    fn stop(&mut self, stop: Option<Stop>) {
        // A tool call or an error outranks a plain end reported alongside it.
        if let Some(stop) = stop
            && !(stop == Stop::End && matches!(self.stop, Some(Stop::ToolUse | Stop::Error)))
        {
            self.stop = Some(stop);
        }
    }
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn int(v: &Value) -> Option<i64> {
    v.as_i64()
}

fn args(v: &Value) -> Value {
    match v {
        Value::String(text) => serde_json::from_str(text).unwrap_or(Value::String(text.clone())),
        Value::Null => Value::Object(Default::default()),
        other => other.clone(),
    }
}

fn result_text(v: &Value) -> String {
    match v {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|i| {
                i.get("text")
                    .and_then(Value::as_str)
                    .map_or_else(|| result_text(i), str::to_string)
            })
            .collect(),
        other => other.to_string(),
    }
}

/// The JSON payloads of a body: each SSE `data:` payload, or the body itself.
fn payloads(body: &[u8]) -> Vec<Value> {
    let text = String::from_utf8_lossy(body);
    if let Ok(v) = serde_json::from_str::<Value>(&text) {
        return vec![v];
    }
    text.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(str::trim)
        .filter(|d| !d.is_empty() && *d != "[DONE]")
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect()
}

pub fn facts(format: Format, kind: Kind, body: &[u8]) -> Facts {
    let mut f = Facts::default();
    let payloads = payloads(body);
    match (format, kind) {
        (Format::Claude, Kind::Request) => payloads.iter().for_each(|p| claude_request(&mut f, p)),
        (Format::Claude, _) => claude_response(&mut f, &payloads),
        (Format::OpenAI, Kind::Request) => payloads.iter().for_each(|p| chat_request(&mut f, p)),
        (Format::OpenAI, _) => chat_response(&mut f, &payloads),
        (Format::OpenAIResponse | Format::Codex, Kind::Request) => {
            payloads.iter().for_each(|p| responses_request(&mut f, p))
        }
        (Format::OpenAIResponse | Format::Codex, _) => responses_response(&mut f, &payloads),
        (Format::Gemini | Format::Antigravity, Kind::Request) => payloads.iter().for_each(|p| {
            let p = if format == Format::Antigravity {
                &p["request"]
            } else {
                p
            };
            gemini_request(&mut f, p)
        }),
        (Format::Gemini | Format::Antigravity, _) => payloads.iter().for_each(|p| {
            let p = if p.get("response").is_some() { &p["response"] } else { p };
            gemini_response(&mut f, p)
        }),
        (Format::Interactions, Kind::Request) => payloads.iter().for_each(|p| interactions_request(&mut f, p)),
        (Format::Interactions, _) => interactions_response(&mut f, &payloads),
    }
    f
}

// Claude

fn claude_blocks(f: &mut Facts, content: &Value) {
    if let Some(text) = content.as_str() {
        f.text.push_str(text);
        return;
    }
    for block in content.as_array().into_iter().flatten() {
        match s(&block["type"]) {
            "text" => f.text.push_str(s(&block["text"])),
            "thinking" => f.thinking.push_str(s(&block["thinking"])),
            "tool_use" => f.tool_calls.push((s(&block["name"]).into(), args(&block["input"]))),
            "tool_result" => f.tool_results.push(result_text(&block["content"])),
            "image" => f.images += 1,
            _ => {}
        }
    }
}

fn claude_stop(reason: &str) -> Option<Stop> {
    match reason {
        "end_turn" | "stop_sequence" => Some(Stop::End),
        "tool_use" => Some(Stop::ToolUse),
        "max_tokens" => Some(Stop::Length),
        _ => None,
    }
}

fn claude_usage(f: &mut Facts, u: &Value) {
    f.usage(int(&u["input_tokens"]), int(&u["output_tokens"]));
}

/// Each declared tool's name, at `path` under every item of `tools`.
fn tool_defs(f: &mut Facts, tools: &Value, path: &[&str]) {
    for tool in tools.as_array().into_iter().flatten() {
        let name = path.iter().fold(tool, |v, key| &v[*key]);
        if let Some(name) = name.as_str() {
            f.tool_defs.push(name.to_string());
        }
    }
}

/// A reasoning setting that asks for reasoning: present and not off.
fn on(v: &Value) -> bool {
    match v {
        Value::String(s) => !s.is_empty() && !matches!(s.as_str(), "none" | "disabled" | "off"),
        Value::Number(n) => n.as_i64() != Some(0),
        Value::Bool(b) => *b,
        _ => false,
    }
}

fn claude_request(f: &mut Facts, p: &Value) {
    tool_defs(f, &p["tools"], &["name"]);
    f.reasoning |= on(&p["thinking"]["type"]) || on(&p["output_config"]["effort"]);
    claude_blocks(f, &p["system"]);
    for m in p["messages"].as_array().into_iter().flatten() {
        claude_blocks(f, &m["content"]);
    }
}

fn claude_response(f: &mut Facts, payloads: &[Value]) {
    let mut tools: BTreeMap<i64, (String, String)> = BTreeMap::new();
    for p in payloads {
        match s(&p["type"]) {
            "message" => {
                claude_blocks(f, &p["content"]);
                f.stop(claude_stop(s(&p["stop_reason"])));
                claude_usage(f, &p["usage"]);
            }
            "message_start" => claude_usage(f, &p["message"]["usage"]),
            "content_block_start" => {
                let block = &p["content_block"];
                match s(&block["type"]) {
                    "tool_use" => {
                        tools.insert(int(&p["index"]).unwrap_or(0), (s(&block["name"]).into(), String::new()));
                    }
                    _ => claude_blocks(f, &Value::Array(vec![block.clone()])),
                }
            }
            "content_block_delta" => {
                let d = &p["delta"];
                match s(&d["type"]) {
                    "text_delta" => f.text.push_str(s(&d["text"])),
                    "thinking_delta" => f.thinking.push_str(s(&d["thinking"])),
                    "input_json_delta" => {
                        if let Some(t) = tools.get_mut(&int(&p["index"]).unwrap_or(0)) {
                            t.1.push_str(s(&d["partial_json"]));
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                f.stop(claude_stop(s(&p["delta"]["stop_reason"])));
                claude_usage(f, &p["usage"]);
            }
            "error" => f.stop(Some(Stop::Error)),
            _ => {}
        }
    }
    for (name, json) in tools.into_values() {
        f.tool_calls.push((name, args(&Value::String(json))));
    }
}

// OpenAI chat completions

fn chat_content(f: &mut Facts, content: &Value) {
    if let Some(text) = content.as_str() {
        f.text.push_str(text);
        return;
    }
    for part in content.as_array().into_iter().flatten() {
        match s(&part["type"]) {
            "text" => f.text.push_str(s(&part["text"])),
            "image_url" => f.images += 1,
            _ => {}
        }
    }
}

fn chat_stop(reason: &str) -> Option<Stop> {
    match reason {
        "stop" => Some(Stop::End),
        "tool_calls" | "function_call" => Some(Stop::ToolUse),
        "length" => Some(Stop::Length),
        "error" => Some(Stop::Error),
        _ => None,
    }
}

fn chat_usage(f: &mut Facts, u: &Value) {
    f.usage(int(&u["prompt_tokens"]), int(&u["completion_tokens"]));
}

fn chat_request(f: &mut Facts, p: &Value) {
    tool_defs(f, &p["tools"], &["function", "name"]);
    f.reasoning |= on(&p["reasoning_effort"]);
    for m in p["messages"].as_array().into_iter().flatten() {
        if s(&m["role"]) == "tool" {
            f.tool_results.push(result_text(&m["content"]));
            continue;
        }
        chat_content(f, &m["content"]);
        f.thinking.push_str(s(&m["reasoning_content"]));
        for call in m["tool_calls"].as_array().into_iter().flatten() {
            f.tool_calls.push((
                s(&call["function"]["name"]).into(),
                args(&call["function"]["arguments"]),
            ));
        }
    }
}

fn chat_response(f: &mut Facts, payloads: &[Value]) {
    let mut tools: BTreeMap<i64, (String, String)> = BTreeMap::new();
    for p in payloads {
        if p.get("error").is_some() {
            f.stop(Some(Stop::Error));
        }
        chat_usage(f, &p["usage"]);
        for choice in p["choices"].as_array().into_iter().flatten() {
            f.stop(chat_stop(s(&choice["finish_reason"])));
            if let Some(m) = choice.get("message") {
                chat_content(f, &m["content"]);
                f.thinking.push_str(s(&m["reasoning_content"]));
                for call in m["tool_calls"].as_array().into_iter().flatten() {
                    f.tool_calls.push((
                        s(&call["function"]["name"]).into(),
                        args(&call["function"]["arguments"]),
                    ));
                }
            }
            let d = &choice["delta"];
            chat_content(f, &d["content"]);
            f.thinking.push_str(s(&d["reasoning_content"]));
            for call in d["tool_calls"].as_array().into_iter().flatten() {
                let t = tools.entry(int(&call["index"]).unwrap_or(0)).or_default();
                t.0.push_str(s(&call["function"]["name"]));
                t.1.push_str(s(&call["function"]["arguments"]));
            }
        }
    }
    for (name, json) in tools.into_values() {
        f.tool_calls.push((name, args(&Value::String(json))));
    }
}

// OpenAI Responses and Codex

fn responses_item(f: &mut Facts, item: &Value) {
    match s(&item["type"]) {
        "message" => {
            if let Some(text) = item["content"].as_str() {
                f.text.push_str(text);
            }
            for part in item["content"].as_array().into_iter().flatten() {
                match s(&part["type"]) {
                    "input_text" | "output_text" | "text" => f.text.push_str(s(&part["text"])),
                    "input_image" => f.images += 1,
                    _ => {}
                }
            }
        }
        "reasoning" => {
            for part in item["summary"]
                .as_array()
                .into_iter()
                .chain(item["content"].as_array())
                .flatten()
            {
                f.thinking.push_str(s(&part["text"]));
            }
        }
        "function_call" => f.tool_calls.push((s(&item["name"]).into(), args(&item["arguments"]))),
        "function_call_output" => f.tool_results.push(result_text(&item["output"])),
        _ => {}
    }
}

fn responses_usage(f: &mut Facts, u: &Value) {
    f.usage(int(&u["input_tokens"]), int(&u["output_tokens"]));
}

fn responses_status(f: &mut Facts, r: &Value) {
    match s(&r["status"]) {
        "completed" => f.stop(Some(
            if r["output"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|i| s(&i["type"]) == "function_call")
            {
                Stop::ToolUse
            } else {
                Stop::End
            },
        )),
        "incomplete" => f.stop(Some(Stop::Length)),
        "failed" => f.stop(Some(Stop::Error)),
        _ => {}
    }
}

fn responses_request(f: &mut Facts, p: &Value) {
    tool_defs(f, &p["tools"], &["name"]);
    f.reasoning |= on(&p["reasoning"]["effort"]);
    f.text.push_str(s(&p["instructions"]));
    if let Some(text) = p["input"].as_str() {
        f.text.push_str(text);
    }
    for item in p["input"].as_array().into_iter().flatten() {
        if item.get("type").is_none() && item.get("role").is_some() {
            responses_item(f, &serde_json::json!({"type": "message", "content": item["content"]}));
        } else {
            responses_item(f, item);
        }
    }
}

fn responses_response(f: &mut Facts, payloads: &[Value]) {
    let streamed = payloads
        .iter()
        .any(|p| p.get("type").is_some_and(|t| s(t).starts_with("response.")));
    for p in payloads {
        if !streamed {
            for item in p["output"].as_array().into_iter().flatten() {
                responses_item(f, item);
            }
            responses_usage(f, &p["usage"]);
            responses_status(f, p);
            continue;
        }
        match s(&p["type"]) {
            "response.output_text.delta" => f.text.push_str(s(&p["delta"])),
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                f.thinking.push_str(s(&p["delta"]))
            }
            "response.output_item.done" if s(&p["item"]["type"]) == "function_call" => responses_item(f, &p["item"]),
            "response.completed" | "response.incomplete" | "response.failed" => {
                responses_usage(f, &p["response"]["usage"]);
                responses_status(f, &p["response"]);
            }
            "error" => f.stop(Some(Stop::Error)),
            _ => {}
        }
    }
}

// Gemini and Antigravity

fn gemini_parts(f: &mut Facts, parts: &Value) {
    for part in parts.as_array().into_iter().flatten() {
        if let Some(text) = part["text"].as_str() {
            if part["thought"].as_bool() == Some(true) {
                f.thinking.push_str(text);
            } else {
                f.text.push_str(text);
            }
        }
        if let Some(call) = part.get("functionCall") {
            f.tool_calls.push((s(&call["name"]).into(), args(&call["args"])));
        }
        if let Some(result) = part.get("functionResponse") {
            f.tool_results.push(result_text(&result["response"]));
        }
        // The Gemini API takes either spelling.
        if ["inlineData", "inline_data", "fileData", "file_data"]
            .iter()
            .any(|k| part.get(*k).is_some())
        {
            f.images += 1;
        }
    }
}

fn gemini_request(f: &mut Facts, p: &Value) {
    let thinking = &p["generationConfig"]["thinkingConfig"];
    f.reasoning |=
        on(&thinking["includeThoughts"]) || on(&thinking["thinkingBudget"]) || on(&thinking["thinkingLevel"]);
    for tool in p["tools"].as_array().into_iter().flatten() {
        for key in ["functionDeclarations", "function_declarations"] {
            tool_defs(f, &tool[key], &["name"]);
        }
    }
    for key in ["systemInstruction", "system_instruction"] {
        gemini_parts(f, &p[key]["parts"]);
    }
    for c in p["contents"].as_array().into_iter().flatten() {
        gemini_parts(f, &c["parts"]);
    }
}

fn gemini_response(f: &mut Facts, p: &Value) {
    if p.get("error").is_some() {
        f.stop(Some(Stop::Error));
    }
    for c in p["candidates"].as_array().into_iter().flatten() {
        gemini_parts(f, &c["content"]["parts"]);
        let calls = c["content"]["parts"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|p| p.get("functionCall").is_some());
        f.stop(match s(&c["finishReason"]) {
            "" => calls.then_some(Stop::ToolUse),
            "MAX_TOKENS" => Some(Stop::Length),
            "STOP" if calls => Some(Stop::ToolUse),
            "STOP" => Some(Stop::End),
            _ => Some(Stop::Error),
        });
    }
    let u = &p["usageMetadata"];
    f.usage(int(&u["promptTokenCount"]), int(&u["candidatesTokenCount"]));
}

// Interactions

fn interactions_content(f: &mut Facts, content: &Value, thinking: bool) {
    let target = |f: &mut Facts, t: &str| {
        if thinking {
            f.thinking.push_str(t)
        } else {
            f.text.push_str(t)
        }
    };
    if let Some(text) = content.as_str() {
        target(f, text);
    }
    for part in content.as_array().into_iter().flatten() {
        match s(&part["type"]) {
            "text" => target(f, s(&part["text"])),
            "image" => f.images += 1,
            _ => {}
        }
    }
}

fn interactions_step(f: &mut Facts, step: &Value) {
    match s(&step["type"]) {
        "thought" => {
            interactions_content(f, &step["content"], true);
            interactions_content(f, &step["summary"], true);
        }
        "function_call" => f.tool_calls.push((s(&step["name"]).into(), args(&step["arguments"]))),
        "function_result" => f.tool_results.push(result_text(&step["result"])),
        _ => interactions_content(f, &step["content"], false),
    }
}

fn interactions_usage(f: &mut Facts, u: &Value) {
    let pick = |a: &str, b: &str| int(&u[a]).or_else(|| int(&u[b]));
    f.usage(
        pick("total_input_tokens", "input_tokens"),
        pick("total_output_tokens", "output_tokens"),
    );
}

fn interactions_status(f: &mut Facts, i: &Value, saw_call: bool) {
    match s(&i["status"]) {
        "completed" => f.stop(Some(if saw_call { Stop::ToolUse } else { Stop::End })),
        "requires_action" => f.stop(Some(Stop::ToolUse)),
        "incomplete" => f.stop(Some(Stop::Length)),
        "failed" => f.stop(Some(Stop::Error)),
        _ => {}
    }
}

fn interactions_request(f: &mut Facts, p: &Value) {
    tool_defs(f, &p["tools"], &["name"]);
    let config = &p["generation_config"];
    f.reasoning |= on(&config["thinking_level"]) || on(&config["thinking_config"]["thinking_budget"]);
    interactions_content(f, &p["system_instruction"], false);
    if let Some(text) = p["input"].as_str() {
        f.text.push_str(text);
    }
    for step in p["input"].as_array().into_iter().flatten() {
        interactions_step(f, step);
    }
}

fn interactions_response(f: &mut Facts, payloads: &[Value]) {
    let mut tools: BTreeMap<i64, (String, String)> = BTreeMap::new();
    for p in payloads {
        match s(&p["event_type"]) {
            "" => {
                let i = if p.get("interaction").is_some() {
                    &p["interaction"]
                } else {
                    p
                };
                let steps = i.get("steps").or_else(|| i.get("outputs"));
                for step in steps.and_then(Value::as_array).into_iter().flatten() {
                    interactions_step(f, step);
                }
                interactions_usage(f, &i["usage"]);
                interactions_status(f, i, !f.tool_calls.is_empty());
            }
            "step.start" if s(&p["step"]["type"]) == "function_call" => {
                tools.insert(
                    int(&p["index"]).unwrap_or(0),
                    (s(&p["step"]["name"]).into(), String::new()),
                );
            }
            "step.delta" => {
                let d = &p["delta"];
                match s(&d["type"]) {
                    "text" => f.text.push_str(s(&d["text"])),
                    "thought_summary" => {
                        f.thinking.push_str(s(&d["content"]["text"]));
                        f.thinking.push_str(s(&d["text"]));
                    }
                    "arguments_delta" => {
                        let t = tools.entry(int(&p["index"]).unwrap_or(0)).or_default();
                        t.1.push_str(s(&d["arguments"]));
                    }
                    _ => {}
                }
            }
            "interaction.completed" => {
                interactions_usage(f, &p["interaction"]["usage"]);
                interactions_status(f, &p["interaction"], !tools.is_empty());
            }
            "interaction.failed" | "error" => f.stop(Some(Stop::Error)),
            _ => {}
        }
    }
    for (name, json) in tools.into_values() {
        f.tool_calls.push((name, args(&Value::String(json))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claude_stream_joins_deltas_and_reads_usage_from_start_and_delta() {
        let body = br#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":12,"output_tokens":1}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","name":"Bash"}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"1}"}}

data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}
"#;
        let f = facts(Format::Claude, Kind::Stream, body);
        assert_eq!(f.tool_calls, vec![("Bash".to_string(), serde_json::json!({"a": 1}))]);
        assert_eq!(f.usage, Some((12, 3)));
        assert_eq!(f.stop, Some(Stop::ToolUse));
    }

    #[test]
    fn a_chat_stream_joins_tool_arguments_by_index() {
        let body = br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"Bash","arguments":"{\"a\""}}]}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":1}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":12,"completion_tokens":3}}

data: [DONE]
"#;
        let f = facts(Format::OpenAI, Kind::Stream, body);
        assert_eq!(f.tool_calls, vec![("Bash".to_string(), serde_json::json!({"a": 1}))]);
        assert_eq!((f.usage, f.stop), (Some((12, 3)), Some(Stop::ToolUse)));
    }

    #[test]
    fn a_gemini_body_reads_thought_parts_as_thinking() {
        let body = br#"{"candidates":[{"content":{"parts":[{"text":"hm","thought":true},{"text":"Hi"}]},"finishReason":"MAX_TOKENS"}],"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":3}}"#;
        let f = facts(Format::Gemini, Kind::Response, body);
        assert_eq!((f.thinking.as_str(), f.text.as_str()), ("hm", "Hi"));
        assert_eq!((f.usage, f.stop), (Some((12, 3)), Some(Stop::Length)));
    }

    #[test]
    fn a_responses_stream_reads_deltas_and_the_terminal_event() {
        let body = br#"data: {"type":"response.output_text.delta","delta":"Hi"}

data: {"type":"response.incomplete","response":{"status":"incomplete","usage":{"input_tokens":12,"output_tokens":3}}}
"#;
        let f = facts(Format::OpenAIResponse, Kind::Stream, body);
        assert_eq!(f.text, "Hi");
        assert_eq!((f.usage, f.stop), (Some((12, 3)), Some(Stop::Length)));
    }

    #[test]
    fn an_interactions_request_reads_steps() {
        let body = br#"{"system_instruction":"S","input":[{"type":"user_input","content":[{"type":"text","text":"U"}]},{"type":"function_result","call_id":"c","result":"R"}]}"#;
        let f = facts(Format::Interactions, Kind::Request, body);
        assert_eq!((f.text.as_str(), f.tool_results.clone()), ("SU", vec!["R".to_string()]));
    }
}
