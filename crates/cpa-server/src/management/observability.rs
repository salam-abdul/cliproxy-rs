//! `GET /observability/usage/api-keys` and `GET /observability/usage/queue` (Go
//! api_key_usage.go, usage.go).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Query as AxumQuery, RawQuery, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Map, Value, json};

use super::Management;
use super::auth_files::{Query, auth_kind, fail, recent_requests};

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

pub(crate) async fn native_status(
    State(state): State<Arc<Management>>,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let model = query.get("model").map(String::as_str).unwrap_or("claude");
    if model.is_empty() || model.len() > 200 || model.chars().any(char::is_control) {
        return fail(StatusCode::BAD_REQUEST, "Invalid model");
    }
    let native = state
        .rt
        .native_state
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .upgrade();
    let Some(native) = native else {
        return super::json_ordered(StatusCode::OK, &json!({"enabled":false}));
    };
    match native.status_for(model).await {
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

fn default_history_range() -> String {
    "24h".to_owned()
}

#[derive(serde::Deserialize)]
pub(crate) struct ActivityParams {
    #[serde(default = "default_history_range")]
    range: String,
    provider: Option<String>,
    credential: Option<String>,
}

/// A filter is absent, or a non-empty value the store could hold.
fn check(filter: Option<&str>) -> Result<(), &'static str> {
    match filter {
        Some(value) if value.is_empty() || value.len() > 256 => Err("Invalid provider or credential filter"),
        _ => Ok(()),
    }
}

pub(crate) async fn activity(
    State(state): State<Arc<Management>>,
    AxumQuery(params): AxumQuery<ActivityParams>,
) -> Response {
    let Some(range) = crate::activity::Range::parse(&params.range) else {
        return fail(StatusCode::BAD_REQUEST, "Range must be 24h, 7d, or 30d");
    };
    if let Err(message) = check(params.provider.as_deref()).and(check(params.credential.as_deref())) {
        return fail(StatusCode::BAD_REQUEST, message);
    }
    let Some(activity) = state.rt.activity() else {
        return fail(StatusCode::CONFLICT, "Activity store is not running on this server");
    };
    match activity.panels(range, params.provider, params.credential).await {
        Ok(result) => super::json_ordered(StatusCode::OK, &result),
        Err(_) => fail(StatusCode::INTERNAL_SERVER_ERROR, "Activity could not be read"),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct HistoryParams {
    #[serde(default = "default_history_range")]
    range: String,
    provider: Option<String>,
}

pub(crate) async fn history(
    State(state): State<Arc<Management>>,
    AxumQuery(params): AxumQuery<HistoryParams>,
) -> Response {
    let Some(range) = crate::activity::Range::parse(&params.range) else {
        return fail(StatusCode::BAD_REQUEST, "Range must be 24h, 7d, or 30d");
    };
    if let Err(message) = check(params.provider.as_deref()) {
        return fail(StatusCode::BAD_REQUEST, message);
    }
    let Some(activity) = state.rt.activity() else {
        return fail(StatusCode::CONFLICT, "Activity store is not running on this server");
    };
    match activity.history(range, params.provider).await {
        Ok(result) => super::json_ordered(StatusCode::OK, &result),
        Err(_) => fail(StatusCode::INTERNAL_SERVER_ERROR, "History could not be read"),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct SpeedParams {
    #[serde(default = "default_history_range")]
    range: String,
    provider: Option<String>,
    credential: Option<String>,
    /// Taken as text so a value that is not a whole number gets this route's own refusal.
    offset: Option<String>,
}

/// The viewer's minutes east of UTC: absent is 0, else a whole number from -840 to 840.
fn offset(value: Option<&str>) -> Result<i32, &'static str> {
    match value {
        None => Ok(0),
        Some(value) => value
            .parse::<i32>()
            .ok()
            .filter(|minutes| (-840..=840).contains(minutes))
            .ok_or("Offset must be whole minutes between -840 and 840"),
    }
}

pub(crate) async fn speed(State(state): State<Arc<Management>>, AxumQuery(params): AxumQuery<SpeedParams>) -> Response {
    let Some(range) = crate::activity::Range::parse(&params.range) else {
        return fail(StatusCode::BAD_REQUEST, "Range must be 24h, 7d, or 30d");
    };
    if let Err(message) = check(params.provider.as_deref()).and(check(params.credential.as_deref())) {
        return fail(StatusCode::BAD_REQUEST, message);
    }
    let offset = match offset(params.offset.as_deref()) {
        Ok(offset) => offset,
        Err(message) => return fail(StatusCode::BAD_REQUEST, message),
    };
    let Some(activity) = state.rt.activity() else {
        return fail(StatusCode::CONFLICT, "Activity store is not running on this server");
    };
    match activity.speed(range, params.provider, params.credential, offset).await {
        Ok(result) => super::json_ordered(StatusCode::OK, &result),
        Err(_) => fail(StatusCode::INTERNAL_SERVER_ERROR, "Speed could not be read"),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct ToolsParams {
    #[serde(default = "default_history_range")]
    range: String,
    provider: Option<String>,
}

pub(crate) async fn tools(State(state): State<Arc<Management>>, AxumQuery(params): AxumQuery<ToolsParams>) -> Response {
    let Some(range) = crate::activity::Range::parse(&params.range) else {
        return fail(StatusCode::BAD_REQUEST, "Range must be 24h, 7d, or 30d");
    };
    if let Err(message) = check(params.provider.as_deref()) {
        return fail(StatusCode::BAD_REQUEST, message);
    }
    let settings = crate::request_logging::history::settings(&state.rt.config());
    let Some(settings) = settings.as_ref() else {
        return fail(StatusCode::CONFLICT, "Body history is off");
    };
    state.rt.history().indexer().nudge(settings);
    let dir = settings.dir.clone();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let read = tokio::task::spawn_blocking(move || {
        crate::request_logging::history::tools::read::read(&dir, range, params.provider.as_deref(), now_ms)
    })
    .await;
    match read {
        Ok(Ok(result)) => super::json_ordered(StatusCode::OK, &result),
        _ => fail(StatusCode::INTERNAL_SERVER_ERROR, "Tools could not be read"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A management router over a runtime with body history on in `dir/history`.
    fn history_router(dir: &std::path::Path) -> axum::Router {
        let hash = bcrypt::hash("synthetic-management", 4).unwrap();
        let cfg = cpa_core::config::Config::parse(&format!(
            "oauth:\n  auth-dir: {}\nmanagement:\n  secret-key: '{hash}'\nobservability:\n  history:\n    enable: true\n    dir: {}\n",
            dir.join("auth").display(),
            dir.join("history").display()
        ))
        .unwrap();
        let rt = crate::testing::runtime(
            cfg,
            vec![],
            cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        );
        super::super::router(Management::new(Arc::new(rt), dir.join("config.yaml")))
    }

    async fn get_tools(router: axum::Router) -> (StatusCode, Value) {
        use tower::ServiceExt;
        let request = axum::http::Request::builder()
            .uri("/v8/management/observability/tools?range=24h")
            .header("authorization", "Bearer synthetic-management")
            .extension(axum::extract::ConnectInfo(
                "127.0.0.1:40000".parse::<std::net::SocketAddr>().unwrap(),
            ))
            .body(axum::body::Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn tools_route_reads_a_populated_index() {
        use crate::request_logging::history::tools::scan::{Call, Conv, Def, SendFacts};
        use crate::request_logging::history::tools::store::{Fingerprint, Index};
        let dir = std::env::temp_dir().join(format!("cpa-tools-route-{}", uuid::Uuid::new_v4()));
        let partition = dir.join("history/date=2026-11-24");
        std::fs::create_dir_all(&partition).unwrap();
        let file = partition.join("080000-1-0000.parquet");
        std::fs::write(&file, b"synthetic").unwrap();
        let mut index = Index::open(&dir.join("history")).unwrap();
        let send = SendFacts {
            at_ms: chrono::Utc::now().timestamp_millis() - 60_000,
            provider: Some("claude".into()),
            conv: Conv::Start(1),
            message: None,
            defs: vec![Def {
                tool: "Read".into(),
                bytes: 40,
            }],
            calls: vec![Call {
                id: 1,
                tool: "Read".into(),
                skill: None,
            }],
            results: Vec::new(),
            skipped_format: false,
            cut: false,
        };
        let read_as = Fingerprint::from_path(&file).unwrap();
        assert!(
            index
                .commit("date=2026-11-24/080000-1-0000.parquet", &read_as, &file, &[send], false)
                .unwrap()
        );
        drop(index);
        let (status, body) = get_tools(history_router(&dir)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["sends"], 1);
        assert_eq!(body["conversations"], 1);
        assert_eq!(body["pendingFiles"], 0);
        assert_eq!(body["tools"][0]["name"], "Read");
        assert_eq!(body["tools"][0]["used"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tools_route_answers_500_when_the_index_cannot_be_read() {
        let dir = std::env::temp_dir().join(format!("cpa-tools-route-{}", uuid::Uuid::new_v4()));
        let index = dir
            .join("history")
            .join(crate::request_logging::history::tools::store::INDEX_FILE);
        std::fs::create_dir_all(&index).unwrap();
        let (status, body) = get_tools(history_router(&dir)).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"], "Tools could not be read");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offset_is_whole_minutes_within_fourteen_hours() {
        assert_eq!(offset(None), Ok(0));
        for (value, minutes) in [("-840", -840), ("0", 0), ("840", 840)] {
            assert_eq!(offset(Some(value)), Ok(minutes), "{value}");
        }
        for value in ["-841", "841", "1.5", "x", ""] {
            assert_eq!(
                offset(Some(value)),
                Err("Offset must be whole minutes between -840 and 840"),
                "{value}"
            );
        }
    }

    #[test]
    fn activity_filters_must_be_non_empty_and_bounded() {
        assert!(check(Some("")).is_err());
        assert!(check(Some(&"a".repeat(257))).is_err());
        assert!(check(None).is_ok());
        assert!(check(Some("codex")).is_ok());
        assert!(check(Some(&"a".repeat(256))).is_ok());
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
