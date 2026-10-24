use crate::claude_usage::timestamp;
use serde_json::Value;
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Reset {
    pub at: u64,
    claim: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Recovery {
    Unknown,
    NotLimited,
    Limited(Reset),
}

impl Recovery {
    pub(super) fn intersect(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::NotLimited, state) | (state, Self::NotLimited) => state,
            (Self::Limited(a), Self::Limited(b)) => Self::Limited(a.max(b)),
        }
    }

    pub(super) fn reset(self) -> Option<Reset> {
        match self {
            Self::Limited(reset) => Some(reset),
            Self::Unknown | Self::NotLimited => None,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Rejection {
    reset: Option<Reset>,
    retry_at: Option<u64>,
}

impl Rejection {
    pub(super) fn new(reset: Option<Reset>, delay: Option<u64>, at: u64) -> Self {
        let retry_at = delay.map(|d| at.saturating_add(d));
        Self {
            reset: reset.map(|mut reset| {
                reset.at = reset.at.max(retry_at.unwrap_or(0));
                reset
            }),
            retry_at,
        }
    }

    pub(super) fn earliest(self, other: Self) -> Self {
        let deadline = |r: Self| r.reset.map(|r| r.at).or(r.retry_at).unwrap_or(u64::MAX);
        if deadline(self) <= deadline(other) { self } else { other }
    }

    pub(super) fn response(self, at: u64) -> axum::response::Response {
        let mut response = super::error(
            429,
            "No account has confirmed available quota under the proxy policy; retry after reset",
        );
        retry_headers(response.headers_mut(), self.reset, at);
        if !response.headers().contains_key("retry-after")
            && let Some(deadline) = self.retry_at.filter(|deadline| *deadline > at)
            && let Ok(value) = deadline.saturating_sub(at).div_ceil(1000).to_string().parse()
        {
            response.headers_mut().insert("retry-after", value);
        }
        response
    }
}
pub(super) fn recovery(q: &Value, model: &str, at: u64) -> Recovery {
    let mut state = Recovery::NotLimited;
    for (key, claim, required) in [
        ("five_hour", "five_hour", true),
        ("seven_day", "seven_day", true),
        ("seven_day_oauth_apps", "seven_day", false),
        ("seven_day_opus", "seven_day_opus", false),
        ("seven_day_sonnet", "seven_day_sonnet", false),
    ] {
        let v = &q[key];
        if key.contains("opus") && !model.contains("opus") || key.contains("sonnet") && !model.contains("sonnet") {
            continue;
        }
        if v.is_null() && !required {
            continue;
        }
        let Some(used) = v["utilization"].as_f64().filter(|u| u.is_finite() && *u >= 0.) else {
            return Recovery::Unknown;
        };
        if used >= 100. {
            let Some(deadline) = timestamp(&v["resets_at"]).filter(|r| *r > at) else {
                return Recovery::Unknown;
            };
            state = state.intersect(Recovery::Limited(Reset { at: deadline, claim }));
        }
    }
    state
}
pub(super) fn header_recovery(signals: &std::collections::BTreeMap<String, String>, at: u64) -> Recovery {
    let get = |name: &str| {
        signals
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim())
    };
    let mut state = Recovery::NotLimited;
    for (label, claim) in [("5h", "five_hour"), ("7d", "seven_day")] {
        let prefix = format!("anthropic-ratelimit-unified-{label}-");
        let exhausted = get(&format!("{prefix}status")).is_some_and(|s| s.eq_ignore_ascii_case("rejected"))
            || get(&format!("{prefix}utilization"))
                .and_then(|s| s.parse::<f64>().ok())
                .is_some_and(|n| n.is_finite() && n >= 1.);
        if exhausted {
            let Some(raw) = get(&format!("{prefix}reset")) else {
                return Recovery::Unknown;
            };
            let Some(deadline) = raw
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite() && *n > 0. && *n < u64::MAX as f64 / 1000.)
                .map(|s| (s * 1000.).ceil() as u64)
                .or_else(|| crate::quota_time::rfc3339(raw).and_then(crate::quota_time::epoch_millis))
            else {
                return Recovery::Unknown;
            };
            if deadline > at {
                state = state.intersect(Recovery::Limited(Reset { at: deadline, claim }));
            }
        }
    }
    state
}
pub(super) fn extra_available(q: &Value) -> bool {
    let extra = &q["extra_usage"];
    extra["is_enabled"] == true
        && matches!(
            (extra["monthly_limit"].as_f64(), extra["used_credits"].as_f64()),
            (Some(limit), Some(used)) if limit.is_finite() && used.is_finite() && used >= 0. && limit > used
        )
}
pub(super) fn retry_headers(headers: &mut axum::http::HeaderMap, reset: Option<Reset>, at: u64) {
    let Some(reset) = reset.filter(|r| r.at > at) else {
        return;
    };
    for (key, value) in [
        ("retry-after", reset.at.saturating_sub(at).div_ceil(1000).to_string()),
        ("anthropic-ratelimit-unified-reset", reset.at.div_ceil(1000).to_string()),
        ("anthropic-ratelimit-unified-representative-claim", reset.claim.into()),
        ("anthropic-ratelimit-unified-status", "rejected".into()),
        ("anthropic-ratelimit-unified-overage-status", "rejected".into()),
    ] {
        headers.insert(key, value.parse().unwrap());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_recovery_cannot_be_hidden_by_another_observation() {
        let at = 2_000_000_000_000;
        let q =
            json!({"five_hour":{"utilization":100,"resets_at":"2033-05-18T04:33:20Z"},"seven_day":{"utilization":10}});
        let rest = recovery(&q, "opus", at);
        let mut signals =
            std::collections::BTreeMap::from([("anthropic-ratelimit-unified-7d-status".into(), "rejected".into())]);
        assert_eq!(rest.intersect(header_recovery(&signals, at)), Recovery::Unknown);
        signals.insert("anthropic-ratelimit-unified-7d-reset".into(), "2000086400".into());
        let headers = header_recovery(&signals, at);
        assert_eq!(rest.intersect(headers).reset().unwrap().at, at + 86400000);
        assert_eq!(headers.intersect(recovery(&Value::Null, "opus", at)), Recovery::Unknown);
        assert_eq!(rest.intersect(header_recovery(&Default::default(), at)), rest);
    }

    #[test]
    fn upstream_recovery_waits_for_cooldown_and_uses_earliest_available_account() {
        let at = 2_000_000_000_000;
        let reset = Reset {
            at: at + 30000,
            claim: "five_hour",
        };
        let first = Rejection::new(Some(reset), Some(120000), at);
        let response = first.response(at);
        assert_eq!(response.status(), 429);
        assert_eq!(response.headers()["retry-after"], "120");
        assert_eq!(response.headers()["anthropic-ratelimit-unified-reset"], "2000000120");
        let fallback = Rejection::new(None, Some(60000), at);
        assert_eq!(first.earliest(fallback).response(at).headers()["retry-after"], "60");
        assert!(
            !fallback
                .response(at)
                .headers()
                .contains_key("anthropic-ratelimit-unified-reset")
        );
        assert!(!fallback.response(at + 60000).headers().contains_key("retry-after"));
        assert_eq!(
            Rejection::new(Some(reset), Some(60000), at).response(at).headers()["retry-after"],
            "60"
        );
    }

    #[test]
    fn retry_uses_latest_blocking_window_and_rejects_unknown_resets() {
        let at = 2_000_000_000_000;
        let mut q = json!({"five_hour":{"utilization":100,"resets_at":"2033-05-18T04:33:20Z"},"seven_day":{"utilization":100,"resets_at":"2033-05-19T03:33:20Z"}});
        let r = recovery(&q, "opus", at).reset().unwrap();
        assert_eq!(r.at, at + 86400000);
        assert_eq!(r.claim, "seven_day");
        let mut headers = axum::http::HeaderMap::new();
        retry_headers(&mut headers, Some(r), at);
        assert_eq!(headers["retry-after"], "86400");
        assert_eq!(headers["anthropic-ratelimit-unified-reset"], "2000086400");
        q["seven_day"]["resets_at"] = Value::Null;
        assert!(recovery(&q, "opus", at) == Recovery::Unknown);
        q["seven_day"]["utilization"] = json!(10);
        assert_eq!(recovery(&q, "opus", at).reset().unwrap().at, at + 3600000);
        q["five_hour"]["utilization"] = json!(10);
        assert_eq!(recovery(&q, "opus", at), Recovery::NotLimited);
        assert_eq!(recovery(&Value::Null, "opus", at), Recovery::Unknown);
    }
    #[test]
    fn reset_reads_canonical_response_headers_and_expires() {
        let signals = std::collections::BTreeMap::from([
            ("Anthropic-Ratelimit-Unified-5h-Status".into(), "rejected".into()),
            ("Anthropic-Ratelimit-Unified-5h-Reset".into(), "2000000120".into()),
        ]);
        let reset = header_recovery(&signals, 2_000_000_000_000).reset().unwrap();
        assert_eq!(
            reset,
            Reset {
                at: 2_000_000_120_000,
                claim: "five_hour"
            }
        );
        assert_eq!(header_recovery(&signals, reset.at), Recovery::NotLimited);
    }
    #[test]
    fn paid_fallback_requires_confirmed_available_credit() {
        for extra in [
            json!(null),
            json!({"is_enabled":false,"monthly_limit":100,"used_credits":0}),
            json!({"is_enabled":true}),
            json!({"is_enabled":true,"monthly_limit":100,"used_credits":100}),
        ] {
            assert!(!extra_available(&json!({"extra_usage":extra})));
        }
        assert!(extra_available(
            &json!({"extra_usage":{"is_enabled":true,"monthly_limit":100,"used_credits":99}})
        ));
    }
}
