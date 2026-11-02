//! The Responses `input` helpers of Codex reasoning replay that xAI replay reuses (Go's
//! `codexReplay*` and `codexReasoningReplay*` helpers): comparable call ids, tool-call
//! keys, message roles, the insertion index and call-id alignment.

use std::collections::{HashMap, HashSet};

use cpa_common::gostr::GoStr;
use cpa_common::json::{self as gj, Res};
use cpa_translate::codex_names::claude_visible_call_id;

use crate::replay::text;

pub(crate) fn is_tool_call(kind: &str) -> bool {
    matches!(kind, "function_call" | "custom_tool_call")
}

pub(crate) fn is_tool_output(kind: &str) -> bool {
    matches!(kind, "function_call_output" | "custom_tool_call_output")
}

/// `codexReplayComparableCallIDs`: the id, plus the form Claude clients see
/// (`SanitizeClaudeToolID`, shortened to 64 bytes) when it differs.
pub(crate) fn comparable_call_ids(id: &str) -> Vec<String> {
    let id = id.trim();
    if id.is_empty() {
        return Vec::new();
    }
    let visible = String::from_utf8_lossy(&claude_visible_call_id(id.as_bytes())).into_owned();
    if visible.is_empty() || visible == id {
        vec![id.to_owned()]
    } else {
        vec![id.to_owned(), visible]
    }
}

/// `codexReplayToolCallKeys`.
pub(crate) fn tool_call_keys(item: &Res<'_>) -> Vec<String> {
    let kind = text(&item.get("type"));
    if !is_tool_call(&kind) {
        return Vec::new();
    }
    comparable_call_ids(&item.get("call_id").str())
        .into_iter()
        .map(|id| format!("{kind}:{id}"))
        .collect()
}

/// `codexReplayMessageRole`: the role, lowercased as Go's `strings.ToLower` does.
pub(crate) fn message_role(item: &Res<'_>) -> Option<String> {
    let kind = text(&item.get("type"));
    let role = text(&item.get("role")).go_lower();
    (!role.is_empty() && (kind.is_empty() || kind == "message")).then_some(role)
}

/// `codexReasoningReplayInsertIndex`: before the output answering a replayed call, else
/// at the last assistant message, else before the first non-instruction item.
pub(crate) fn insert_index(inputs: &[Res<'_>], replay: &[Vec<u8>]) -> usize {
    let mut ids = HashSet::new();
    for item in replay {
        let r = gj::parse(item);
        if is_tool_call(&text(&r.get("type"))) {
            ids.extend(comparable_call_ids(&r.get("call_id").str()));
        }
    }
    if !ids.is_empty()
        && let Some(index) = inputs.iter().position(|input| {
            let id = text(&input.get("call_id"));
            is_tool_output(&text(&input.get("type"))) && (id.is_empty() || ids.contains(&id))
        })
    {
        return index;
    }
    if let Some(index) = inputs
        .iter()
        .rposition(|i| message_role(i).as_deref() == Some("assistant"))
    {
        return index;
    }
    inputs
        .iter()
        .position(|i| !matches!(message_role(i).as_deref(), Some("developer" | "system")))
        .unwrap_or(inputs.len())
}

/// `codexAlignReasoningReplayToolCallIDs`: replayed calls take the call id the input's
/// output uses.
pub(crate) fn align_call_ids(inputs: &[Res<'_>], items: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut output_ids: HashMap<String, String> = HashMap::new();
    for input in inputs {
        if !is_tool_output(&text(&input.get("type"))) {
            continue;
        }
        let id = text(&input.get("call_id"));
        if id.is_empty() {
            continue;
        }
        for candidate in comparable_call_ids(&id) {
            output_ids.insert(candidate, id.clone());
        }
    }
    if output_ids.is_empty() {
        return items;
    }
    items
        .into_iter()
        .map(|item| {
            let r = gj::parse(&item);
            if !is_tool_call(&text(&r.get("type"))) {
                return item;
            }
            let id = text(&r.get("call_id"));
            let output = comparable_call_ids(&id)
                .into_iter()
                .find_map(|c| output_ids.get(&c).filter(|v| !v.is_empty()).cloned());
            match output {
                Some(output) if output != id => gj::try_set_str(&item, "call_id", &output).unwrap_or(item),
                _ => item,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_lowercase_like_go() {
        let items: Vec<Vec<u8>> = vec![
            br#"{"role":"system","content":"s"}"#.to_vec(),
            r#"{"type":"message","role":"ASSİSTANT","content":"a"}"#.as_bytes().to_vec(),
            br#"{"role":"user","content":"u"}"#.to_vec(),
        ];
        let inputs: Vec<Res<'_>> = items.iter().map(|i| gj::parse(i)).collect();
        // Go lowercases U+0130 to a plain `i`; Unicode lowercasing would add a combining dot.
        assert_eq!(message_role(&inputs[1]).as_deref(), Some("assistant"));
        assert_eq!(insert_index(&inputs, &[]), 1);
    }

    #[test]
    fn long_call_ids_shorten_like_go() {
        // 70 sanitized characters: 47-byte prefix plus `_` and 16 hex digits.
        let id = "call.".to_owned() + &"x".repeat(65);
        let ids = comparable_call_ids(&id);
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[1].len(), 64);
        assert!(ids[1].starts_with("call_xxx"));
        assert_eq!(comparable_call_ids("  call_1 "), vec!["call_1".to_owned()]);
    }
}
