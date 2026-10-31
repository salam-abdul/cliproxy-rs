//! Shared primitives for schema repair and reference resolution.

use crate::json::{self as gj, GoValue};

pub(super) fn ref_name(reference: &[u8]) -> Vec<u8> {
    match reference.iter().rposition(|&c| c == b'/') {
        Some(i) if i + 1 < reference.len() => pointer_unescape(&reference[i + 1..]),
        _ => reference.to_vec(),
    }
}

/// `~1` -> `/`, then `~0` -> `~` (two replacements).
pub(super) fn pointer_unescape(part: &[u8]) -> Vec<u8> {
    let replace = |s: &[u8], from: &[u8], to: u8| {
        let mut out = Vec::with_capacity(s.len());
        let mut i = 0;
        while i < s.len() {
            if s[i..].starts_with(from) {
                out.push(to);
                i += from.len();
            } else {
                out.push(s[i]);
                i += 1;
            }
        }
        out
    };
    replace(&replace(part, b"~1", b'/'), b"~0", b'~')
}

pub(super) fn merge_hint(existing: &[u8], hint: &[u8]) -> Vec<u8> {
    if existing.is_empty() {
        return hint.to_vec();
    }
    if existing == hint
        || existing.starts_with(&[hint, b" ("].concat())
        || contains_seq(existing, &[&b"("[..], hint, b")"].concat())
    {
        return existing.to_vec();
    }
    [existing, b" (", hint, b")"].concat()
}

pub(super) fn contains_seq(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}

/// Decode only the first JSON value; a later decoder read handles trailing bytes.
pub(super) fn decode_first(text: &[u8]) -> Option<GoValue> {
    let start = text.iter().position(|c| !matches!(c, b' ' | b'\t' | b'\n' | b'\r'))?;
    let rest = &text[start..];
    let end = match rest[0] {
        b't' | b'n' => 4,
        b'f' => 5,
        _ => gj::parse(rest).raw.len(),
    };
    GoValue::parse(rest.get(..end)?)
}
