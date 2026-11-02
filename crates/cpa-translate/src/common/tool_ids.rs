use cpa_common::json as gj;
use rand::Rng;
use std::sync::atomic::{AtomicU64, Ordering};

use super::now_nanos;

// Tool IDs and names (common/request.go, util/claude_tool_id.go)

const TOOLU_LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// `toolu_` plus 24 random alphanumerics (common.GenerateClaudeToolCallID).
pub fn generate_claude_tool_call_id() -> Vec<u8> {
    let mut rng = rand::rng();
    let mut out = b"toolu_".to_vec();
    out.extend((0..24).map(|_| TOOLU_LETTERS[rng.random_range(0..TOOLU_LETTERS.len())]));
    out
}

/// Replaces every rune outside `[a-zA-Z0-9_-]` (each invalid byte counts as one) with `_`
/// (also Codex's sanitizeToolName).
pub fn sanitize(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let (c, n) = gj::decode_rune(&s[i..]);
        match c {
            Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '-' => out.push(c as u8),
            _ => out.push(b'_'),
        }
        i += n;
    }
    out
}

static TOOL_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn sanitize_claude_tool_id(id: &[u8]) -> Vec<u8> {
    let s = sanitize(id);
    if s.is_empty() {
        let n = TOOL_ID_COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        return format!("toolu_{}_{n}", now_nanos()).into_bytes();
    }
    s
}

pub fn sanitize_claude_function_name(name: &[u8]) -> Vec<u8> {
    if name.is_empty() {
        return vec![];
    }
    let mut s = sanitize(name);
    s.truncate(64);
    if s.is_empty() {
        s.push(b'_');
    }
    s
}
