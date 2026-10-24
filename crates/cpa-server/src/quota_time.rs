use std::time::{Duration, SystemTime};

pub(crate) fn rfc3339(raw: &str) -> Option<SystemTime> {
    chrono::DateTime::parse_from_rfc3339(raw).ok().map(SystemTime::from)
}

pub(crate) fn reset_time(raw: &str) -> Option<SystemTime> {
    let raw = raw.trim();
    if let Ok(seconds) = raw.parse::<f64>() {
        return (seconds.is_finite() && seconds > 0.0)
            .then(|| Duration::try_from_secs_f64(seconds).ok())
            .flatten()
            .and_then(|d| SystemTime::UNIX_EPOCH.checked_add(d));
    }
    rfc3339(raw)
}

pub(crate) fn epoch_millis(time: SystemTime) -> Option<u64> {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_formats_preserve_fractional_seconds_and_offsets() {
        let expected = SystemTime::UNIX_EPOCH + Duration::from_millis(2_000_000_000_125);
        for raw in [
            " 2000000000.125 ",
            "2033-05-18T03:33:20.125Z",
            "2033-05-17T22:33:20.125-05:00",
        ] {
            assert_eq!(reset_time(raw), Some(expected));
        }
        for raw in ["NaN", "inf", "-1", "0", "1e300", "invalid"] {
            assert_eq!(reset_time(raw), None);
        }
        assert_eq!(epoch_millis(rfc3339("1969-12-31T23:59:59Z").unwrap()), None);
        assert_eq!(rfc3339("2000000000"), None);
    }
}
