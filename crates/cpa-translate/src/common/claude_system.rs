use cpa_common::json::{Kind, Res};
use cpa_core::config::go_trim_space;

use super::{is_claude_code_attribution_text, norm};

// ---------------------------------------------------------------------------------------
// Structured output (common/claude_system.go)

const JSON_OBJECT_INSTRUCTION: &str = "You must format your entire response as a valid JSON object. Do not include any explanations, markdown code blocks (such as ```json), or any text outside of the JSON object.";

pub fn claude_structured_output_instruction(format: &Res<'_>) -> Vec<u8> {
    if !format.exists() {
        return vec![];
    }
    match norm(&format.get("type")).as_str() {
        "json_object" => JSON_OBJECT_INSTRUCTION.into(),
        "json_schema" => {
            let json_schema = format.get("json_schema");
            let mut schema = json_schema.get("schema");
            if !schema.exists() {
                schema = format.get("schema");
            }
            if !schema.exists() {
                return JSON_OBJECT_INSTRUCTION.into();
            }
            let mut out = b"You must format your entire response as valid JSON that conforms strictly to the following JSON schema:\n".to_vec();
            for (key, label) in [("name", "Schema Name: "), ("description", "Schema Description: ")] {
                let mut value = go_trim_space(&json_schema.get(key).bytes()).to_vec();
                if value.is_empty() {
                    value = go_trim_space(&format.get(key).bytes()).to_vec();
                }
                if !value.is_empty() {
                    out.extend_from_slice(label.as_bytes());
                    out.extend_from_slice(&value);
                    out.push(b'\n');
                }
            }
            out.extend_from_slice(b"JSON Schema:\n");
            out.extend_from_slice(&schema.raw);
            out.extend_from_slice(b"\nDo not include any explanations, markdown code blocks (such as ```json), or any text outside of the JSON object.");
            out
        }
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------------------
// Claude system text (common/claude_system.go)

/// `<system-reminder>\n{text}\n</system-reminder>` (common.SystemReminderText).
pub fn system_reminder_text(text: &[u8]) -> Vec<u8> {
    [&b"<system-reminder>\n"[..], text, b"\n</system-reminder>"].concat()
}

/// common.ClaudeMessageSystemReminderText: a Claude `system`-role message's text parts
/// (attribution lines dropped) joined with newlines and wrapped as a reminder.
pub fn claude_message_system_reminder_text(content: &Res<'_>) -> Option<Vec<u8>> {
    let mut parts: Vec<Vec<u8>> = vec![];
    let mut keep = |text: Vec<u8>| {
        if !text.is_empty() && !is_claude_code_attribution_text(&text) {
            parts.push(text);
        }
    };
    if content.kind == Kind::String {
        keep(content.s.to_vec());
    } else if content.is_array() {
        content.each(|_, item| {
            if item.get("type").bytes().as_ref() == b"text" {
                keep(item.get("text").bytes().into_owned());
            }
            true
        });
    }
    let text = parts.join(&b'\n');
    (!parts.is_empty() && !go_trim_space(&text).is_empty()).then(|| system_reminder_text(&text))
}

/// common.AlignClaudeToolResults: when a user turn answers exactly the pending tool uses,
/// its tool_result parts are reordered (in their own slots) to follow the tool_use order.
pub fn align_claude_tool_results<'a>(parts: Vec<Res<'a>>, tool_use_ids: &[Vec<u8>]) -> Vec<Res<'a>> {
    if tool_use_ids.is_empty() {
        return parts;
    }
    let slots: Vec<usize> = (0..parts.len())
        .filter(|&i| parts[i].get("type").bytes().as_ref() == b"tool_result")
        .collect();
    if slots.len() != tool_use_ids.len() {
        return parts;
    }
    let mut used = vec![false; slots.len()];
    let mut order = Vec::with_capacity(slots.len());
    for id in tool_use_ids {
        let found = slots.iter().enumerate().position(|(n, &slot)| {
            !used[n] && !id.is_empty() && parts[slot].get("tool_use_id").bytes().as_ref() == id.as_slice()
        });
        let Some(n) = found else { return parts };
        used[n] = true;
        order.push(slots[n]);
    }
    let mut out = parts.clone();
    for (slot, from) in slots.iter().zip(order) {
        out[*slot] = parts[from].clone();
    }
    out
}
