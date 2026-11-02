//! Devin tool rules shared by the Devin executor and the Responses-to-Interactions
//! translator (translator/common/devin_tools.go).

use std::sync::LazyLock;

use cpa_core::config::go_trim_space;
use regex::bytes::Regex;

use crate::gostr::{GoStr, contains, index, lower_bytes};

const EXEC_TARGET: &[u8] = b"returning output or a session ID for ongoing interaction";
const EXEC_OBFUSCATED: &[u8] = b"returning output or an session ID for ongoing interaction";
const STDIN_TARGET: &[u8] = b"Writes characters to an existing unified exec session and returns recent output.";
const STDIN_OBFUSCATED: &[u8] = b"Writes characters to a existing unified exec session and returns recent output.";

static EXEC_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)returning output or a session ID for ongoing interaction").expect("valid"));
static STDIN_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)Writes characters to an existing unified exec session and returns recent output(\.?)")
        .expect("valid")
});

/// `IsDevinCodexAppAutomationUpdate`.
pub fn is_codex_app_automation_update(namespace: &[u8], tool: &[u8]) -> bool {
    let fold = |a: &[u8], b: &str| String::from_utf8_lossy(go_trim_space(a)).go_eq_fold(b);
    (fold(namespace, "mcp__codex_app") && fold(tool, "automation_update"))
        || fold(tool, "mcp__codex_app__automation_update")
}

/// `bytes.ReplaceAll` for a non-empty `from`.
fn replace_all(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut rest = haystack;
    while let Some(i) = index(rest, from).filter(|_| !from.is_empty()) {
        out.extend_from_slice(&rest[..i]);
        out.extend_from_slice(to);
        rest = &rest[i + from.len()..];
    }
    out.extend_from_slice(rest);
    out
}

/// `ObfuscateExecCommandDescription` / `ObfuscateWriteStdinDescription`.
fn obfuscate(desc: &[u8], target: &[u8], obfuscated: &[u8], regex: &Regex, replacement: &[u8]) -> Vec<u8> {
    if contains(desc, obfuscated) {
        desc.to_vec()
    } else if contains(desc, target) {
        replace_all(desc, target, obfuscated)
    } else {
        regex.replace_all(desc, replacement).into_owned()
    }
}

/// `ObfuscateExecCommandDescription`.
pub fn obfuscate_exec_command_description(desc: &[u8]) -> Vec<u8> {
    obfuscate(desc, EXEC_TARGET, EXEC_OBFUSCATED, &EXEC_REGEX, EXEC_OBFUSCATED)
}

/// `ObfuscateWriteStdinDescription`.
pub fn obfuscate_write_stdin_description(desc: &[u8]) -> Vec<u8> {
    obfuscate(
        desc,
        STDIN_TARGET,
        STDIN_OBFUSCATED,
        &STDIN_REGEX,
        b"Writes characters to a existing unified exec session and returns recent output$1",
    )
}

/// `SanitizeDevinToolDescription`: Codex's `exec_command` and `write_stdin` wording gets
/// a one-letter change; the tool name is lowercased as Go's `strings.ToLower` does.
pub fn sanitize_tool_description(tool: &[u8], desc: &[u8]) -> Vec<u8> {
    if desc.is_empty() {
        return Vec::new();
    }
    let tool = lower_bytes(go_trim_space(tool));
    let mut desc = desc.to_vec();
    if tool == "exec_command" || tool.ends_with("__exec_command") {
        desc = obfuscate_exec_command_description(&desc);
    }
    if tool == "write_stdin" || tool.ends_with("__write_stdin") {
        desc = obfuscate_write_stdin_description(&desc);
    }
    desc
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values from Go 1.26 (SanitizeDevinToolDescription, regexp (?i) folding).
    #[test]
    fn devin_descriptions_match_go() {
        let exec = sanitize_tool_description(
            b" Exec_Command ",
            "Runs, RETURNING output or a \u{17f}ession ID for ongoing interaction.".as_bytes(),
        );
        assert_eq!(
            exec,
            b"Runs, returning output or an session ID for ongoing interaction."
        );
        let stdin = sanitize_tool_description(
            b"ns__write_stdin",
            b"WRITES characters to an existing unified exec session and returns recent output!",
        );
        assert_eq!(
            stdin,
            b"Writes characters to a existing unified exec session and returns recent output!"
        );
        let kept = b"returning output or an session ID for ongoing interaction, returning output or a session ID for ongoing interaction";
        assert_eq!(sanitize_tool_description(b"exec_command", kept), kept);
        // Go lowercases U+0130 to a plain `i`, so this is still `write_stdin`.
        let dotted = sanitize_tool_description("WR\u{130}TE_STD\u{130}N".as_bytes(), STDIN_TARGET);
        assert_eq!(dotted, STDIN_OBFUSCATED);
        assert!(is_codex_app_automation_update(
            b" MCP__codex_app ",
            b"Automation_Update"
        ));
        assert!(is_codex_app_automation_update(
            b"",
            b"mcp__codex_app__automation_update"
        ));
        assert!(!is_codex_app_automation_update(b"mcp__codex_app", b"other"));
    }
}
