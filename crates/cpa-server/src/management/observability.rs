//! `GET /server/latest-version`, `GET /observability/usage/api-keys` and
//! `GET /observability/usage/queue` (Go config_basic.go, api_key_usage.go, usage.go).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Map, Value, json};

use super::Management;
use super::auth_files::{Query, auth_kind, fail, recent_requests, reply};

pub(crate) async fn latest_version() -> Response {
    reply(
        StatusCode::NOT_IMPLEMENTED,
        [
            ("error", "source_build_only".into()),
            ("message", "Build and update this personal fork from source.".into()),
        ],
    )
}

/// Go `GetAPIKeyUsage`: API-key credentials grouped by provider (the OpenAI-compatible
/// name when set) and keyed by `base_url|api_key`, with summed counters.
pub(crate) async fn api_key_usage(State(state): State<Arc<Management>>) -> Response {
    let store = state.rt.store();
    let mut out: BTreeMap<String, BTreeMap<String, (u64, u64, Value)>> = BTreeMap::new();
    for c in store.snapshot().iter() {
        if auth_kind(c) != Some("apikey") {
            continue;
        }
        let Some(key) = c.attributes.get("api_key").map(|k| k.trim()).filter(|k| !k.is_empty()) else {
            continue;
        };
        let base = ["base_url", "base-url"]
            .iter()
            .filter_map(|k| c.attributes.get(*k))
            .map(|v| v.trim())
            .find(|v| !v.is_empty())
            .unwrap_or_default();
        let provider = c
            .attributes
            .get("compat_name")
            .map(|n| n.trim())
            .filter(|n| !n.is_empty())
            .unwrap_or(c.provider.trim())
            .to_lowercase();
        let provider = if provider.is_empty() {
            "unknown".to_owned()
        } else {
            provider
        };
        let activity = store.activity(&c.id);
        let recent = recent_requests(&activity);
        let entry = out.entry(provider).or_default().entry(format!("{base}|{key}"));
        match entry {
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert((activity.success, activity.failed, recent));
            }
            std::collections::btree_map::Entry::Occupied(mut o) => {
                let (s, f, buckets) = o.get_mut();
                *s += activity.success;
                *f += activity.failed;
                merge_buckets(buckets, &recent);
            }
        }
    }
    let body: Map<String, Value> = out
        .into_iter()
        .map(|(provider, keys)| {
            let keys: Map<String, Value> = keys
                .into_iter()
                .map(|(k, (s, f, r))| (k, json!({"success": s, "failed": f, "recent_requests": r})))
                .collect();
            (provider, Value::Object(keys))
        })
        .collect();
    // apiKeyUsageEntry and RecentRequestBucket structs in sorted maps.
    super::json_ordered(StatusCode::OK, &Value::Object(body))
}

/// Go `mergeRecentRequestBuckets`: counts add up index by index.
fn merge_buckets(dst: &mut Value, src: &Value) {
    let (Some(dst), Some(src)) = (dst.as_array_mut(), src.as_array()) else {
        return;
    };
    for (d, s) in dst.iter_mut().zip(src) {
        for key in ["success", "failed"] {
            let sum = d[key].as_u64().unwrap_or(0) + s[key].as_u64().unwrap_or(0);
            d[key] = sum.into();
        }
    }
}

/// Go `GetUsageQueue`: pops up to `count` (default 1) queued usage records. A record
/// that is valid JSON is embedded as is, anything else as a JSON string.
pub(crate) async fn usage_queue(State(state): State<Arc<Management>>, RawQuery(raw): RawQuery) -> Response {
    let q = Query::parse(raw);
    let value = q.first("count").trim();
    let count = if value.is_empty() {
        1
    } else {
        match value.parse::<i64>() {
            Ok(n) if n > 0 => usize::try_from(n).unwrap_or(usize::MAX),
            _ => return fail(StatusCode::BAD_REQUEST, "count must be a positive integer"),
        }
    };
    let records: Vec<Value> = state
        .rt
        .usage_queue()
        .pop_oldest(count)
        .into_iter()
        .map(|payload| {
            serde_json::from_slice::<Value>(&payload)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&payload).into_owned()))
        })
        .collect();
    // Records are embedded as stored (usageQueueRecord.MarshalJSON).
    super::json_ordered(StatusCode::OK, &Value::Array(records))
}

pub(crate) async fn native_status(State(state): State<Arc<Management>>) -> Response {
    let native = state
        .rt
        .native_state
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .upgrade();
    let Some(native) = native else {
        return super::json_ordered(StatusCode::OK, &json!({"enabled":false}));
    };
    match native.status().await {
        Ok(status) => super::json_ordered(StatusCode::OK, &status),
        Err(_) => fail(StatusCode::SERVICE_UNAVAILABLE, "Native routing status unavailable"),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NativePolicy {
    allow_extra_usage: bool,
}

pub(crate) async fn native_policy(
    State(state): State<Arc<Management>>,
    axum::Json(policy): axum::Json<NativePolicy>,
) -> Response {
    let native = state
        .rt
        .native_state
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .upgrade();
    let Some(native) = native else {
        return fail(StatusCode::CONFLICT, "Native routing is not enabled on this server");
    };
    match native.set_overage(policy.allow_extra_usage).await {
        Ok(()) => super::json_ordered(StatusCode::OK, &json!({"allowExtraUsage":policy.allow_extra_usage})),
        Err(_) => fail(StatusCode::INTERNAL_SERVER_ERROR, "Native policy could not be saved"),
    }
}

pub(crate) async fn native_events(State(state): State<Arc<Management>>) -> Response {
    super::json_ordered(StatusCode::OK, &state.rt.native_events.snapshot())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn release_checks_are_local_and_unsupported() {
        let response = latest_version().await;
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body = axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["error"],
            "source_build_only"
        );
    }

    #[test]
    fn same_base_and_key_sum_bucket_by_bucket() {
        let mut dst = json!([{"time": "a", "success": 1, "failed": 0}, {"time": "b", "success": 0, "failed": 2}]);
        let src = json!([{"time": "a", "success": 3, "failed": 1}, {"time": "b", "success": 0, "failed": 5}]);
        merge_buckets(&mut dst, &src);
        assert_eq!(
            dst,
            json!([{"time": "a", "success": 4, "failed": 1}, {"time": "b", "success": 0, "failed": 7}])
        );
    }
}
