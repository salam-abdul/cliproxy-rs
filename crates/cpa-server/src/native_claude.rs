//! Opt-in native Claude CONNECT transport. Unlike the API translator, inference
//! bodies and feature headers are never shaped; only the inference credential changes.
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
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, BufReader},
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
    // Test fixtures cannot use a real provider destination or native auth directory.
    fixture_accounts: Option<Vec<Account>>,
    fixture_quota: Option<Value>,
    upstream: Option<Target>,
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
fn yes() -> bool {
    true
}
#[derive(Default, Serialize, Deserialize)]
struct Pins {
    #[serde(default)]
    pins: BTreeMap<String, String>,
    #[serde(default)]
    cooldowns: BTreeMap<String, u64>,
}
struct State {
    config: Config,
    rt: Arc<Runtime>,
    client: wreq::Client,
    tls: std::sync::RwLock<Arc<SslAcceptor>>,
    secret: String,
    pins: Mutex<Pins>,
    quota: tokio::sync::Mutex<HashMap<String, (Instant, Value)>>,
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
    let e = &q["extra_usage"];
    match e["is_enabled"].as_bool() {
        Some(false) => {}
        Some(true) if matches!((e["monthly_limit"].as_f64(),e["used_credits"].as_f64()),(Some(l),Some(u)) if l.is_finite()&&u.is_finite()&&l>=0.0&&u>=l) =>
            {}
        _ => return false,
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
// Rank only fresh, future weekly resets. Missing dates cannot win selection.
fn weekly_reset(q: &Value, at: u64) -> Option<u64> {
    q["seven_day"]["resets_at"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .and_then(|d| u64::try_from(d.timestamp_millis()).ok())
        .filter(|reset| *reset > at)
}
fn choose_account<'a>(
    sid: &str,
    pool: &'a [Account],
    pins: &Pins,
    tried: &HashSet<String>,
    at: u64,
) -> Option<&'a Account> {
    let usable: Vec<_> = pool
        .iter()
        .filter(|a| a.enabled && !tried.contains(&a.id) && pins.cooldowns.get(&a.id).copied().unwrap_or(0) <= at)
        .collect();
    usable
        .iter()
        .find(|a| pins.pins.get(sid) == Some(&a.id))
        .copied()
        .or_else(|| {
            usable
                .into_iter()
                .min_by_key(|a| weekly_reset(&a.quota, at).unwrap_or(u64::MAX))
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
fn persist(path: &Path, pins: &Pins) -> anyhow::Result<()> {
    let temp = path.with_extension("json.tmp");
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&temp)?;
    f.write_all(&serde_json::to_vec(pins)?)?;
    f.sync_all()?;
    std::fs::rename(temp, path)?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}
impl State {
    fn event(&self, mut v: Value) {
        v["at"] = json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        if let Ok(mut f) = opts.open(&self.config.events) {
            let _ = writeln!(f, "{v}");
        }
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
    async fn quota(&self, a: &Account) -> anyhow::Result<Value> {
        if self.config.fixture_accounts.is_some() {
            return Ok(a.quota.clone());
        }
        if let Some(q) = &self.config.fixture_quota {
            return Ok(q.clone());
        }
        // One owner for refresh; quota lookups are single-flight and token-bound.
        let mut cache = self.quota.lock().await;
        let key = hash(format!("{}:{}", a.id, a.token));
        if let Some((at, q)) = cache.get(&key).filter(|(at, _)| at.elapsed() < Duration::from_secs(30)) {
            let _ = at;
            return Ok(q.clone());
        }
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
        let bytes = r.bytes().await?;
        anyhow::ensure!(bytes.len() <= 1048576, "Usage response too large");
        let q: Value = serde_json::from_slice(&bytes)?;
        cache.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(30));
        cache.insert(key, (Instant::now(), q.clone()));
        Ok(q)
    }
    fn select(&self, sid: &str, pool: &[Account], tried: &HashSet<String>) -> anyhow::Result<Option<Account>> {
        let mut pins = self.pins.lock().unwrap();
        let a = choose_account(sid, pool, &pins, tried, now()).cloned();
        if let Some(a) = &a
            && pins.pins.get(sid) != Some(&a.id)
        {
            pins.pins.insert(sid.to_owned(), a.id.clone());
            persist(&self.config.database, &pins)?;
        }
        Ok(a)
    }
    async fn admit(&self, sid: &str, model: &str, tried: &mut HashSet<String>) -> anyhow::Result<Option<Account>> {
        loop {
            let mut pool = Vec::new();
            for a in self
                .accounts()?
                .into_iter()
                .filter(|a| a.enabled && !tried.contains(&a.id))
            {
                match self.quota(&a).await {
                    Ok(q) if eligible(&q, model) => {
                        let mut a = a;
                        a.quota = q;
                        pool.push(a);
                    }
                    _ => self.event(json!({"type":"quota-unavailable","account":a.id})),
                }
            }
            let Some(a) = self.select(sid, &pool, tried)? else {
                return Ok(None);
            };
            tried.insert(a.id.clone());
            let Some(current) = self
                .accounts()?
                .into_iter()
                .find(|current| current.id == a.id && current.enabled)
            else {
                continue;
            };
            if matches!(self.quota(&current).await,Ok(q) if eligible(&q,model)) {
                return Ok(Some(current));
            }
        }
    }
    fn cool(&self, a: &Account, status: u16) -> anyhow::Result<()> {
        let mut pins = self.pins.lock().unwrap();
        pins.cooldowns
            .insert(a.id.clone(), now() + if status == 429 { 60000 } else { 15000 });
        persist(&self.config.database, &pins)
    }
    async fn status(&self) -> anyhow::Result<Value> {
        let mut all = Vec::new();
        for a in self.accounts()? {
            let q = if a.enabled { self.quota(&a).await.ok() } else { None };
            all.push(json!({"id":a.id,"fileId":a.file_id,"name":a.name,"enabled":a.enabled,"disabled":a.disabled,"expired":a.expired,"includedQuota":q.map(|q|json!({"fiveHour":q["five_hour"]["utilization"],"weekly":q["seven_day"]["utilization"],"weeklyResetsAt":q["seven_day"]["resets_at"],"eligible":eligible(&q,"claude-sonnet-5-5")}))}));
        }
        let ready = !self.config.qualification && all.iter().any(|a| a["includedQuota"]["eligible"] == true);
        let mut counts = BTreeMap::new();
        for id in self.pins.lock().unwrap().pins.values() {
            *counts.entry(id.clone()).or_insert(0) += 1;
        }
        let tail = (|| -> std::io::Result<String> {
            use std::io::{Read, Seek, SeekFrom};
            let mut f = std::fs::File::open(&self.config.events)?;
            let size = f.metadata()?.len();
            f.seek(SeekFrom::Start(size.saturating_sub(65536)))?;
            let mut out = String::new();
            f.read_to_string(&mut out)?;
            Ok(out)
        })()
        .unwrap_or_default();
        let recent = tail
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|e| e["type"] == "inference")
            .rev()
            .take(10)
            .collect::<Vec<_>>();
        Ok(
            json!({"selectionStrategy":"soonest-reset","ready":ready,"ownerReady":true,"proxyReady":true,"qualification":self.config.qualification,"accounts":all,"affinity":counts.into_iter().map(|(a,n)|json!({"account":a,"sessions":n})).collect::<Vec<_>>(),"recent":recent}),
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
    let Some(model) = body["model"].as_str().filter(|m| m.starts_with("claude-")) else {
        return Ok(error(400, "Native Claude model required; aliases are not rewritten"));
    };
    let model = model.to_owned();
    let lock = s.sessions.lock().unwrap().entry(sid.clone()).or_default().clone();
    let guard = lock.lock_owned().await;
    let mut tried = HashSet::new();
    loop {
        let Some(a) = s.admit(&sid, &model, &mut tried).await? else {
            return Ok(error(
                429,
                "No account has confirmed available included quota; extra usage and direct fallback are disabled",
            ));
        };
        let started = Instant::now();
        let result = s
            .client
            .request(parts.method.clone(), format!("{target}{path}"))
            .headers(headers(&parts.headers, Some(&a.token)))
            .body(raw.clone())
            .read_timeout(Duration::from_secs(30))
            .send()
            .await;
        let r = match result {
            Ok(r) => r,
            Err(_) => {
                s.cool(&a, 0)?;
                s.event(json!({"type":"transport-failure","session":sid,"account":a.id,"replayed":false}));
                return Ok(error(
                    422,
                    "Upstream response was lost; request was not replayed to avoid duplicate inference. Check the session before explicitly retrying.",
                ));
            }
        };
        let status = r.status();
        if [401, 429, 503, 529].contains(&status.as_u16()) {
            s.cool(&a, status.as_u16())?;
            s.quota.lock().await.clear();
            s.event(json!({"type":"failover","session":sid,"account":a.id,"status":status.as_u16()}));
            continue;
        }
        let h = headers(r.headers(), None);
        let encoding = h
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let facts = Exchange {
            a,
            model,
            sid,
            status: status.as_u16(),
            digest: hash(&raw),
            body_bytes: raw.len(),
            beta: parts
                .headers
                .get("anthropic-beta")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned(),
            started,
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
                            o.finish(false).await;
                        }
                        Some((item, (stream, record, guard)))
                    }
                    None => {
                        if let Some(o) = record.take() {
                            o.finish(true).await;
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
#[derive(Clone)]
struct Exchange {
    a: Account,
    model: String,
    sid: String,
    status: u16,
    digest: String,
    body_bytes: usize,
    beta: String,
    started: Instant,
}
struct Observation {
    s: Arc<State>,
    facts: Exchange,
    usage: Arc<Mutex<UsageScanner>>,
    writer: Option<tokio::io::DuplexStream>,
    decoder: Option<JoinHandle<()>>,
    published: bool,
}
#[derive(Default)]
struct UsageScanner {
    line: Vec<u8>,
    json: JsonUsage,
    usage: serde_json::Map<String, Value>,
    oversized_line: bool,
}

#[derive(Default)]
struct JsonUsage {
    depth: i32,
    string: bool,
    escaped: bool,
    word: Vec<u8>,
    key: bool,
    want: bool,
    capture: Option<Vec<u8>>,
    value: Option<Value>,
}
impl JsonUsage {
    fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if let Some(capture) = self.capture.as_mut() {
                if capture.len() < 65536 {
                    capture.push(b)
                } else {
                    self.capture = None;
                }
            }
            if self.string {
                if self.escaped {
                    self.escaped = false;
                    continue;
                }
                if b == b'\\' {
                    self.escaped = true;
                    continue;
                }
                if b == b'"' {
                    self.string = false;
                    self.key = self.depth == 1 && self.word == b"usage";
                    self.word.clear();
                } else if self.depth == 1 && self.word.len() < 16 {
                    self.word.push(b)
                }
                continue;
            }
            if b.is_ascii_whitespace() {
                continue;
            }
            if self.want {
                self.want = false;
                if b == b'{' {
                    self.capture = Some(vec![b]);
                }
            }
            match b {
                b'"' => {
                    self.string = true;
                    self.word.clear();
                }
                b':' => {
                    self.want = self.key;
                    self.key = false;
                }
                b'{' | b'[' => {
                    self.depth += 1;
                    self.key = false;
                }
                b'}' | b']' => {
                    self.depth -= 1;
                    if self.depth == 1
                        && let Some(raw) = self.capture.take()
                    {
                        self.value = serde_json::from_slice(&raw).ok();
                    }
                    self.key = false;
                }
                _ => self.key = false,
            }
        }
    }
}
impl UsageScanner {
    fn push(&mut self, bytes: &[u8]) {
        self.json.push(bytes);
        for &byte in bytes {
            if byte == b'\n' {
                if !self.oversized_line
                    && let Some(data) = self.line.strip_prefix(b"data:")
                    && let Ok(v) = serde_json::from_slice::<Value>(data)
                    && let Some(u) = v["message"]["usage"].as_object().or(v["usage"].as_object())
                {
                    self.usage.extend(u.clone());
                }
                self.line.clear();
                self.oversized_line = false;
            } else if self.line.len() < 1048576 && !self.oversized_line {
                self.line.push(byte)
            } else {
                self.line.clear();
                self.oversized_line = true;
            }
        }
    }
    fn value(&self) -> Value {
        if !self.usage.is_empty() {
            Value::Object(self.usage.clone())
        } else {
            self.json.value.clone().unwrap_or(Value::Null)
        }
    }
}
impl Observation {
    fn new(s: Arc<State>, facts: Exchange, encoding: &str) -> Self {
        use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder};
        let (writer, reader) = tokio::io::duplex(65536);
        let source = BufReader::new(reader);
        let mut reader: Box<dyn AsyncRead + Unpin + Send> = match encoding {
            "gzip" => Box::new(GzipDecoder::new(source)),
            "br" => Box::new(BrotliDecoder::new(source)),
            "deflate" => Box::new(ZlibDecoder::new(source)),
            "zstd" => Box::new(ZstdDecoder::new(source)),
            _ => Box::new(source),
        };
        let usage = Arc::new(Mutex::new(UsageScanner::default()));
        let sink = usage.clone();
        let decoder = tokio::spawn(async move {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink.lock().unwrap().push(&buffer[..n]),
                }
            }
        });
        Self {
            s,
            facts,
            usage,
            writer: Some(writer),
            decoder: Some(decoder),
            published: false,
        }
    }
    async fn observe(&mut self, b: &[u8]) {
        use tokio::io::AsyncWriteExt;
        if let Some(writer) = self.writer.as_mut()
            && writer.write_all(b).await.is_err()
        {
            self.writer.take();
        }
    }
    async fn finish(mut self, complete: bool) {
        self.writer.take();
        if let Some(mut task) = self.decoder.take()
            && tokio::time::timeout(Duration::from_secs(1), &mut task).await.is_err()
        {
            task.abort();
        }
        self.publish(complete);
    }
    fn publish(&mut self, complete: bool) {
        if std::mem::replace(&mut self.published, true) {
            return;
        }
        let usage = self.usage.lock().unwrap().value();
        let f = &self.facts;
        self.s.event(json!({"type":"inference","session":f.sid,"account":f.a.id,"model":f.model,"status":if complete{f.status}else{499},"upstreamStatus":f.status,"complete":complete,"bodySha256":f.digest,"bodyBytes":f.body_bytes,"beta":f.beta,"usage":usage}));
        self.s
            .rt
            .store()
            .note_activity(&f.a.file_id, complete && f.status < 400);
        let credential = self.s.rt.store().snapshot().into_iter().find(|c| c.id == f.a.file_id);
        let auth_index = credential
            .as_ref()
            .map(|c| cpa_core::config::credentials::auth_index(c))
            .unwrap_or_default();
        let detail = crate::usage_record::parse_claude(&serde_json::to_vec(&json!({"usage":usage})).unwrap());
        let record = crate::usage_record::Record {
            timestamp: chrono::Utc::now().to_rfc3339(),
            execution_id: uuid::Uuid::new_v4().to_string(),
            provider: "claude".into(),
            executor_type: "claude".into(),
            model: f.model.clone(),
            alias: f.model.clone(),
            response_model: String::new(),
            source: f.a.name.clone(),
            auth_index,
            auth_type: "oauth".into(),
            generate: true,
            stream: true,
            failed: !complete || f.status >= 400,
            fail_status: if !complete {
                499
            } else if f.status >= 400 {
                f.status as i64
            } else {
                0
            },
            latency_ms: f.started.elapsed().as_millis() as i64,
            detail,
            ..Default::default()
        };
        self.s.rt.usage_queue().enqueue(crate::usage_record::queued(
            &record,
            &crate::usage_record::Client {
                session_id: f.sid.clone(),
                session_source: "native",
                endpoint: "POST /v1/messages".into(),
                ..Default::default()
            },
        ));
    }
}
impl Drop for Observation {
    fn drop(&mut self) {
        self.writer.take();
        if self.published {
            if let Some(task) = self.decoder.take() {
                task.abort();
            }
            return;
        }
        self.published = true;
        let mut finalizer = Self {
            s: self.s.clone(),
            facts: self.facts.clone(),
            usage: self.usage.clone(),
            writer: None,
            decoder: self.decoder.take(),
            published: false,
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                finalizer.finish(false).await;
            });
        } else {
            finalizer.publish(false);
        }
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
    // Existing SQLite pins are migrated offline once; never read SQLite as JSON.
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
    let pins = if c.database.is_file() {
        serde_json::from_slice(&private(&c.database)?)?
    } else {
        Pins::default()
    };
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
        config: c.clone(),
        rt,
        client: client.build()?,
        tls: std::sync::RwLock::new(tls),
        secret,
        pins: Mutex::new(pins),
        quota: Default::default(),
        sessions: Default::default(),
    });
    let plain = TcpListener::bind(("127.0.0.1", c.http_proxy_port))
        .await
        .context("native CONNECT listener")?;
    let secure = TcpListener::bind(("127.0.0.1", c.port))
        .await
        .context("native diagnostics listener")?;
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
        let mut pins = Pins::default();
        let tried = HashSet::new();
        assert_eq!(choose_account("new", &pool, &pins, &tried, at).unwrap().id, "sooner");
        pins.pins.insert("existing".into(), "later".into());
        assert_eq!(
            choose_account("existing", &pool, &pins, &tried, at).unwrap().id,
            "later"
        );
        pins.cooldowns.insert("later".into(), at + 1000);
        assert_eq!(
            choose_account("existing", &pool, &pins, &tried, at).unwrap().id,
            "sooner"
        );
        pins.cooldowns.clear();
        let mut disabled = pool.clone();
        disabled[0].enabled = false;
        assert_eq!(
            choose_account("existing", &disabled, &pins, &tried, at).unwrap().id,
            "sooner"
        );
        let tried = HashSet::from(["later".to_owned()]);
        assert_eq!(
            choose_account("existing", &pool, &pins, &tried, at).unwrap().id,
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
    fn quota_rejects_unknown_or_spendable_paid_extra() {
        let mut q = json!({"five_hour":{"utilization":1},"seven_day":{"utilization":1}});
        assert!(!eligible(&q, "claude-haiku-5-5"));
        q["extra_usage"] = json!({"is_enabled":false});
        assert!(eligible(&q, "claude-haiku-5-5"));
        q["extra_usage"] = json!({"is_enabled":true,"monthly_limit":100,"used_credits":99});
        assert!(!eligible(&q, "claude-haiku-5-5"));
        q["extra_usage"]["used_credits"] = json!(100);
        assert!(eligible(&q, "claude-haiku-5-5"));
        q["five_hour"]["utilization"] = json!(100);
        assert!(!eligible(&q, "claude-haiku-5-5"));
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
        assert!(scan.json.capture.as_ref().is_none_or(|v| v.len() <= 65536));
        assert!(scan.line.len() <= 1048576);
    }
    #[test]
    fn affinity_is_atomic_private_and_survives_reload() {
        let dir = std::env::temp_dir().join(format!("native-pins-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("pins.json");
        let mut pins = Pins::default();
        pins.pins.insert("native".into(), "account-b".into());
        persist(&path, &pins).unwrap();
        let read: Pins = serde_json::from_slice(&private(&path).unwrap()).unwrap();
        assert_eq!(read.pins["native"], "account-b");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
