use cpa_core::exec::{ExecError, FailureScope};

use super::{betas, cloak, detect, text};
use crate::rawjson;

/// `sanitizeClaudeMessagesForClaudeUpstreamWithDebug`: Claude-family targets (and
/// compat models, which keep empty thinking blocks) go through the shared Messages
/// signature sanitizer; then empty web_search domain lists are removed.
pub(super) fn sanitize_for_upstream(mut body: String, base_model: &str, preserve_empty_thinking: bool) -> String {
    use cpa_common::signature::{Provider, provider_from_model_name, sanitize_claude_messages_for_claude_upstream};
    if (provider_from_model_name(base_model) == Provider::Claude || preserve_empty_thinking)
        && let Some(sanitized) =
            sanitize_claude_messages_for_claude_upstream(body.as_bytes(), base_model, preserve_empty_thinking).0
    {
        body = text(sanitized);
    }
    // sanitizeClaudeWebSearchDomains: one pass over the tools, then the deletes.
    let mut empty = Vec::new();
    for (t, tool) in rawjson::get(&body, "tools").array().iter().enumerate() {
        if !tool.get("type").str().starts_with("web_search_") {
            continue;
        }
        for field in ["allowed_domains", "blocked_domains"] {
            let v = tool.get(field);
            if v.kind() == gjson::Kind::Array && v.array().is_empty() {
                empty.push(format!("tools.{t}.{field}"));
            }
        }
    }
    for path in empty {
        body = rawjson::delete(&body, &path);
    }
    body
}

/// `extractAndRemoveBetas`.
pub(super) fn extract_betas(body: String) -> (Vec<String>, String) {
    let v = rawjson::get(&body, "betas");
    if !v.exists() {
        return (Vec::new(), body);
    }
    let betas = if v.kind() == gjson::Kind::Array {
        v.array()
            .iter()
            .map(|b| b.str().trim().to_owned())
            .filter(|b| !b.is_empty())
            .collect()
    } else {
        Some(v.str().trim().to_owned())
            .filter(|b| !b.is_empty())
            .into_iter()
            .collect()
    };
    (betas, rawjson::delete(&body, "betas"))
}

/// `defaultModelMaxTokens`.
pub(super) const DEFAULT_MAX_TOKENS: i64 = 1024;

/// Fill `thinking.display: updates` for progress-display models with active thinking.
pub(super) fn cloak_thinking_display(body: String) -> String {
    if rawjson::get(&body, "thinking.display").exists() || !betas::progress_display(&rawjson::string(&body, "model")) {
        return body;
    }
    match rawjson::string(&body, "thinking.type").trim().to_lowercase().as_str() {
        "adaptive" | "enabled" => rawjson::set_str(&body, "thinking.display", "updates"),
        _ => body,
    }
}

/// `injectFakeUserID`: a caller's valid Claude Code user ID is kept.
/// `injectFakeUserID`: a missing or invalid `metadata.user_id` is replaced.
pub(super) fn needs_fake_user_id(body: &str) -> bool {
    let existing = rawjson::string(body, "metadata.user_id");
    !rawjson::get(body, "metadata").exists() || existing.is_empty() || !detect::valid_user_id(&existing)
}

/// `injectClaudeDiagnosticsWithState`: after context_management when present.
pub(super) fn inject_diagnostics(body: &str, previous_message_id: &str) -> String {
    let value = if previous_message_id.is_empty() {
        r#"{"previous_message_id":null}"#.to_owned()
    } else {
        format!(
            r#"{{"previous_message_id":{}}}"#,
            cpa_common::json::marshal_string(previous_message_id, false)
        )
    };
    if rawjson::get(body, "diagnostics").exists() {
        return rawjson::set_raw(body, "diagnostics", &value);
    }
    let cm = rawjson::get(body, "context_management");
    if let Some(start) = rawjson::offset(body, &cm) {
        let end = start + cm.json().len();
        return format!("{},\"diagnostics\":{value}{}", &body[..end], &body[end..]);
    }
    rawjson::set_raw(body, "diagnostics", &value)
}

/// `disableThinkingIfToolChoiceForced`.
pub(super) fn disable_thinking_if_tool_choice_forced(body: String) -> String {
    if !matches!(rawjson::get(&body, "tool_choice.type").str(), "any" | "tool") {
        return body;
    }
    let mut body = rawjson::delete(&body, "thinking");
    body = rawjson::delete(&body, "output_config.effort");
    let oc = rawjson::get(&body, "output_config");
    let mut members = 0;
    oc.each(|_, _| {
        members += 1;
        true
    });
    if oc.kind() == gjson::Kind::Object && members == 0 {
        body = rawjson::delete(&body, "output_config");
    }
    body
}

/// `normalizeClaudeSamplingForUpstream`.
pub(super) fn normalize_sampling(mut body: String, native: bool) -> String {
    let thinking = matches!(
        rawjson::string(&body, "thinking.type").trim().to_lowercase().as_str(),
        "enabled" | "adaptive" | "auto"
    );
    if !native {
        body = rawjson::delete_owned(body, "temperature");
        body = rawjson::delete_owned(body, "top_p");
        if thinking {
            body = rawjson::delete_owned(body, "top_k");
        }
        return body;
    }
    if thinking {
        let t = rawjson::get(&body, "temperature");
        if t.exists() && t.f64() != 1.0 {
            body = rawjson::delete(&body, "temperature");
        }
        let p = rawjson::get(&body, "top_p");
        if p.exists() && p.f64() < 0.95 {
            body = rawjson::delete(&body, "top_p");
        }
        return rawjson::delete_owned(body, "top_k");
    }
    if rawjson::get(&body, "temperature").exists() && rawjson::get(&body, "top_p").exists() {
        body = rawjson::delete(&body, "top_p");
    }
    body
}

fn has_mid_system(body: &str) -> bool {
    rawjson::get(body, "messages")
        .array()
        .iter()
        .any(|m| m.get("role").str() == "system")
}

/// `validateClaudeMidSystemMessageModel`.
pub(super) fn validate_mid_system(body: &str, confirmed: bool, first_party: bool) -> Result<(), ExecError> {
    if confirmed || !first_party || !cloak::legacy_system_reminder(body) || !has_mid_system(body) {
        return Ok(());
    }
    let model = rawjson::string(body, "model");
    let model = if model.is_empty() { "unknown".to_owned() } else { model };
    Err(ExecError::local(
        400,
        FailureScope::Request,
        format!(
            "invalid_request_error: role 'system' is not supported on this model. Model {model:?} predates mid-conversation system turns, so system instructions must stay in the top-level system field for it."
        ),
    ))
}

/// `rebuildMidSystemMessagesToTopLevel`: fold role=system turns into `system`.
pub(super) fn rebuild_mid_system(body: &str) -> String {
    let messages = rawjson::get(body, "messages");
    if messages.kind() != gjson::Kind::Array {
        return body.to_owned();
    }
    let mut moved = Vec::new();
    let mut kept = Vec::new();
    for m in messages.array() {
        if m.get("role").str().trim().eq_ignore_ascii_case("system") {
            moved.extend(system_text_parts(&m.get("content")));
        } else {
            kept.push(m.json().to_owned());
        }
    }
    if moved.is_empty() {
        return body.to_owned();
    }
    let mut parts = system_text_parts(&rawjson::get(body, "system"));
    parts.extend(moved);
    let body = rawjson::set_raw(body, "system", &format!("[{}]", parts.join(",")));
    rawjson::set_raw(&body, "messages", &format!("[{}]", kept.join(",")))
}

/// `claudeSystemTextParts`: strings become text blocks, text objects stay raw.
fn system_text_parts(content: &gjson::Value<'_>) -> Vec<String> {
    let block = |text: &str| rawjson::set_str(r#"{"type":"text","text":""}"#, "text", text);
    match content.kind() {
        gjson::Kind::String if !content.str().trim().is_empty() => vec![block(content.str())],
        gjson::Kind::Array => content
            .array()
            .iter()
            .filter_map(|item| match item.kind() {
                gjson::Kind::String if !item.str().trim().is_empty() => Some(block(item.str())),
                gjson::Kind::Object
                    if item.get("type").str() == "text" && !item.get("text").str().trim().is_empty() =>
                {
                    Some(item.json().to_owned())
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// `util.StripClaudeCodeAttributionSystem`.
pub(super) fn strip_attribution_system(body: &str) -> String {
    let system = rawjson::get(body, "system");
    let attribution = |t: &str| t.trim_start().starts_with(cloak::BILLING_PREFIX);
    match system.kind() {
        gjson::Kind::String if attribution(system.str()) => rawjson::delete(body, "system"),
        gjson::Kind::Array => {
            let blocks = system.array();
            let kept: Vec<String> = blocks
                .iter()
                .filter(|b| !(b.get("type").str() == "text" && attribution(b.get("text").str())))
                .map(|b| b.json().to_owned())
                .collect();
            if kept.len() == blocks.len() {
                body.to_owned()
            } else if kept.is_empty() {
                rawjson::delete(body, "system")
            } else {
                rawjson::set_raw(body, "system", &format!("[{}]", kept.join(",")))
            }
        }
        _ => body.to_owned(),
    }
}
