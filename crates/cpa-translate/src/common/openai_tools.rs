use cpa_common::json as gj;
use cpa_core::config::go_trim_space;
use std::collections::{HashMap, HashSet};

// OpenAI tool-call ordering (common/openai_tools.go)

/// common.AlignOpenAIToolCallMessages: moves each `tool` reply directly after the
/// assistant message that made the call, when every call ID of that assistant message is
/// unambiguous and answered exactly once later on.
pub fn align_openai_tool_call_messages(messages: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    align_openai_tool_call_messages_with(messages, &[])
}

/// AlignOpenAIToolCallMessages with extra call IDs treated as ambiguous (trimmed).
pub fn align_openai_tool_call_messages_with(messages: Vec<Vec<u8>>, extra_ambiguous: &[Vec<u8>]) -> Vec<Vec<u8>> {
    if messages.len() <= 1 {
        return messages;
    }
    struct Assistant {
        index: usize,
        call_ids: Vec<Vec<u8>>,
        has_empty: bool,
    }
    let mut assistants = vec![];
    let mut assistant_by_call: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut ambiguous: HashSet<Vec<u8>> = extra_ambiguous
        .iter()
        .map(|id| go_trim_space(id).to_vec())
        .filter(|id| !id.is_empty())
        .collect();
    let mut tools_by_call: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
    for (i, raw) in messages.iter().enumerate() {
        match gj::get(raw, "role").bytes().as_ref() {
            b"assistant" => {
                let calls = gj::get(raw, "tool_calls");
                if !calls.is_array() {
                    continue;
                }
                let calls = calls.array();
                if calls.is_empty() {
                    continue;
                }
                let mut call_ids = vec![];
                let mut has_empty = false;
                for call in &calls {
                    let id = call.get("id").bytes().into_owned();
                    if id.is_empty() {
                        ambiguous.insert(vec![]);
                        has_empty = true;
                        continue;
                    }
                    if assistant_by_call.insert(id.clone(), i).is_some() {
                        ambiguous.insert(id.clone());
                    }
                    call_ids.push(id);
                }
                if !call_ids.is_empty() || has_empty {
                    assistants.push(Assistant {
                        index: i,
                        call_ids,
                        has_empty,
                    });
                }
            }
            b"tool" => {
                let id = gj::get(raw, "tool_call_id").bytes().into_owned();
                if id.is_empty() {
                    ambiguous.insert(vec![]);
                } else {
                    let indexes = tools_by_call.entry(id.clone()).or_default();
                    indexes.push(i);
                    if indexes.len() > 1 {
                        ambiguous.insert(id);
                    }
                }
            }
            _ => {}
        }
    }
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    'assistants: for a in &assistants {
        if a.has_empty {
            continue;
        }
        let mut matched = vec![];
        for id in &a.call_ids {
            match tools_by_call.get(id).map(Vec::as_slice) {
                Some(&[tool]) if !ambiguous.contains(id) && tool > a.index => matched.push(tool),
                _ => continue 'assistants,
            }
        }
        matched.sort_unstable();
        if matched
            .iter()
            .enumerate()
            .any(|(offset, &tool)| tool != a.index + offset + 1)
        {
            groups.insert(a.index, matched);
        }
    }
    if groups.is_empty() {
        return messages;
    }
    let moved: HashSet<usize> = groups.values().flatten().copied().collect();
    let mut out = Vec::with_capacity(messages.len());
    for (i, message) in messages.iter().enumerate() {
        if moved.contains(&i) {
            continue;
        }
        out.push(message.clone());
        if let Some(tools) = groups.get(&i) {
            out.extend(tools.iter().map(|&t| messages[t].clone()));
        }
    }
    out
}
