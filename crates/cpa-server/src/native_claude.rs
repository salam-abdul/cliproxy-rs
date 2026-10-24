//! Opt-in native Claude CONNECT transport. Unlike the API translator, inference
//! bodies and feature headers are never shaped; only the inference credential changes.
pub(crate) mod diagnostics;
mod observation;
mod policy;
mod quota;
mod routing;
#[cfg(test)]
use observation::UsageScanner;
use observation::{Exchange, Observation};
use policy::choose_account;
#[cfg(test)]
use policy::weekly_reset;
use routing::RoutingState;

use crate::Runtime;
use anyhow::Context;
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode},
    response::Response,
};
use base64::Engine;
use btls::ssl::{Ssl, SslAcceptor, SslFiletype, SslMethod};
use futures_util::{Stream, StreamExt};
use hyper::{body::Incoming, service::service_fn};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    port: u16,
    #[serde(default)]
    http_proxy_port: u16,
    database: PathBuf,
    events: PathBuf,
    proxy_secret: PathBuf,
    key: PathBuf,
    cert: PathBuf,
    #[serde(default)]
    auth_dir: PathBuf,
    #[serde(default)]
    settings_file: PathBuf,
    #[serde(default)]
    qualification: bool,
    #[serde(default = "body_limit")]
    max_body_bytes: usize,
    #[serde(default = "minimum_remaining")]
    new_session_min_remaining: f64,
    #[serde(default)]
    selection_shadow: bool,
    #[serde(default = "max_attempts")]
    max_attempts: usize,
    // Test fixtures cannot use a real provider destination or native auth directory.
    fixture_accounts: Option<Vec<Account>>,
    fixture_quota: Option<Value>,
    upstream: Option<Target>,
}
fn minimum_remaining() -> f64 {
    5.0
}
fn max_attempts() -> usize {
    3
}
fn body_limit() -> usize {
    16 * 1024 * 1024
}
#[derive(Clone, Deserialize)]
struct Target {
    hostname: String,
    port: u16,
    ca: Option<PathBuf>,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Account {
    id: String,
    token: String,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    file_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    expired: bool,
    #[serde(default)]
    quota: Value,
}
impl Account {
    fn quota_id(&self) -> &str {
        if self.file_id.is_empty() {
            &self.id
        } else {
            &self.file_id
        }
    }
}
#[derive(Clone, Copy)]
enum QuotaTier {
    Included,
    Extra,
}
fn yes() -> bool {
    true
}
pub(crate) struct State {
    config: Config,
    events: diagnostics::Writer,
    rt: Arc<Runtime>,
    client: wreq::Client,
    tls: std::sync::RwLock<Arc<SslAcceptor>>,
    secret: String,
    routing: Mutex<RoutingState>,
    quota: quota::Cache,
    sessions: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}
pub struct Running(Vec<JoinHandle<()>>, Option<std::fs::File>);
impl Drop for Running {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
        let _ = self.1.take();
    }
}
fn hash(s: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(s.as_ref()))
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && id.bytes().all(|c| c.is_ascii_alphanumeric() || b"._:-".contains(&c))
}
fn session(body: &Value, headers: &HeaderMap) -> Option<String> {
    let m = &body["metadata"];
    let mut id = m["session_id"].as_str().or(m["sessionId"].as_str()).map(str::to_owned);
    if id.is_none()
        && let Some(raw) = m["user_id"].as_str().filter(|s| s.len() <= 4096)
    {
        id = serde_json::from_str::<Value>(raw)
            .ok()
            .and_then(|v| v["session_id"].as_str().map(str::to_owned))
            .or_else(|| raw.rsplit_once("_session_").map(|(_, s)| s.to_owned()));
    }
    id.or_else(|| {
        headers
            .get("x-claude-code-session-id")
            .and_then(|h| h.to_str().ok())
            .map(str::to_owned)
    })
    .filter(|id| valid_id(id))
    .map(hash)
}
fn available(v: &Value) -> bool {
    v["utilization"]
        .as_f64()
        .is_some_and(|n| n.is_finite() && (0.0..100.0).contains(&n))
}
fn eligible(q: &Value, model: &str) -> bool {
    if !available(&q["five_hour"]) || !available(&q["seven_day"]) {
        return false;
    }
    q.as_object().is_some_and(|q| {
        q.iter().all(|(k, v)| {
            v.is_null()
                || !(k.contains("sonnet") && model.contains("sonnet")
                    || k.contains("opus") && model.contains("opus")
                    || k == "seven_day_oauth_apps")
                || available(v)
        })
    })
}
fn headers(input: &HeaderMap, token: Option<&str>) -> HeaderMap {
    let nominated: HashSet<_> = input
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(','))
        .map(|s| s.trim().to_ascii_lowercase())
        .collect();
    let mut out = HeaderMap::new();
    for (k, v) in input {
        if [
            "connection",
            "proxy-connection",
            "keep-alive",
            "transfer-encoding",
            "te",
            "trailer",
            "upgrade",
            "proxy-authorization",
            "proxy-authenticate",
        ]
        .contains(&k.as_str())
            || nominated.contains(k.as_str())
            || token.is_some() && ["authorization", "x-api-key"].contains(&k.as_str())
        {
            continue;
        }
        out.append(k.clone(), v.clone());
    }
    if let Some(token) = token
        && let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}"))
    {
        out.insert("authorization", value);
    }
    out
}
fn error(status: u16, message: &str) -> Response {
    Response::builder().status(status).header("content-type","application/json").body(Body::from(json!({"type":"error","error":{"type":if status==429 {"rate_limit_error"}else{"api_error"},"message":message}}).to_string())).unwrap()
}
fn private(path: &Path) -> anyhow::Result<Vec<u8>> {
    let m = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        m.is_file() && !m.file_type().is_symlink(),
        "Private regular file required"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        anyhow::ensure!(m.permissions().mode() & 0o077 == 0, "Private file permissions required");
    }
    Ok(std::fs::read(path)?)
}
impl State {
    pub(crate) async fn set_overage(self: &Arc<Self>, allow: bool) -> anyhow::Result<()> {
        let s = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut routing = s.routing.lock().unwrap();
            let mut next = routing.clone();
            next.policy.allow_extra_usage = allow;
            next.persist(&s.config.database)?;
            *routing = next;
            Ok(())
        })
        .await?
    }
    fn allow_extra_usage(&self) -> bool {
        self.routing.lock().unwrap().policy.allow_extra_usage
    }
    fn reset(&self, a: &Account, model: &str) -> Option<quota::Reset> {
        let at = now();
        let mut recovery = quota::recovery(&a.quota, model, at);
        if let Some(snapshot) = self.rt.executors.claude.quota().snapshot(a.quota_id()) {
            recovery = recovery.intersect(quota::header_recovery(&snapshot.signals, at));
        }
        recovery.reset().map(|mut reset| {
            let routing = self.routing.lock().unwrap();
            reset.at = reset.at.max(routing.cooldowns.until(&a.id, model));
            reset
        })
    }
    fn event(&self, mut v: Value) {
        v["at"] = json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        self.events.send(&v, &self.rt.native_events.lost);
        self.rt.native_events.record(v);
    }

    fn accounts(&self) -> anyhow::Result<Vec<Account>> {
        if let Some(a) = &self.config.fixture_accounts {
            return Ok(a.clone());
        }
        let settings: Value = if self.config.settings_file.is_file() {
            serde_json::from_slice(&private(&self.config.settings_file)?)?
        } else {
            json!({})
        };
        let mut all = Vec::new();
        for c in self
            .rt
            .store()
            .snapshot()
            .into_iter()
            .filter(|c| c.provider == "claude")
        {
            let cpa_core::credential::Source::File(ref path) = c.source else {
                continue;
            };
            let root = std::fs::canonicalize(&self.config.auth_dir)?;
            let file = std::fs::canonicalize(path)?;
            anyhow::ensure!(file.starts_with(&root), "Credential outside auth directory");
            let a: Value = serde_json::from_slice(&private(path)?)?;
            anyhow::ensure!(a["type"] == "claude", "Credential provider mismatch");
            let fallback = hash(format!(
                "claude:{}",
                a["organization_uuid"]
                    .as_str()
                    .or(a["account_uuid"].as_str())
                    .unwrap_or(&c.id)
            ));
            let id = a["native_route_id"].as_str().unwrap_or(&fallback).to_owned();
            anyhow::ensure!(valid_id(&id), "Invalid account identity");
            let expired = a["expired"]
                .as_str()
                .or(a["expires_at"].as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .is_none_or(|d| d.timestamp_millis() <= now() as i64 + 30000);
            let token = a["access_token"].as_str().unwrap_or_default().to_owned();
            let disabled = c.disabled || a["disabled"].as_bool() == Some(true);
            let name = settings["names"][&id]
                .as_str()
                .or(a["native_route_name"].as_str())
                .unwrap_or(&c.label)
                .to_owned();
            all.push(Account {
                id,
                token: token.clone(),
                enabled: !disabled && !expired && !token.is_empty(),
                file_id: c.id.clone(),
                name,
                disabled,
                expired,
                quota: Value::Null,
            });
        }
        anyhow::ensure!(
            all.iter().map(|a| &a.id).collect::<HashSet<_>>().len() == all.len(),
            "Duplicate account identity"
        );
        let preferred = settings["preferredAccount"].as_str();
        all.sort_by_key(|a| Some(a.id.as_str()) != preferred);
        Ok(all)
    }
    fn included_available(&self, a: &Account, q: &Value, model: &str) -> bool {
        if !eligible(q, model) {
            return false;
        }
        let wall = std::time::SystemTime::now();
        self.rt
            .executors
            .claude
            .quota()
            .snapshot(a.quota_id())
            .is_none_or(|snapshot| {
                !crate::scheduler::Windows::observed("claude", &snapshot.signals, snapshot.observed_at, wall).exhausted
            })
    }

    async fn quota(&self, a: &Account) -> anyhow::Result<Value> {
        if self.config.fixture_accounts.is_some() {
            return Ok(a.quota.clone());
        }
        if let Some(q) = &self.config.fixture_quota {
            return Ok(q.clone());
        }
        let key = hash(format!("{}:{}", a.id, a.token));
        self.quota
            .get(key, async {
                let r = self
                    .client
                    .get("https://api.anthropic.com/api/oauth/usage")
                    .bearer_auth(&a.token)
                    .header("anthropic-beta", "oauth-2025-04-20")
                    .header("user-agent", "claude-code/2.1.296")
                    .timeout(Duration::from_secs(15))
                    .send()
                    .await?;
                anyhow::ensure!(
                    r.status() == StatusCode::OK,
                    "Included quota availability not established"
                );
                let mut bytes = Vec::new();
                let mut stream = r.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk?;
                    anyhow::ensure!(chunk.len() <= 1048576 - bytes.len(), "Usage response too large");
                    bytes.extend_from_slice(&chunk);
                }
                let q: Value = serde_json::from_slice(&bytes)?;
                Ok(q)
            })
            .await
    }

    async fn quota_accounts(&self, accounts: Vec<Account>) -> Vec<Account> {
        futures_util::stream::iter(accounts)
            .map(|mut a| async {
                if a.enabled {
                    a.quota = self.quota(&a).await.unwrap_or(Value::Null);
                }
                a
            })
            .buffered(8)
            .collect()
            .await
    }
    async fn select(
        self: &Arc<Self>,
        sid: &str,
        pool: Vec<Account>,
        tried: &HashSet<String>,
        model: &str,
    ) -> anyhow::Result<Option<Account>> {
        let s = self.clone();
        let sid = sid.to_owned();
        let tried = tried.clone();
        let model = model.to_owned();
        tokio::task::spawn_blocking(move || {
            let active: HashSet<_> = s
                .sessions
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, lock)| Arc::strong_count(lock) > 1)
                .map(|(sid, _)| sid.clone())
                .collect();
            let mut routing = s.routing.lock().unwrap();
            let at = now();
            let mut next = routing.clone();
            let mut protected = active.clone();
            protected.remove(&sid);
            next.prune(at, &protected);
            let pool: Vec<_> = pool
                .into_iter()
                .filter(|a| next.cooldowns.model_until(&a.id, &model) <= at)
                .collect();
            let baseline = choose_account(&sid, &pool, &next, &tried, at);
            let proposed = policy::proposed(
                &sid,
                &pool,
                &next,
                &tried,
                at,
                &model,
                s.config.new_session_min_remaining,
            );
            let selected = if s.config.selection_shadow { baseline } else { proposed }.cloned();
            let reason = match selected.as_ref() {
                None => "no_eligible_account",
                Some(a) if next.affinity.get(&sid) == Some(&a.id) => "session_affinity",
                Some(a) if baseline.is_some_and(|b| b.id != a.id) => "quota_headroom",
                _ => "soonest_reset",
            };
            s.event(json!({
                "type":"selection", "session":sid, "model":model,
                "account":selected.as_ref().map(|a| &a.id), "proposedAccount":proposed.map(|a| &a.id),
                "shadow":s.config.selection_shadow, "reason":reason
            }));
            if let Some(a) = &selected {
                let checkpoint = next.affinity.bind(sid, a.id.clone(), at, &active)?;
                if checkpoint || next.affinity.len() != routing.affinity.len() {
                    next.persist(&s.config.database)?;
                }
            }
            *routing = next;
            Ok(selected)
        })
        .await?
    }

    async fn admit(
        self: &Arc<Self>,
        sid: &str,
        model: &str,
        tried: &mut HashSet<String>,
    ) -> anyhow::Result<(Option<Account>, Option<quota::Reset>)> {
        loop {
            if tried.len() >= self.config.max_attempts {
                return Ok((None, None));
            }
            let accounts = self
                .accounts()?
                .into_iter()
                .filter(|a| a.enabled && !tried.contains(&a.id))
                .collect();
            let accounts = self.quota_accounts(accounts).await;
            let retry = accounts.iter().filter_map(|a| self.reset(a, model)).min();
            let (included, extra): (Vec<_>, Vec<_>) = accounts
                .into_iter()
                .partition(|a| self.included_available(a, &a.quota, model));
            let mut tier = QuotaTier::Included;
            let mut selected = self.select(sid, included, tried, model).await?;
            if selected.is_none() && self.allow_extra_usage() {
                let extra = extra.into_iter().filter(|a| quota::extra_available(&a.quota)).collect();
                tier = QuotaTier::Extra;
                selected = self.select(sid, extra, tried, model).await?;
            }
            let Some(a) = selected else {
                return Ok((None, retry));
            };
            tried.insert(a.id.clone());
            let Some(mut current) = self
                .accounts()?
                .into_iter()
                .find(|current| current.id == a.id && current.enabled)
            else {
                continue;
            };
            if let Ok(q) = self.quota(&current).await {
                let admitted = match tier {
                    QuotaTier::Included => self.included_available(&current, &q, model),
                    QuotaTier::Extra => self.allow_extra_usage() && quota::extra_available(&q),
                };
                if admitted {
                    current.quota = q;
                    return Ok((Some(current), None));
                }
            }
        }
    }
    async fn cool(self: &Arc<Self>, a: &Account, model: &str, status: u16, delay: Option<u64>) -> anyhow::Result<()> {
        let s = self.clone();
        let account = a.id.clone();
        let model = model.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut routing = s.routing.lock().unwrap();
            let mut next = routing.clone();
            next.cooldowns.cool(&account, &model, status, delay, now());
            next.persist(&s.config.database)?;
            *routing = next;
            Ok(())
        })
        .await?
    }
    pub(crate) async fn status(self: &Arc<Self>) -> anyhow::Result<Value> {
        let mut all = Vec::new();
        let allow_extra = self.allow_extra_usage();
        let mut ready = false;
        for a in self.quota_accounts(self.accounts()?).await {
            ready |= a.enabled
                && (self.included_available(&a, &a.quota, "claude") || allow_extra && quota::extra_available(&a.quota));
            let q = (!a.quota.is_null()).then_some(&a.quota);
            all.push(json!({"id":a.id,"fileId":a.file_id,"name":a.name,"enabled":a.enabled,"disabled":a.disabled,"expired":a.expired,"includedQuota":q.map(|q|json!({"fiveHour":q["five_hour"]["utilization"],"fiveHourResetsAt":q["five_hour"]["resets_at"],"weekly":q["seven_day"]["utilization"],"weeklyResetsAt":q["seven_day"]["resets_at"],"eligible":self.included_available(&a,q,"claude")}))}));
        }
        let ready = !self.config.qualification && ready;
        let state = self.clone();
        let counts = tokio::task::spawn_blocking(move || {
            let mut counts = BTreeMap::new();
            for id in state.routing.lock().unwrap().affinity.accounts() {
                *counts.entry(id.clone()).or_insert(0) += 1;
            }
            counts
        })
        .await?;
        let snapshot = self.rt.native_events.snapshot();
        let recent: Vec<_> = snapshot["events"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|e| e["type"] == "inference")
            .take(10)
            .cloned()
            .collect();
        Ok(
            json!({"enabled":true,"allowExtraUsage":allow_extra,"selectionStrategy":"soonest-reset","ready":ready,"ownerReady":true,"proxyReady":true,"qualification":self.config.qualification,"accounts":all,"affinity":counts.into_iter().map(|(a,n)|json!({"account":a,"sessions":n})).collect::<Vec<_>>(),"recent":recent}),
        )
    }
}
async fn handle(s: Arc<State>, req: Request<Incoming>, diagnostics: bool) -> Response {
    match handle_result(s, req, diagnostics).await {
        Ok(r) => r,
        Err(_) => error(
            502,
            "Native router could not complete request; no direct inference fallback",
        ),
    }
}
async fn handle_result(s: Arc<State>, req: Request<Incoming>, diagnostics: bool) -> anyhow::Result<Response> {
    let request_started = Instant::now();
    let request_id = uuid::Uuid::new_v4().to_string();
    let (parts, body) = req.into_parts();
    let path = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if diagnostics {
        if path == "/health" {
            let ready = s.status().await.is_ok_and(|v| v["ready"] == true);
            return Ok(Response::builder()
                .status(if ready { 200 } else { 503 })
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"ready":ready,"qualification":s.config.qualification}).to_string(),
                ))?);
        }
        if path == "/status" {
            if !constant(
                parts
                    .headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or(""),
                &format!("Bearer {}", s.secret),
            ) {
                return Ok(error(401, "Local management credential required"));
            }
            return Ok(Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(s.status().await?.to_string()))?);
        }
        return Ok(error(400, "Use CONNECT proxy protocol"));
    }
    let infer = parts.method == Method::POST && parts.uri.path() == "/v1/messages";
    if !infer && s.config.upstream.is_some() {
        return Ok(error(400, "Fixture cannot contact native services"));
    }
    let raw = match axum::body::to_bytes(Body::new(body), s.config.max_body_bytes).await {
        Ok(b) => b,
        Err(_) => return Ok(error(413, "Native request exceeds body limit")),
    };
    let target = s
        .config
        .upstream
        .as_ref()
        .map(|t| format!("https://{}:{}", t.hostname, t.port))
        .unwrap_or_else(|| "https://api.anthropic.com".into());
    if !infer {
        let r = s
            .client
            .request(parts.method, format!("{target}{path}"))
            .headers(headers(&parts.headers, None))
            .body(raw)
            .send()
            .await?;
        let status = r.status();
        let h = headers(r.headers(), None);
        let mut response = Response::new(Body::from_stream(r.bytes_stream()));
        *response.status_mut() = status;
        *response.headers_mut() = h;
        return Ok(response);
    }
    if s.config.qualification {
        return Ok(error(503, "Native router is staged; inference disabled"));
    }
    let body: Value = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(_) => return Ok(error(400, "Invalid native JSON")),
    };
    let Some(sid) = session(&body, &parts.headers) else {
        return Ok(error(
            400,
            "Native Claude session identity required; request was not routed",
        ));
    };
    let Some(model) = body["model"]
        .as_str()
        .filter(|m| m.starts_with("claude-") && m.len() <= 256)
    else {
        return Ok(error(400, "Native Claude model required; aliases are not rewritten"));
    };
    let model = model.to_owned();
    let lock = {
        let mut sessions = s.sessions.lock().unwrap();
        sessions.retain(|_, lock| Arc::strong_count(lock) > 1);
        sessions.entry(sid.clone()).or_default().clone()
    };
    let preparation_ms = request_started.elapsed().as_millis();
    let waiting = Instant::now();
    let guard = lock.lock_owned().await;
    let session_wait_ms = waiting.elapsed().as_millis();
    let mut admission_ms = 0u128;
    let mut upstream_headers_ms = 0u128;
    let admission_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut tried = HashSet::new();
    let mut rejection: Option<quota::Rejection> = None;
    loop {
        if tried.len() >= s.config.max_attempts {
            if let Some(rejection) = rejection {
                s.event(json!({"type":"rejected","requestId":request_id,"session":sid,"model":model,"outcome":"quota_unavailable","status":429,"attempts":tried.len()}));
                return Ok(rejection.response(now()));
            }
            s.event(json!({"type":"rejected","requestId":request_id,"session":sid,"model":model,"outcome":"attempt_limit","status":503,"attempts":tried.len()}));
            return Ok(error(503, "Native routing attempt limit reached; retry later"));
        }
        let admission_started = Instant::now();
        let admitted = tokio::time::timeout_at(admission_deadline, s.admit(&sid, &model, &mut tried)).await;
        admission_ms += admission_started.elapsed().as_millis();
        let a = match admitted {
            Ok(Ok((Some(a), _))) => a,
            other => {
                if let Ok(Ok((None, reset))) = &other
                    && let Some(rejection) = rejection
                {
                    let rejection = reset.map_or(rejection, |reset| {
                        rejection.earliest(quota::Rejection::new(Some(reset), None, now()))
                    });
                    s.event(json!({"type":"rejected","requestId":request_id,"session":sid,"model":model,"outcome":"quota_unavailable","status":429,"attempts":tried.len()}));
                    return Ok(rejection.response(now()));
                }
                let reset = match &other {
                    Ok(Ok((None, reset))) => *reset,
                    _ => None,
                };
                let (status, outcome, message) = match other {
                    Ok(Ok((None, None))) if !tried.is_empty() => (
                        503,
                        "routing_unavailable",
                        "No remaining account is available for this model; retry later",
                    ),
                    Ok(Ok((None, _))) => (
                        429,
                        "quota_unavailable",
                        "No account has confirmed available quota under the proxy policy; retry after reset",
                    ),
                    Err(_) => (
                        503,
                        "admission_timeout",
                        "Native quota admission timed out; retry later",
                    ),
                    _ => (
                        502,
                        "routing_error",
                        "Native routing state unavailable; no request sent",
                    ),
                };
                s.event(json!({"type":"rejected","requestId":request_id,"session":sid,"model":model,"outcome":outcome,"status":status,"attempts":tried.len(),"timing":{"admissionMs":admission_ms}}));
                let mut response = error(status, message);
                if status == 429 {
                    quota::retry_headers(response.headers_mut(), reset, now());
                }
                return Ok(response);
            }
        };
        let started = Instant::now();
        let result = s
            .client
            .request(parts.method.clone(), format!("{target}{path}"))
            .headers(headers(&parts.headers, Some(&a.token)))
            .body(raw.clone())
            .read_timeout(Duration::from_secs(600))
            .send()
            .await;
        let r = match result {
            Ok(r) => r,
            Err(upstream_error) => {
                s.cool(&a, &model, 0, None).await?;
                s.rt.store().note_activity(&a.file_id, false);
                // Log only classifications, never error Display strings containing URLs or credentials.
                let failure_kind = if upstream_error.is_timeout() {
                    "timeout"
                } else if upstream_error.is_connect() {
                    "connect"
                } else if upstream_error.is_connection_reset() {
                    "connection-reset"
                } else {
                    "transport"
                };
                s.event(json!({"type":"transport-failure","requestId":request_id,"model":model,"outcome":"transport_lost","session":sid,"account":a.id,"replayed":false,"failureKind":failure_kind,"elapsedMs":started.elapsed().as_millis()}));
                return Ok(error(
                    422,
                    "Upstream response was lost; request was not replayed to avoid duplicate inference. Check the session before explicitly retrying.",
                ));
            }
        };
        upstream_headers_ms += started.elapsed().as_millis();
        let status = r.status();
        s.rt.executors
            .claude
            .quota()
            .observe_model(a.quota_id(), &model, r.headers());
        if [401, 403, 429, 503, 529].contains(&status.as_u16()) {
            let delay = policy::retry_delay(r.headers().get("retry-after").and_then(|v| v.to_str().ok()), now());
            if status == StatusCode::TOO_MANY_REQUESTS {
                let next = quota::Rejection::new(s.reset(&a, &model), Some(delay.unwrap_or(60000)), now());
                rejection = Some(rejection.map_or(next, |previous| previous.earliest(next)));
            }
            s.cool(&a, &model, status.as_u16(), delay).await?;
            s.quota.invalidate(&hash(format!("{}:{}", a.id, a.token))).await;
            s.event(json!({"type":"failover","requestId":request_id,"model":model,"session":sid,"account":a.id,"status":status.as_u16(),"outcome":"retryable_rejection","attempt":tried.len(),"retryAfterMs":delay}));
            continue;
        }
        let h = headers(r.headers(), None);
        let encoding = h
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let facts = Exchange {
            request_id,
            sse: h.get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|v| {
                v.split(';')
                    .next()
                    .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
            }),
            timing: json!({"preparationMs":preparation_ms,"sessionWaitMs":session_wait_ms,"admissionMs":admission_ms,"upstreamHeadersMs":upstream_headers_ms}),
            headers_at: Instant::now(),
            attempts: tried.len(),
            a,
            model,
            sid,
            status: status.as_u16(),
            digest: hash(&raw),
            body_bytes: raw.len(),
            started: request_started,
        };
        let record = Observation::new(s.clone(), facts, &encoding);
        let stream: std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, wreq::Error>> + Send>> =
            Box::pin(r.bytes_stream());
        let stream = futures_util::stream::unfold(
            (stream, Some(record), guard),
            |(mut stream, mut record, guard)| async move {
                match stream.next().await {
                    Some(item) => {
                        if let Some(o) = record.as_mut()
                            && let Ok(b) = &item
                        {
                            o.observe(b).await;
                        }
                        if item.is_err()
                            && let Some(o) = record.take()
                        {
                            o.finish("upstream_stream_error").await;
                        }
                        Some((item, (stream, record, guard)))
                    }
                    None => {
                        if let Some(o) = record.take() {
                            o.finish("complete").await;
                        }
                        None
                    }
                }
            },
        );
        let mut response = Response::new(Body::from_stream(stream));
        *response.status_mut() = status;
        *response.headers_mut() = h;
        return Ok(response);
    }
}
fn constant(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.as_bytes().ct_eq(b.as_bytes()).into()
}
fn connection<I>(
    io: I,
    s: Arc<State>,
    diagnostics: bool,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    Box::pin(async move {
        let service = service_fn(move |mut req: Request<Incoming>| {
            let s = s.clone();
            async move {
                if req.method() != Method::CONNECT {
                    return Ok::<_, std::convert::Infallible>(handle(s, req, diagnostics).await);
                }
                let expected = format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode(format!("claude:{}", s.secret))
                );
                if !constant(
                    req.headers()
                        .get("proxy-authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or(""),
                    &expected,
                ) {
                    return Ok(Response::builder()
                        .status(407)
                        .header("proxy-authenticate", "Basic realm=\"claude-local\"")
                        .header("connection", "close")
                        .header("proxy-connection", "close")
                        .body(Body::empty())
                        .unwrap());
                }
                let authority = req.uri().authority().cloned();
                let Some(a) = authority.filter(|a| a.port_u16().unwrap_or(443) == 443) else {
                    return Ok(error(400, "Only HTTPS CONNECT on port443 is supported"));
                };
                let host = a.host().to_owned();
                let upgraded = hyper::upgrade::on(&mut req);
                tokio::spawn(async move {
                    if let Ok(io) = upgraded.await {
                        let mut io = TokioIo::new(io);
                        if host == "api.anthropic.com" {
                            if let Ok(ssl) = new_ssl(&s)
                                && let Ok(mut tls) = tokio_btls::SslStream::new(ssl, io)
                                && matches!(
                                    tokio::time::timeout(
                                        Duration::from_secs(10),
                                        std::pin::Pin::new(&mut tls).accept()
                                    )
                                    .await,
                                    Ok(Ok(()))
                                )
                            {
                                Box::pin(connection(tls, s, false)).await;
                            }
                        } else if s.config.upstream.is_none()
                            && let Ok(Ok(mut remote)) =
                                tokio::time::timeout(Duration::from_secs(10), TcpStream::connect((host.as_str(), 443)))
                                    .await
                        {
                            let _ = tokio::io::copy_bidirectional(&mut io, &mut remote).await;
                        }
                    }
                });
                Ok(Response::new(Body::empty()))
            }
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(io), service)
            .with_upgrades()
            .await;
    })
}
pub async fn start(rt: Arc<Runtime>) -> anyhow::Result<Option<Running>> {
    let config = rt.config();
    let Some(path) = config
        .document
        .get("native-claude")
        .and_then(|v| v.get("config-file"))
        .and_then(|v| v.as_str())
    else {
        return Ok(None);
    };
    start_config(rt, Path::new(path)).await.map(Some)
}
pub async fn start_config(rt: Arc<Runtime>, path: &Path) -> anyhow::Result<Running> {
    let c: Config = serde_json::from_slice(&std::fs::read(path)?)?;
    anyhow::ensure!(
        c.new_session_min_remaining.is_finite() && (0.0..=50.0).contains(&c.new_session_min_remaining),
        "newSessionMinRemaining must be between 0 and 50"
    );
    anyhow::ensure!(
        (1..=16).contains(&c.max_attempts),
        "maxAttempts must be between 1 and 16"
    );
    // Existing SQLite routing are migrated offline once; never read SQLite as JSON.
    anyhow::ensure!(
        !c.database.extension().is_some_and(|e| e == "sqlite"),
        "Run the offline installer upgrade to migrate SQLite affinity before starting Rust"
    );
    if c.fixture_accounts.is_some() || c.fixture_quota.is_some() {
        anyhow::ensure!(
            c.upstream
                .as_ref()
                .is_some_and(|u| ["localhost", "127.0.0.1", "::1"].contains(&u.hostname.as_str())),
            "Fixtures require a loopback fake upstream"
        );
    }
    let owner = if c.fixture_accounts.is_none() && !c.auth_dir.as_os_str().is_empty() {
        use std::fs::OpenOptions;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(c.auth_dir.join(".native-owner.lock"))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            anyhow::ensure!(
                unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
                "A native credential owner already runs for this auth directory"
            );
        }
        Some(file)
    } else {
        None
    };
    let secret = String::from_utf8(private(&c.proxy_secret)?)?.trim().to_owned();
    anyhow::ensure!(secret.len() >= 32, "Private proxy credential required");
    let routing = RoutingState::load(&c.database, now())?;
    renew_certificate(&c)?;
    let tls = load_tls(&c)?;
    let mut client = wreq::Client::builder()
        .no_proxy()
        .retry(wreq::retry::Policy::never())
        .connect_timeout(Duration::from_secs(15))
        .redirect(wreq::redirect::Policy::none())
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd();
    if let Some(ca) = c.upstream.as_ref().and_then(|u| u.ca.as_ref()) {
        client = client.tls_cert_store(wreq::tls::trust::CertStore::from_pem_stack(std::fs::read(ca)?)?);
    }
    let s = Arc::new(State {
        events: diagnostics::Writer::start(c.events.clone(), rt.native_events.lost.clone())?,
        config: c.clone(),
        rt,
        client: client.build()?,
        tls: std::sync::RwLock::new(tls),
        secret,
        routing: Mutex::new(routing),
        quota: Default::default(),
        sessions: Default::default(),
    });
    let plain = TcpListener::bind(("127.0.0.1", c.http_proxy_port))
        .await
        .context("native CONNECT listener")?;
    let secure = TcpListener::bind(("127.0.0.1", c.port))
        .await
        .context("native diagnostics listener")?;
    *s.rt.native_state.write().unwrap() = Arc::downgrade(&s);
    let cert_state = s.clone();
    let certificate = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            match renew_certificate(&cert_state.config).and_then(|changed| {
                if changed {
                    load_tls(&cert_state.config).map(Some)
                } else {
                    Ok(None)
                }
            }) {
                Ok(Some(tls)) => *cert_state.tls.write().unwrap() = tls,
                Ok(None) => {}
                Err(_) => tracing::error!("Native Claude certificate renewal failed"),
            }
        }
    });
    let s1 = s.clone();
    let a = tokio::spawn(async move {
        while let Ok((io, _)) = plain.accept().await {
            let s = s1.clone();
            tokio::spawn(connection(io, s, true));
        }
    });
    let b = tokio::spawn(async move {
        while let Ok((io, _)) = secure.accept().await {
            let s = s.clone();
            tokio::spawn(async move {
                if let Ok(ssl) = new_ssl(&s)
                    && let Ok(mut tls) = tokio_btls::SslStream::new(ssl, io)
                    && matches!(
                        tokio::time::timeout(Duration::from_secs(10), std::pin::Pin::new(&mut tls).accept()).await,
                        Ok(Ok(()))
                    )
                {
                    connection(tls, s, true).await;
                }
            });
        }
    });
    println!(
        "Native Claude transparent router ready on loopback:{}",
        c.http_proxy_port
    );
    Ok(Running(vec![a, b, certificate], owner))
}

fn load_tls(c: &Config) -> anyhow::Result<Arc<SslAcceptor>> {
    let mut tls = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls())?;
    tls.set_certificate_chain_file(&c.cert)?;
    tls.set_private_key_file(&c.key, SslFiletype::PEM)?;
    tls.check_private_key()?;
    tls.set_alpn_select_callback(|_, offered| {
        btls::ssl::select_next_proto(b"\x08http/1.1", offered).ok_or(btls::ssl::AlpnError::ALERT_FATAL)
    });
    Ok(Arc::new(tls.build()))
}
fn renew_certificate(c: &Config) -> anyhow::Result<bool> {
    if c.fixture_accounts.is_some() || c.fixture_quota.is_some() {
        return Ok(false);
    }
    use btls::{
        asn1::Asn1Time,
        bn::BigNum,
        hash::MessageDigest,
        pkey::PKey,
        x509::{
            X509,
            extension::{ExtendedKeyUsage, KeyUsage, SubjectAlternativeName},
        },
    };
    let old = X509::from_pem(&std::fs::read(&c.cert)?)?;
    let threshold = Asn1Time::days_from_now(30)?;
    if old.not_after().compare(threshold.as_ref())? == std::cmp::Ordering::Greater {
        return Ok(false);
    }
    let state = c.cert.parent().context("Certificate directory")?;
    let ca = X509::from_pem(&std::fs::read(state.join("ca.crt"))?)?;
    let ca_key = PKey::private_key_from_pem(&private(&state.join("ca.key"))?)?;
    let key = PKey::private_key_from_pem(&private(&c.key)?)?;
    let mut b = X509::builder()?;
    b.set_version(2)?;
    let serial = BigNum::from_slice(uuid::Uuid::new_v4().as_bytes())?.to_asn1_integer()?;
    b.set_serial_number(&serial)?;
    b.set_subject_name(old.subject_name())?;
    b.set_issuer_name(ca.subject_name())?;
    b.set_pubkey(&key)?;
    let begin = Asn1Time::days_from_now(0)?;
    let end = Asn1Time::days_from_now(365)?;
    b.set_not_before(begin.as_ref())?;
    b.set_not_after(end.as_ref())?;
    let san = SubjectAlternativeName::new()
        .dns("api.anthropic.com")
        .dns("localhost")
        .ip("127.0.0.1")
        .build(&b.x509v3_context(Some(&ca), None))?;
    b.append_extension(&san)?;
    {
        let usage = KeyUsage::new().digital_signature().key_encipherment().build()?;
        b.append_extension(&usage)?;
    };
    {
        let usage = ExtendedKeyUsage::new().server_auth().build()?;
        b.append_extension(&usage)?;
    };
    b.sign(&ca_key, MessageDigest::sha256())?;
    let tmp = c.cert.with_extension("crt.tmp");
    std::fs::write(&tmp, b.build().to_pem()?)?;
    std::fs::rename(tmp, &c.cert)?;
    Ok(true)
}

fn new_ssl(s: &State) -> Result<Ssl, btls::error::ErrorStack> {
    Ssl::new(s.tls.read().unwrap().context())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn new_sessions_choose_soonest_weekly_reset_but_existing_pins_win() {
        let at = 2_000_000_000_000;
        let account = |id: &str, reset: &str| Account {
            id: id.into(),
            token: String::new(),
            enabled: true,
            file_id: String::new(),
            name: String::new(),
            disabled: false,
            expired: false,
            quota: json!({"seven_day":{"resets_at":reset}}),
        };
        let pool = vec![
            account("later", "2033-05-19T04:33:20Z"),
            account("sooner", "2033-05-18T04:33:20Z"),
        ];
        let mut routing = RoutingState::default();
        let tried = HashSet::new();
        assert_eq!(choose_account("new", &pool, &routing, &tried, at).unwrap().id, "sooner");
        routing
            .affinity
            .bind("existing".into(), "later".into(), at, &HashSet::new())
            .unwrap();
        assert_eq!(
            choose_account("existing", &pool, &routing, &tried, at).unwrap().id,
            "later"
        );
        routing.cooldowns.cool("later", "opus", 429, Some(1000), at);
        assert_eq!(
            choose_account("existing", &pool, &routing, &tried, at).unwrap().id,
            "sooner"
        );
        routing.cooldowns = Default::default();
        let mut disabled = pool.clone();
        disabled[0].enabled = false;
        assert_eq!(
            choose_account("existing", &disabled, &routing, &tried, at).unwrap().id,
            "sooner"
        );
        let tried = HashSet::from(["later".to_owned()]);
        assert_eq!(
            choose_account("existing", &pool, &routing, &tried, at).unwrap().id,
            "sooner"
        );
    }
    #[test]
    fn reset_ranking_uses_weekly_not_short_window_and_unknown_dates_rank_last() {
        let at = 2_000_000_000_000;
        let valid =
            json!({"seven_day":{"resets_at":"2033-05-18T04:33:20Z"},"five_hour":{"resets_at":"2033-05-18T03:33:20Z"}});
        assert_eq!(weekly_reset(&valid, at), Some(at + 3600000));
        for reset in [Value::Null, json!("invalid"), json!("2020-01-01T00:00:00Z")] {
            assert_eq!(weekly_reset(&json!({"seven_day":{"resets_at":reset}}), at), None);
        }
    }
    #[test]
    fn native_identity_is_stable_across_account_metadata() {
        let a = json!({"metadata":{"user_id":"user_A_account_A_session_native-123"}});
        let b = json!({"metadata":{"user_id":"{\"account_uuid\":\"B\",\"session_id\":\"native-123\"}"}});
        assert_eq!(session(&a, &HeaderMap::new()), session(&b, &HeaderMap::new()));
        assert!(session(&json!({}), &HeaderMap::new()).is_none());
    }
    #[test]
    fn included_quota_does_not_require_changing_account_billing() {
        let mut q = json!({"five_hour":{"utilization":1},"seven_day":{"utilization":1}});
        assert!(eligible(&q, "claude-haiku-5-5"));
        q["extra_usage"] = json!({"is_enabled":true,"monthly_limit":100,"used_credits":99});
        assert!(eligible(&q, "claude-haiku-5-5"));
        q["five_hour"]["utilization"] = json!(100);
        assert!(!eligible(&q, "claude-haiku-5-5"));
        q["five_hour"]["utilization"] = json!(1);
        q["seven_day_opus"] = json!({"utilization":100});
        assert!(!eligible(&q, "claude-opus-5-5"));
        assert!(eligible(&q, "claude-haiku-5-5"));
    }
    #[test]
    fn headers_keep_future_betas_and_separate_credentials() {
        let mut h = HeaderMap::new();
        h.insert(
            "anthropic-beta",
            HeaderValue::from_static("inline-tools-2026-09-15,FUTURE-BETA"),
        );
        h.insert("authorization", HeaderValue::from_static("Bearer native"));
        h.insert("x-api-key", HeaderValue::from_static("native-key"));
        h.insert("proxy-authorization", HeaderValue::from_static("secret"));
        let inference = headers(&h, Some("inference"));
        assert_eq!(inference["anthropic-beta"], h["anthropic-beta"]);
        assert_eq!(inference["authorization"], "Bearer inference");
        assert!(!inference.contains_key("x-api-key"));
        assert!(!inference.contains_key("proxy-authorization"));
        assert_eq!(headers(&h, None)["authorization"], "Bearer native");
    }
    #[test]
    fn usage_keeps_split_events_after_large_output() {
        let mut scan = UsageScanner::default();
        for b in b"data: {\"message\":{\"usage\":{\"cache_read_input_tokens\":42}}}\n\n" {
            scan.push(&[*b]);
        }
        for _ in 0..2000 {
            scan.push(&vec![b'x'; 1024]);
            scan.push(b"\n");
        }
        for b in b"data: {\"usage\":{\"output_tokens\":7}}\n\n" {
            scan.push(&[*b]);
        }
        assert_eq!(scan.value(), json!({"cache_read_input_tokens":42,"output_tokens":7}));
    }
}
