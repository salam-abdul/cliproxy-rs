//! Go `time.Time` text.

use std::fmt::Display;

use chrono::{DateTime, Offset, TimeZone, Timelike};

/// Go `time.Time.Format(time.RFC3339Nano)` and `MarshalJSON`: nanoseconds with trailing
/// zeros trimmed (no dot when zero), `Z` for UTC, else `±hh:mm`.
pub fn rfc3339_nano<Tz: TimeZone>(t: &DateTime<Tz>) -> String
where
    Tz::Offset: Display,
{
    let mut out = t.format("%Y-%m-%dT%H:%M:%S").to_string();
    // chrono reports a leap second as nanoseconds past one billion.
    let nanos = t.nanosecond() % 1_000_000_000;
    if nanos != 0 {
        let fraction = format!("{nanos:09}");
        out.push('.');
        out.push_str(fraction.trim_end_matches('0'));
    }
    let offset = t.offset().fix().local_minus_utc();
    if offset == 0 {
        out.push('Z');
    } else {
        let sign = if offset < 0 { '-' } else { '+' };
        let abs = offset.abs();
        out.push_str(&format!("{sign}{:02}:{:02}", abs / 3600, (abs % 3600) / 60));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::rfc3339_nano;
    use chrono::{DateTime, FixedOffset, Local, TimeZone, Utc};

    #[test]
    fn rfc3339_nano_trims_like_go() {
        let t = |s: &str| DateTime::parse_from_rfc3339(s).unwrap();
        let utc = |s: &str| t(s).with_timezone(&Utc);
        assert_eq!(
            rfc3339_nano(&utc("2026-07-21T12:00:00.120000000Z")),
            "2026-07-21T12:00:00.12Z"
        );
        assert_eq!(
            rfc3339_nano(&utc("2026-07-21T12:00:00.000000001Z")),
            "2026-07-21T12:00:00.000000001Z"
        );
        assert_eq!(rfc3339_nano(&utc("2026-07-21T12:00:00Z")), "2026-07-21T12:00:00Z");
        assert_eq!(
            rfc3339_nano(&t("2026-10-03T13:30:00.123456+05:30")),
            "2026-10-03T13:30:00.123456+05:30"
        );
        assert_eq!(
            rfc3339_nano(&t("2026-10-03T04:05:06-07:00")),
            "2026-10-03T04:05:06-07:00"
        );
        let zero_offset = FixedOffset::east_opt(0)
            .unwrap()
            .timestamp_opt(1_700_000_000, 0)
            .unwrap();
        assert_eq!(rfc3339_nano(&zero_offset), "2023-11-14T22:13:20Z");
        let local = utc("2026-10-03T04:05:06.12001Z").with_timezone(&Local);
        assert!(rfc3339_nano(&local).contains(".12001"));
    }
}
