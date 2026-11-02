//! Codex's 64-byte limits on tool names and call ids, and its service tiers, shared by
//! the Codex translators and the Codex and xAI reasoning replay.

use std::collections::{HashMap, HashSet};

use cpa_common::gostr::last_index;
use cpa_common::json::{Kind, Res};
use cpa_core::config::go_trim_space;
use sha2::{Digest, Sha256};

use crate::common::{go_lower, sanitize_claude_tool_id};

/// The Responses API limit on tool names and call ids, in bytes.
pub const NAME_LIMIT: usize = 64;

/// `shortenNameIfNeeded`: at most 64 bytes, keeping `mcp__` plus the last `__` segment.
pub fn shorten_name(name: &[u8]) -> Vec<u8> {
    if name.len() <= NAME_LIMIT {
        return name.to_vec();
    }
    if name.starts_with(b"mcp__")
        && let Some(idx) = last_index(name, b"__")
        && idx > 0
    {
        let mut candidate = [&b"mcp__"[..], &name[idx + 2..]].concat();
        candidate.truncate(NAME_LIMIT);
        return candidate;
    }
    name[..NAME_LIMIT].to_vec()
}

/// `buildShortNameMap`: each name to a unique short name from `shorten`, suffixing `_1`,
/// `_2`, ... on collisions.
pub fn short_name_map<N: AsRef<[u8]>>(
    names: impl IntoIterator<Item = N>,
    shorten: impl Fn(&[u8]) -> Vec<u8>,
) -> HashMap<Vec<u8>, Vec<u8>> {
    let mut used: HashSet<Vec<u8>> = HashSet::new();
    let mut map = HashMap::new();
    for name in names {
        let name = name.as_ref();
        let candidate = shorten(name);
        let unique = if used.contains(&candidate) {
            (1usize..)
                .map(|i| {
                    let suffix = format!("_{i}");
                    let allowed = NAME_LIMIT.saturating_sub(suffix.len());
                    [&candidate[..candidate.len().min(allowed)], suffix.as_bytes()].concat()
                })
                .find(|tmp| !used.contains(tmp))
                .unwrap_or_default()
        } else {
            candidate
        };
        used.insert(unique.clone());
        map.insert(name.to_vec(), unique);
    }
    map
}

/// `shortenCodexCallIDIfNeeded`: call ids within the 64-byte limit, with a
/// sha256-derived suffix.
pub fn shorten_call_id(id: &[u8]) -> Vec<u8> {
    if id.len() <= NAME_LIMIT {
        return id.to_vec();
    }
    let sum = Sha256::digest(id);
    let suffix = format!("_{}", cpa_core::hex::encode(&sum[..8]));
    [&id[..NAME_LIMIT - suffix.len()], suffix.as_bytes()].concat()
}

/// The form of a call id a Claude client sees: `SanitizeClaudeToolID`, then shortened.
pub fn claude_visible_call_id(id: &[u8]) -> Vec<u8> {
    shorten_call_id(&sanitize_claude_tool_id(id))
}

/// `normalizeCodexServiceTier`: `fast` and `priority` are `priority`, `ultrafast` stays;
/// anything else, or a non-string, is no tier.
pub fn service_tier(r: &Res<'_>) -> &'static str {
    if r.kind != Kind::String {
        return "";
    }
    match go_lower(go_trim_space(&r.s)).as_slice() {
        b"fast" | b"priority" => "priority",
        b"ultrafast" => "ultrafast",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_tiers_normalize_like_go() {
        let tier = |json: &str| service_tier(&cpa_common::json::get(json.as_bytes(), "t"));
        assert_eq!(tier(r#"{"t":" Fast "}"#), "priority");
        assert_eq!(tier(r#"{"t":"PRİORİTY"}"#), "priority", "Go lowercases U+0130 to i");
        assert_eq!(tier(r#"{"t":"UltraFast"}"#), "ultrafast");
        assert_eq!(tier(r#"{"t":"flex"}"#), "");
        assert_eq!(tier(r#"{"t":1}"#), "");
    }

    #[test]
    fn names_and_call_ids_fit_the_limit() {
        let long = [b'a'; 70];
        assert_eq!(shorten_name(&long).len(), NAME_LIMIT);
        let mcp = [&b"mcp__server__"[..], &[b'x'; 60][..], b"__tool"].concat();
        assert_eq!(shorten_name(&mcp), b"mcp__tool");
        let map = short_name_map([&long[..], &[&long[..], b"b"].concat()[..]], shorten_name);
        assert_eq!(map[&long[..]], &long[..64]);
        assert_eq!(map[&[&long[..], b"b"].concat()], [&long[..62], b"_1"].concat());
        let id = shorten_call_id(&[b'c'; 80]);
        assert_eq!((id.len(), &id[..47], id[47]), (64, &[b'c'; 47][..], b'_'));
        assert_eq!(claude_visible_call_id(b"call.1"), b"call_1");
    }
}
