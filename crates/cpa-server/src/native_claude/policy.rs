use super::{Account, routing::RoutingState};
use serde_json::Value;
use std::collections::HashSet;

pub(super) fn retry_delay(value: Option<&str>, at: u64) -> Option<u64> {
    let value = value?.trim();
    let ms = value.parse::<u64>().ok().map(|n| n.saturating_mul(1000)).or_else(|| {
        chrono::DateTime::parse_from_rfc2822(value)
            .ok()
            .and_then(|date| u64::try_from(date.timestamp_millis()).ok())
            .map(|date| date.saturating_sub(at))
    })?;
    Some(ms.clamp(1000, 15 * 60 * 1000))
}
pub(super) fn headroom(q: &Value, model: &str, minimum: f64) -> bool {
    q.as_object().is_some_and(|q| {
        q.iter()
            .filter(|(k, v)| {
                !v.is_null()
                    && (k.as_str() == "five_hour"
                        || k.as_str() == "seven_day"
                        || k.as_str() == "seven_day_oauth_apps"
                        || k.contains("sonnet") && model.contains("sonnet")
                        || k.contains("opus") && model.contains("opus"))
            })
            .all(|(_, v)| {
                v["utilization"]
                    .as_f64()
                    .is_some_and(|n| n.is_finite() && n <= 100.0 - minimum)
            })
    })
}
pub(super) fn proposed<'a>(
    sid: &str,
    pool: &'a [Account],
    routing: &RoutingState,
    tried: &HashSet<String>,
    at: u64,
    model: &str,
    minimum: f64,
) -> Option<&'a Account> {
    let first = choose_account(sid, pool, routing, tried, at)?;
    if routing.affinity.get(sid) == Some(&first.id) || headroom(&first.quota, model, minimum) {
        return Some(first);
    }
    pool.iter()
        .filter(|a| {
            a.enabled
                && !tried.contains(&a.id)
                && routing.cooldowns.account_until(&a.id) <= at
                && headroom(&a.quota, model, minimum)
        })
        .min_by_key(|a| weekly_reset(&a.quota, at).unwrap_or(u64::MAX))
        .or(Some(first))
}
// Rank only fresh, future weekly resets. Missing dates cannot win selection.
pub(super) fn weekly_reset(q: &Value, at: u64) -> Option<u64> {
    q["seven_day"]["resets_at"]
        .as_str()
        .and_then(crate::quota_time::rfc3339)
        .and_then(crate::quota_time::epoch_millis)
        .filter(|reset| *reset > at)
}
pub(super) fn choose_account<'a>(
    sid: &str,
    pool: &'a [Account],
    routing: &RoutingState,
    tried: &HashSet<String>,
    at: u64,
) -> Option<&'a Account> {
    let usable: Vec<_> = pool
        .iter()
        .filter(|a| a.enabled && !tried.contains(&a.id) && routing.cooldowns.account_until(&a.id) <= at)
        .collect();
    usable
        .iter()
        .find(|a| routing.affinity.get(sid) == Some(&a.id))
        .copied()
        .or_else(|| {
            usable
                .into_iter()
                .min_by_key(|a| weekly_reset(&a.quota, at).unwrap_or(u64::MAX))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn retry_after_is_bounded_and_supports_http_date() {
        assert_eq!(retry_delay(Some("120"), 0), Some(120000));
        assert_eq!(retry_delay(Some("999999999999999"), 0), Some(900000));
        assert_eq!(retry_delay(Some("-1"), 0), None);
        assert_eq!(
            retry_delay(Some("Wed, 21 Oct 2015 07:28:00 GMT"), 1445412470000),
            Some(10000)
        );
    }
    #[test]
    fn new_sessions_prefer_headroom_but_pinned_and_only_accounts_remain_usable() {
        let make = |id: &str, used: f64, reset: &str| Account {
            id: id.into(),
            token: String::new(),
            enabled: true,
            file_id: String::new(),
            name: String::new(),
            disabled: false,
            expired: false,
            quota: json!({"five_hour":{"utilization":used},"seven_day":{"utilization":used,"resets_at":reset}}),
        };
        let pool = vec![
            make("low", 98., "2033-05-18T04:33:20Z"),
            make("high", 20., "2033-05-19T04:33:20Z"),
        ];
        let mut routing = RoutingState::default();
        let tried = HashSet::new();
        let at = 2_000_000_000_000;
        assert_eq!(
            proposed("s", &pool, &routing, &tried, at, "opus", 5.).unwrap().id,
            "high"
        );
        routing
            .affinity
            .bind("s".into(), "low".into(), at, &HashSet::new())
            .unwrap();
        assert_eq!(
            proposed("s", &pool, &routing, &tried, at, "opus", 5.).unwrap().id,
            "low"
        );
        assert_eq!(
            proposed("new", &pool[..1], &routing, &tried, at, "opus", 5.)
                .unwrap()
                .id,
            "low"
        );
    }
}
