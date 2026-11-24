//! Values a translator generates (clock readings, random and counter-based IDs) replaced
//! with placeholders, so an output compares equal across runs. Each distinct generated
//! value gets a numbered placeholder by first appearance, so a value used twice stays
//! recognizably the same.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;

/// How far a number may be from the run's clock and still count as a clock reading.
const CLOCK_WINDOW_SECS: u128 = 24 * 3600;

/// A JSON string body: what an ID or timestamp in the output lives in.
static JSON_STRING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""((?:[^"\\]|\\.)*)""#).unwrap());
static DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[0-9]{10,}").unwrap());
/// Hexadecimal clock readings, as in `call_<hex nanos>_<n>`.
static HEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[0-9a-f]{12,}").unwrap());
/// An ID field holding `<tool name>-<counter>`, from a process-wide counter
/// (`gemini_claude_response.rs`, `antigravity_claude_response.rs`).
static COUNTER_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""(?:id|call_id|tool_use_id|tool_call_id)":"([A-Za-z_][A-Za-z0-9_.]*-[0-9]+)""#).unwrap()
});
static RFC3339: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]+)?(Z|[+-][0-9]{2}:[0-9]{2})$").unwrap()
});
/// `toolu_` plus 24 random alphanumerics (`generate_claude_tool_call_id`).
static RANDOM_TOOLU: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^toolu_[A-Za-z0-9]{24}$").unwrap());
/// A bare JSON number field holding a clock reading in seconds.
static CLOCK_FIELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""(created|created_at|createdAt|completed_at|timestamp)":([0-9]{9,})"#).unwrap());

fn now_nanos() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
}

/// Whether `digits` read as seconds, milliseconds, microseconds or nanoseconds since the
/// epoch lands within [`CLOCK_WINDOW_SECS`] of now.
fn is_clock(digits: &str) -> bool {
    let Ok(value) = digits.parse::<u128>() else {
        return false;
    };
    let now = now_nanos();
    let window = CLOCK_WINDOW_SECS * 1_000_000_000;
    [1_000_000_000u128, 1_000_000, 1_000, 1]
        .iter()
        .any(|scale| value.checked_mul(*scale).is_some_and(|ns| ns.abs_diff(now) <= window))
}

fn is_rfc3339_now(s: &str) -> bool {
    if !RFC3339.is_match(s) {
        return false;
    }
    // Year, month and day against today's: a fixture date never matches the run's clock.
    let days = (now_nanos() / 1_000_000_000 / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    let today = format!("{y:04}-{m:02}-{d:02}");
    let (y, m, d) = civil_from_days(days - 1);
    let yesterday = format!("{y:04}-{m:02}-{d:02}");
    s.starts_with(&today) || s.starts_with(&yesterday)
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Whether a JSON string's content is a generated value.
fn is_generated(content: &str) -> bool {
    RANDOM_TOOLU.is_match(content)
        || DIGITS.find_iter(content).any(|m| is_clock(m.as_str()))
        || HEX
            .find_iter(content)
            .any(|m| u128::from_str_radix(m.as_str(), 16).is_ok_and(|v| is_clock(&v.to_string())))
}

/// `out` with every generated value replaced. Clock-reading numbers become `<time>`; a
/// JSON string that is a clock reading becomes `"<time>"`; any other JSON string holding a
/// generated value becomes `"<id:N>"`.
pub fn normalize(out: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(out);
    let text = CLOCK_FIELD.replace_all(&text, |c: &regex::Captures<'_>| {
        if is_clock(&c[2]) {
            format!(r#""{}":"<time>""#, &c[1])
        } else {
            c[0].to_string()
        }
    });
    let counters: HashSet<String> = COUNTER_ID.captures_iter(&text).map(|c| c[1].to_string()).collect();
    let mut ids: HashMap<String, usize> = HashMap::new();
    let text = JSON_STRING.replace_all(&text, |c: &regex::Captures<'_>| {
        let content = &c[1];
        if is_rfc3339_now(content) {
            return r#""<time>""#.to_string();
        }
        if !is_generated(content) && !counters.contains(content) {
            return c[0].to_string();
        }
        let next = ids.len() + 1;
        let n = *ids.entry(content.to_string()).or_insert(next);
        format!(r#""<id:{n}>""#)
    });
    text.into_owned().into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(s: &str) -> String {
        String::from_utf8(normalize(s.as_bytes())).unwrap()
    }

    fn now_secs() -> u128 {
        now_nanos() / 1_000_000_000
    }

    #[test]
    fn a_clock_number_field_becomes_time() {
        let s = format!(r#"{{"created":{},"n":1700000000}}"#, now_secs());
        assert_eq!(norm(&s), r#"{"created":"<time>","n":1700000000}"#);
    }

    #[test]
    fn a_fixture_number_is_kept() {
        assert_eq!(norm(r#"{"created":1700000000}"#), r#"{"created":1700000000}"#);
    }

    #[test]
    fn ids_built_on_the_clock_get_numbered_placeholders() {
        let n = now_nanos();
        let s = format!(r#"["call_{n}_1","Bash-{n}-2","call_{n}_1","step_{n}"]"#);
        assert_eq!(norm(&s), r#"["<id:1>","<id:2>","<id:1>","<id:3>"]"#);
    }

    #[test]
    fn hex_clock_and_counter_ids_get_placeholders() {
        let hex = format!("{:x}", now_nanos());
        let s =
            format!(r#"{{"call_id":"call_{hex}_9","id":"get_weather-2","text":"get_weather-2","name":"get_weather"}}"#);
        assert_eq!(
            norm(&s),
            r#"{"call_id":"<id:1>","id":"<id:2>","text":"<id:2>","name":"get_weather"}"#
        );
    }

    #[test]
    fn a_random_toolu_id_is_replaced_and_a_fixture_one_kept() {
        assert_eq!(
            norm(r#"["toolu_aB3dE5fG7hI9jK1lM3nO5pQ7","toolu_fixture_call_1"]"#),
            r#"["<id:1>","toolu_fixture_call_1"]"#
        );
    }

    #[test]
    fn a_clock_timestamp_string_becomes_time() {
        let days = (now_secs() / 86_400) as i64;
        let (y, m, d) = civil_from_days(days);
        let s = format!(r#"{{"created":"{y:04}-{m:02}-{d:02}T10:11:12.5Z","fixed":"2024-01-02T03:04:05Z"}}"#);
        assert_eq!(norm(&s), r#"{"created":"<time>","fixed":"2024-01-02T03:04:05Z"}"#);
    }

    #[test]
    fn civil_dates_match_known_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }
}
