//! Shared translator helpers: internal/translator/common and the translator-facing parts
//! of internal/util. Strings are bytes, as in Go.

use cpa_common::json::{self as gj, Kind, Res};
use cpa_core::config::go_trim_space;
use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

mod cache_control;
mod claude_messages;
mod claude_system;
mod claude_user_id;
mod openai_tools;
mod tool_ids;
mod tool_schema;

pub use cache_control::{attach_cache_control, attach_message_cache_control, attach_tool_message_cache_control};
pub use claude_messages::ClaudeMessages;
pub use claude_system::{
    align_claude_tool_results, claude_message_system_reminder_text, claude_structured_output_instruction,
    system_reminder_text,
};
pub use claude_user_id::{derive_claude_user_id, is_gemini_thought_part};
pub use openai_tools::{align_openai_tool_call_messages, align_openai_tool_call_messages_with};
pub use tool_ids::{generate_claude_tool_call_id, sanitize, sanitize_claude_function_name, sanitize_claude_tool_id};
pub use tool_schema::normalize_claude_tool_input_schema;

/// Trimmed, lowercased text for comparisons (strings.ToLower(strings.TrimSpace(x))).
pub fn norm(r: &Res<'_>) -> String {
    r.str().trim().to_lowercase()
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub fn now_nanos() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos())
}

// ---------------------------------------------------------------------------------------
// Request model (common/request.go)

/// `event: <event>\ndata: <payload>\n\n` (common.SSEEventData).
pub fn sse_event(event: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(event.len() + payload.len() + 16);
    out.extend_from_slice(b"event: ");
    out.extend_from_slice(event.as_bytes());
    out.extend_from_slice(b"\ndata: ");
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\n\n");
    out
}

/// common.RequestModelName: the first non-blank `model` or `request.model` string.
pub fn request_model_name(original: &[u8], request: &[u8]) -> Vec<u8> {
    for raw in [original, request] {
        if raw.is_empty() || !gj::valid(raw) {
            continue;
        }
        let root = gj::parse(raw);
        for path in ["model", "request.model"] {
            let model = root.get(path);
            if model.kind == Kind::String && !go_trim_space(&model.s).is_empty() {
                return model.s.to_vec();
            }
        }
    }
    vec![]
}

// ---------------------------------------------------------------------------------------
// Go string helpers on bytes

/// strings.TrimLeftFunc(s, unicode.IsSpace).
pub fn trim_left_space(s: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < s.len() {
        match gj::decode_rune(&s[start..]) {
            (Some(c), n) if c.is_whitespace() => start += n,
            _ => break,
        }
    }
    &s[start..]
}

/// Each rune of `s` as Go's `range` and `[]rune` see it: invalid bytes are U+FFFD, one
/// per byte.
pub fn go_runes(s: &[u8]) -> impl Iterator<Item = char> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        if i >= s.len() {
            return None;
        }
        let (c, n) = gj::decode_rune(&s[i..]);
        i += n.max(1);
        Some(c.unwrap_or(char::REPLACEMENT_CHARACTER))
    })
}

/// strings.ToLower: Go's simple case mapping, and invalid bytes become U+FFFD once any
/// rune is not ASCII.
pub fn go_lower(s: &[u8]) -> Vec<u8> {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    let mut out = Vec::with_capacity(s.len());
    let mut buf = [0; 4];
    for c in go_runes(s) {
        out.extend_from_slice(cpa_common::gostr::to_lower_rune(c).encode_utf8(&mut buf).as_bytes());
    }
    out
}

/// util.IsClaudeCodeAttributionSystemText: Claude Code's billing header line.
pub fn is_claude_code_attribution_text(text: &[u8]) -> bool {
    trim_left_space(text).starts_with(b"x-anthropic-billing-header:")
}

/// util.HasUnsupportedUnicodePropertyEscape: `\p{`, `\P{` or `\0` outside an escaped
/// backslash.
pub fn has_unsupported_unicode_property_escape(pattern: &[u8]) -> bool {
    let mut i = 0;
    while i < pattern.len() {
        if pattern[i] != b'\\' {
            i += 1;
            continue;
        }
        let Some(&next) = pattern.get(i + 1) else { break };
        if matches!(next, b'p' | b'P') && pattern.get(i + 2) == Some(&b'{') {
            return true;
        }
        if next == b'0' {
            return true;
        }
        i += 2;
    }
    false
}

/// util.SchemaMapKeywords: keywords whose values map names to subschemas.
pub const SCHEMA_MAP_KEYWORDS: [&[u8]; 6] = [
    b"properties",
    b"$defs",
    b"definitions",
    b"patternProperties",
    b"dependentSchemas",
    b"dependencies",
];

/// util.SchemaValueKeywords: keywords holding one subschema or a list of them.
pub const SCHEMA_VALUE_KEYWORDS: [&[u8]; 16] = [
    b"items",
    b"prefixItems",
    b"contains",
    b"additionalProperties",
    b"propertyNames",
    b"unevaluatedProperties",
    b"unevaluatedItems",
    b"additionalItems",
    b"contentSchema",
    b"anyOf",
    b"oneOf",
    b"allOf",
    b"not",
    b"if",
    b"then",
    b"else",
];

/// util.FixJSON: converts single-quoted strings to double-quoted ones. Go walks `[]rune`,
/// so invalid bytes come out as U+FFFD.
pub fn fix_json(input: &[u8]) -> Vec<u8> {
    let runes: Vec<char> = go_runes(input).collect();
    let mut out = Vec::with_capacity(input.len());
    let mut buf = [0; 4];
    let mut put = |out: &mut Vec<u8>, c: char| out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    let (mut in_double, mut in_single, mut escaped) = (false, false, false);
    let mut i = 0;
    while i < runes.len() {
        let r = runes[i];
        if in_double {
            put(&mut out, r);
            if escaped {
                escaped = false;
            } else if r == '\\' {
                escaped = true;
            } else if r == '"' {
                in_double = false;
            }
        } else if in_single {
            if escaped {
                escaped = false;
                match r {
                    'n' | 'r' | 't' | 'b' | 'f' | '/' | '"' => {
                        out.push(b'\\');
                        put(&mut out, r);
                    }
                    '\\' => out.extend_from_slice(b"\\\\"),
                    '\'' => out.push(b'\''),
                    'u' => {
                        out.extend_from_slice(b"\\u");
                        for _ in 0..4 {
                            match runes.get(i + 1) {
                                Some(&p) if p.is_ascii_hexdigit() => {
                                    put(&mut out, p);
                                    i += 1;
                                }
                                _ => break,
                            }
                        }
                    }
                    _ => {
                        out.push(b'\\');
                        put(&mut out, r);
                    }
                }
            } else if r == '\\' {
                escaped = true;
            } else if r == '\'' {
                out.push(b'"');
                in_single = false;
            } else if r == '"' {
                out.extend_from_slice(b"\\\"");
            } else {
                put(&mut out, r);
            }
        } else if r == '"' {
            in_double = true;
            put(&mut out, r);
        } else if r == '\'' {
            in_single = true;
            out.push(b'"');
        } else {
            put(&mut out, r);
        }
        i += 1;
    }
    if in_single {
        out.push(b'"');
    }
    out
}

/// util.CanonicalToolName: trimmed, leading underscores dropped, lowercased.
pub(crate) fn canonical_tool_name(name: &[u8]) -> Vec<u8> {
    let trimmed = go_trim_space(name);
    let start = trimmed.iter().position(|&c| c != b'_').unwrap_or(trimmed.len());
    go_lower(&trimmed[start..])
}

/// util.ToolNameMapFromClaudeRequest: canonical name -> declared name for the request's
/// tools, first declaration winning. `None` when the request is invalid or names none.
pub fn tool_name_map_from_claude_request(raw: &[u8]) -> Option<HashMap<Vec<u8>, Vec<u8>>> {
    if raw.is_empty() || !gj::valid(raw) {
        return None;
    }
    let tools = gj::get(raw, "tools");
    if !tools.exists() || !tools.is_array() {
        return None;
    }
    let mut out = HashMap::new();
    tools.each(|_, tool| {
        let mut name = go_trim_space(&tool.get("name").bytes()).to_vec();
        if name.is_empty() {
            name = go_trim_space(&tool.get("function.name").bytes()).to_vec();
        }
        let key = canonical_tool_name(&name);
        if !name.is_empty() && !key.is_empty() {
            out.entry(key).or_insert(name);
        }
        true
    });
    (!out.is_empty()).then_some(out)
}

/// util.MapToolName: the declared spelling of `name`, or `name` unchanged.
pub fn map_tool_name(map: Option<&HashMap<Vec<u8>, Vec<u8>>>, name: &[u8]) -> Vec<u8> {
    if let (false, Some(map)) = (name.is_empty(), map)
        && let Some(mapped) = map.get(&canonical_tool_name(name))
        && !mapped.is_empty()
    {
        return mapped.clone();
    }
    name.to_vec()
}

/// thinking.GetThinkingText on bytes: `text`, then `thinking` as a string or an object
/// holding `text` or `thinking`.
// `cpa_common::thinking::get_thinking_text` takes a UTF-8 gjson value; this keeps
// Go's byte handling for translator inputs.
pub fn thinking_text(part: &Res<'_>) -> Vec<u8> {
    let text = part.get("text");
    if text.kind == Kind::String {
        return text.s.to_vec();
    }
    let thinking = part.get("thinking");
    if thinking.kind == Kind::String {
        return thinking.s.to_vec();
    }
    if thinking.is_object() {
        for key in ["text", "thinking"] {
            let inner = thinking.get(key);
            if inner.kind == Kind::String {
                return inner.s.to_vec();
            }
        }
    }
    vec![]
}

// ---------------------------------------------------------------------------------------
// Function names and file data (util/util.go, common/file_data.go)

/// util.SanitizeFunctionName: runes outside `[a-zA-Z0-9_.:-]` (each invalid byte counts
/// as one) become `_`, a leading non-letter gets a `_` prefix, at most 64 bytes.
pub fn sanitize_function_name(name: &[u8]) -> Vec<u8> {
    if name.is_empty() {
        return vec![];
    }
    let mut s: Vec<u8> = go_runes(name)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-') {
                c as u8
            } else {
                b'_'
            }
        })
        .collect();
    if !(s[0].is_ascii_alphabetic() || s[0] == b'_') {
        s.truncate(63);
        s.insert(0, b'_');
    }
    s.truncate(64);
    s
}

/// filepath.Ext: the suffix from the last dot of the final path element.
pub(crate) fn file_ext(name: &[u8]) -> &[u8] {
    for i in (0..name.len()).rev() {
        match name[i] {
            b'/' => break,
            b'.' => return &name[i..],
            _ => {}
        }
    }
    b""
}

/// common.NormalizeOpenAIFileData: the MIME type and base64 payload of OpenAI file
/// content, from a `data:` URL or (for raw base64) the filename's extension.
pub fn normalize_openai_file_data(filename: &[u8], fallback: &[u8], data: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if data.is_empty() {
        return None;
    }
    let mut fallback = fallback.to_vec();
    if fallback.is_empty() {
        let ext = file_ext(filename);
        let ext = go_lower(ext.strip_prefix(b".").unwrap_or(ext));
        fallback = crate::mime::mime_type(&ext).unwrap_or_default().as_bytes().to_vec();
    }
    if data.len() < 5 || !data[..5].eq_ignore_ascii_case(b"data:") {
        return (!fallback.is_empty()).then(|| (fallback, data.to_vec()));
    }
    let rest = &data[5..];
    let comma = rest.iter().position(|&c| c == b',')?;
    let (metadata, payload) = (&rest[..comma], &rest[comma + 1..]);
    if payload.is_empty() {
        return None;
    }
    let mut fields = metadata.split(|&c| c == b';');
    let mime = go_trim_space(fields.next().unwrap_or_default());
    if mime.is_empty() {
        return None;
    }
    use cpa_common::gostr::GoStr;
    fields
        .any(|f| String::from_utf8_lossy(go_trim_space(f)).go_eq_fold("base64"))
        .then(|| (mime.to_vec(), payload.to_vec()))
}

/// strings.ToUpper (Go's simple case mapping; invalid bytes become U+FFFD once any rune
/// is not ASCII).
pub fn go_upper(s: &[u8]) -> Vec<u8> {
    if s.is_ascii() {
        return s.to_ascii_uppercase();
    }
    let mut out = Vec::with_capacity(s.len());
    let mut buf = [0; 4];
    for c in go_runes(s) {
        out.extend_from_slice(cpa_common::gostr::to_upper_rune(c).encode_utf8(&mut buf).as_bytes());
    }
    out
}

/// util.SanitizedToolNameMap: sanitized name -> declared name for request tools (top-level
/// `name`) whose names need sanitizing; the first declaration wins.
pub fn sanitized_tool_name_map(raw: &[u8]) -> Option<HashMap<Vec<u8>, Vec<u8>>> {
    if raw.is_empty() || !gj::valid(raw) {
        return None;
    }
    let tools = gj::get(raw, "tools");
    if !tools.is_array() {
        return None;
    }
    let mut out = HashMap::new();
    tools.each(|_, tool| {
        let name = go_trim_space(&tool.get("name").bytes()).to_vec();
        if !name.is_empty() {
            let sanitized = sanitize_function_name(&name);
            if sanitized != name {
                out.entry(sanitized).or_insert(name);
            }
        }
        true
    });
    (!out.is_empty()).then_some(out)
}

/// util.RestoreSanitizedToolName.
pub fn restore_sanitized_tool_name(map: Option<&HashMap<Vec<u8>, Vec<u8>>>, name: &[u8]) -> Vec<u8> {
    map.and_then(|m| m.get(name))
        .filter(|_| !name.is_empty())
        .cloned()
        .unwrap_or_else(|| name.to_vec())
}

/// `time.Unix(secs, 0).Format(time.RFC3339Nano)` in UTC: `YYYY-MM-DDThh:mm:ssZ`.
// ponytail: Go formats in the process's local zone; this assumes UTC, as Go runs in its
// container image. Years beyond four digits print in full, as Go does. Format in the
// local zone if a server runs outside UTC.
pub fn format_rfc3339_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days (proleptic Gregorian, like Go).
    let z = days as i128 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i128::from(month <= 2);
    let year = if year < 0 {
        format!("-{:04}", -year)
    } else {
        format!("{year:04}")
    };
    format!(
        "{year}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// `time.Parse(time.RFC3339Nano, s).Unix()`: `YYYY-MM-DDThh:mm:ss[.frac]` with `Z` or
/// `±hh:mm`. The hour may have one digit and the fraction may use a comma, as Go's general
/// layout parser allows.
pub fn parse_rfc3339_unix(s: &[u8]) -> Option<i64> {
    fn num(s: &[u8], at: &mut usize, min: usize, max: usize) -> Option<i64> {
        let start = *at;
        while *at < s.len() && *at - start < max && s[*at].is_ascii_digit() {
            *at += 1;
        }
        if *at - start < min {
            return None;
        }
        std::str::from_utf8(&s[start..*at]).ok()?.parse().ok()
    }
    fn lit(s: &[u8], at: &mut usize, c: u8) -> Option<()> {
        (s.get(*at) == Some(&c)).then(|| *at += 1)
    }
    let mut at = 0;
    let year = num(s, &mut at, 4, 4)?;
    lit(s, &mut at, b'-')?;
    let month = num(s, &mut at, 2, 2)?;
    lit(s, &mut at, b'-')?;
    let day = num(s, &mut at, 2, 2)?;
    lit(s, &mut at, b'T')?;
    let hour = num(s, &mut at, 1, 2)?;
    lit(s, &mut at, b':')?;
    let min = num(s, &mut at, 2, 2)?;
    lit(s, &mut at, b':')?;
    let sec = num(s, &mut at, 2, 2)?;
    if matches!(s.get(at), Some(b'.' | b',')) && s.get(at + 1).is_some_and(u8::is_ascii_digit) {
        at += 1;
        while s.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
    }
    let offset = match s.get(at) {
        Some(b'Z') => {
            at += 1;
            0
        }
        Some(&sign @ (b'+' | b'-')) => {
            at += 1;
            let hh = num(s, &mut at, 2, 2)?;
            lit(s, &mut at, b':')?;
            let mm = num(s, &mut at, 2, 2)?;
            // Go's fast path stops at 23; its general layout parser accepts 24.
            if hh > 24 || mm > 59 {
                return None;
            }
            let off = (hh * 60 + mm) * 60;
            if sign == b'-' { -off } else { off }
        }
        _ => return None,
    };
    if at != s.len() || !(1..=12).contains(&month) || hour > 23 || min > 59 || sec > 59 {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][month as usize - 1];
    if day < 1 || day > days_in {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hour * 3600 + min * 60 + sec - offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_formats_like_go_in_utc() {
        assert_eq!(format_rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339_utc(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(format_rfc3339_utc(-1), "1969-12-31T23:59:59Z");
        assert_eq!(format_rfc3339_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_rfc3339_utc(253_402_300_800), "10000-01-01T00:00:00Z");
        assert_eq!(format_rfc3339_utc(-62_167_219_201), "-0001-12-31T23:59:59Z");
    }

    #[test]
    fn rfc3339_matches_go_unix_seconds() {
        assert_eq!(parse_rfc3339_unix(b"1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_unix(b"2025-01-01T00:00:00.123456789123Z"),
            Some(1735689600)
        );
        assert_eq!(parse_rfc3339_unix(b"2024-02-29T23:59:59+01:30"), Some(1709245799));
        assert_eq!(parse_rfc3339_unix(b"2025-01-01T00:00:00+24:00"), Some(1735603200));
        assert_eq!(parse_rfc3339_unix(b"2025-01-01T00:00:00.Z"), None);
        assert_eq!(parse_rfc3339_unix(b"0001-01-01T00:00:00Z"), Some(-62135596800));
        assert_eq!(parse_rfc3339_unix(b"2023-02-29T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_unix(b"2025-01-01T1:02:03,5-00:00"), Some(1735693323));
        assert_eq!(parse_rfc3339_unix(b"2025-01-01 00:00:00Z"), None);
        assert_eq!(parse_rfc3339_unix(b"2025-01-01T00:00:00"), None);
    }

    #[test]
    fn sanitize_counts_runes_not_bytes() {
        assert_eq!(sanitize_claude_function_name("é.b\u{ff}".as_bytes()), b"__b_");
        assert_eq!(sanitize_claude_function_name(b"\xff\xfe"), b"__");
        assert_eq!(sanitize_claude_function_name(&[b'a'; 70]).len(), 64);
        let id = sanitize_claude_tool_id(b"");
        assert!(id.starts_with(b"toolu_"));
    }
}
