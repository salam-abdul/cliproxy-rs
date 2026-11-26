//! One send's tool facts, read from its stored request and response bodies. Bodies are
//! only read: values come from `cpa_common::json` over the stored bytes, after
//! `valid` accepted them.

use cpa_common::json::{self as gj, Kind};

use super::super::row::{Row, Source};
use super::{Hasher, hash};

/// Which conversation a send belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Conv {
    /// A send that starts or resends a conversation: the hash of its session and the
    /// exact bytes of its first message.
    Start(i64),
    /// A thread continuation: the hash of the response message ID it continues.
    Continue(i64),
    None,
}

/// A tool the request defined, and the size of its definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Def {
    pub tool: String,
    pub bytes: i64,
}

/// A tool call: the hash of its ID, its tool, and a Skill call's skill name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Call {
    pub id: i64,
    pub tool: String,
    pub skill: Option<String>,
}

/// A tool result: the hash of the call ID it answers, its error flag, the bytes of its
/// content and the bytes of the text blocks that follow it in the same message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Outcome {
    pub id: i64,
    pub is_error: bool,
    pub bytes: i64,
    pub loaded: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SendFacts {
    pub at_ms: i64,
    pub provider: Option<String>,
    pub conv: Conv,
    /// Hash of the response's message ID.
    pub message: Option<i64>,
    pub defs: Vec<Def>,
    pub calls: Vec<Call>,
    pub results: Vec<Outcome>,
    /// Not a Claude Messages send.
    pub skipped_format: bool,
    /// A body was truncated, not text, or not valid JSON or SSE.
    pub cut: bool,
}

/// The tool facts of one stored send. A row that is not a Claude Messages send is only
/// counted as skipped. Each body is judged on its own: a complete request still yields
/// its facts when the response was cut, and the other way round.
pub(crate) fn scan(row: &Row, prefixes: &[String]) -> SendFacts {
    let mut facts = SendFacts {
        at_ms: row.started_at_us.div_euclid(1000),
        provider: row.provider.clone(),
        conv: Conv::None,
        message: None,
        defs: Vec::new(),
        calls: Vec::new(),
        results: Vec::new(),
        skipped_format: false,
        cut: false,
    };
    if row.provider.as_deref() != Some("claude") {
        facts.skipped_format = true;
        return facts;
    }
    let names = Names {
        prefixes,
        proxy: row.source == Source::Proxy,
    };
    let request = row
        .request_body
        .as_deref()
        .filter(|_| !row.request_truncated)
        .map(str::as_bytes)
        .filter(|body| gj::valid(body));
    match request {
        Some(body) => read_request(body, row.session_id.as_deref().unwrap_or(""), &names, &mut facts),
        None => facts.cut = true,
    }
    let response = row.response_body.as_deref().filter(|_| !row.response_truncated);
    if !response.is_some_and(|body| read_response(body.as_bytes(), &names, &mut facts)) {
        facts.cut = true;
    }
    facts
}

/// Reads aliased tool names back. First the alias prefixes of the configured client
/// keys: a name under one is the text after the first `_` that follows the prefix (the
/// alias's tool word ends there). Then, on translating-proxy rows only, any name made of
/// alias words, which covers keys outside the config such as a Home user key; native
/// rows are never aliased. Every other name, including real MCP names shaped like an
/// alias, is kept.
struct Names<'a> {
    prefixes: &'a [String],
    proxy: bool,
}

impl Names<'_> {
    fn read(&self, name: &str) -> String {
        self.prefixes
            .iter()
            .filter_map(|prefix| name.strip_prefix(prefix.as_str()))
            .filter_map(|rest| rest.split_once('_').map(|(_, original)| original))
            .find(|original| !original.is_empty())
            .or_else(|| self.proxy.then(|| cpa_exec::claude::alias_original(name)).flatten())
            .unwrap_or(name)
            .to_owned()
    }

    /// A `tool_use` or `server_tool_use` block.
    fn call(&self, block: &gj::Res<'_>) -> Option<Call> {
        let id = block.get("id").str();
        if id.is_empty() {
            return None;
        }
        let tool = self.read(&block.get("name").str());
        let skill = (tool == "Skill")
            .then(|| block.get("input.skill"))
            .filter(|skill| skill.kind == Kind::String)
            .map(|skill| skill.str().into_owned());
        Some(Call {
            id: hash(id.as_bytes()),
            tool,
            skill,
        })
    }
}

fn read_request(body: &[u8], session: &str, names: &Names<'_>, facts: &mut SendFacts) {
    let tools = gj::get(body, "tools");
    if tools.is_array() {
        for tool in tools.array() {
            // Calls name a tool by its `name`, versioned server tools included.
            let name = Some(tool.get("name").str())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| tool.get("type").str());
            if !name.is_empty() {
                facts.defs.push(Def {
                    tool: names.read(&name),
                    bytes: len(tool.raw()),
                });
            }
        }
    }
    let messages = gj::get(body, "messages");
    let messages = if messages.is_array() {
        messages.array()
    } else {
        Vec::new()
    };
    // A continuation as the Claude executor reads one: `thread.type` is `continue` and
    // the parent is named.
    let parent = gj::get(body, "thread.previous_message_id");
    let continues = gj::get(body, "thread.type").str() == "continue";
    facts.conv = if continues && parent.kind == Kind::String && !parent.str().is_empty() {
        Conv::Continue(hash(&parent.bytes()))
    } else if let Some(first) = messages.first() {
        Conv::Start(conversation_key(session, first.raw()))
    } else {
        Conv::None
    };
    for message in &messages {
        let content = message.get("content");
        if !content.is_array() {
            continue;
        }
        let assistant = message.get("role").str() == "assistant";
        // The text block right after a result is what a Skill result loaded into the
        // conversation; later blocks, such as reminders, are not.
        let mut after_result = false;
        for block in content.array() {
            let just_after = std::mem::take(&mut after_result);
            match block.get("type").str().as_ref() {
                "tool_use" | "server_tool_use" if assistant => facts.calls.extend(names.call(&block)),
                kind if assistant && kind.ends_with("_tool_result") => {
                    facts.results.extend(server_result(&block));
                }
                "tool_result" => {
                    let id = block.get("tool_use_id").str();
                    after_result = !id.is_empty();
                    if after_result {
                        facts.results.push(Outcome {
                            id: hash(id.as_bytes()),
                            is_error: block.get("is_error").kind == Kind::True,
                            bytes: len(block.get("content").raw()),
                            loaded: 0,
                        });
                    }
                }
                "text" if just_after => {
                    if let Some(result) = facts.results.last_mut() {
                        result.loaded = len(block.raw());
                    }
                }
                _ => {}
            }
        }
    }
}

/// The key of a conversation: its session and its first message, member by member
/// without any `cache_control`, which a client adds to the first send and moves later.
/// Raw spans are hashed in order with a separator byte after each, so nothing is
/// re-serialized and spans split differently cannot collide.
fn conversation_key(session: &str, first: &[u8]) -> i64 {
    let mut hasher = Hasher::default();
    hasher.update(session.as_bytes());
    hasher.update(&[0]);
    gj::object_members(first, |key, _, value| {
        if key == b"cache_control" {
            return true;
        }
        hasher.update(key);
        hasher.update(&[0]);
        if key == b"content" && value.first() == Some(&b'[') {
            for block in gj::parse(value).array() {
                hasher.update(&[1]);
                if block.is_object() {
                    gj::object_members(block.raw(), |key, _, value| {
                        if key != b"cache_control" {
                            hasher.update(key);
                            hasher.update(&[0]);
                            hasher.update(value);
                            hasher.update(&[0]);
                        }
                        true
                    });
                } else {
                    hasher.update(block.raw());
                    hasher.update(&[0]);
                }
            }
            hasher.update(&[2]);
        } else {
            hasher.update(value);
            hasher.update(&[0]);
        }
        true
    });
    hasher.finish()
}

/// Reads a buffered JSON or SSE response. False when the body is neither.
fn read_response(body: &[u8], names: &Names<'_>, facts: &mut SendFacts) -> bool {
    if body.trim_ascii_start().first() == Some(&b'{') {
        if !gj::valid(body) {
            return false;
        }
        let id = gj::get(body, "id");
        facts.message = (id.kind == Kind::String && !id.str().is_empty()).then(|| hash(&id.bytes()));
        let content = gj::get(body, "content");
        if content.is_array() {
            for block in content.array() {
                match block.get("type").str().as_ref() {
                    "tool_use" | "server_tool_use" => facts.calls.extend(names.call(&block)),
                    kind if kind.ends_with("_tool_result") => facts.results.extend(server_result(&block)),
                    _ => {}
                }
            }
        }
        return true;
    }
    let events: Vec<&[u8]> = body
        .split(|&b| b == b'\n')
        .filter_map(|line| line.strip_prefix(b"data:"))
        .map(<[u8]>::trim_ascii)
        .collect();
    if events.is_empty() || !events.iter().all(|event| gj::valid(event)) {
        return false;
    }
    // Tool blocks by stream index, with the joined input of a Skill block.
    let mut blocks: Vec<(i64, Call, Option<String>)> = Vec::new();
    for event in events {
        match gj::get(event, "type").str().as_ref() {
            "message_start" => {
                let id = gj::get(event, "message.id");
                if id.kind == Kind::String && !id.str().is_empty() {
                    facts.message = Some(hash(&id.bytes()));
                }
            }
            "content_block_start" => {
                let block = gj::get(event, "content_block");
                let kind = block.get("type").str();
                if kind.ends_with("_tool_result") {
                    facts.results.extend(server_result(&block));
                } else if (kind == "tool_use" || kind == "server_tool_use")
                    && let Some(call) = names.call(&block)
                {
                    let input = (call.tool == "Skill").then(String::new);
                    blocks.push((gj::get(event, "index").int(), call, input));
                }
            }
            "content_block_delta" => {
                let delta = gj::get(event, "delta");
                if delta.get("type").str() != "input_json_delta" {
                    continue;
                }
                let index = gj::get(event, "index").int();
                if let Some((_, _, Some(input))) = blocks.iter_mut().rev().find(|(i, _, _)| *i == index) {
                    input.push_str(&delta.get("partial_json").str());
                }
            }
            _ => {}
        }
    }
    for (_, mut call, input) in blocks {
        if call.skill.is_none() {
            call.skill = input.as_deref().and_then(skill_of);
        }
        facts.calls.push(call);
    }
    true
}

/// The result of a server tool, an assistant block whose type ends in `_tool_result`:
/// an error when its content's type ends in `_error` or `is_error` is true.
fn server_result(block: &gj::Res<'_>) -> Option<Outcome> {
    let id = block.get("tool_use_id").str();
    if id.is_empty() {
        return None;
    }
    let content = block.get("content");
    Some(Outcome {
        id: hash(id.as_bytes()),
        is_error: block.get("is_error").kind == Kind::True || content.get("type").str().ends_with("_error"),
        bytes: len(content.raw()),
        loaded: 0,
    })
}

/// The `skill` string of a Skill call's joined input.
fn skill_of(input: &str) -> Option<String> {
    match serde_json::from_str::<serde_json::Value>(input).ok()?.get("skill")? {
        serde_json::Value::String(skill) => Some(skill.clone()),
        _ => None,
    }
}

fn len(raw: &[u8]) -> i64 {
    i64::try_from(raw.len()).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[path = "scan_tests.rs"]
mod tests;
