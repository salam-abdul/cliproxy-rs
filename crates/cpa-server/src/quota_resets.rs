//! Quota resets: the one-off rate-limit resets Anthropic (grants) and ChatGPT (credits)
//! give some accounts. The management API lists them and spends one on request.
//!
//! A claim spends something that cannot be given back, so it is never retried here, one
//! claim runs per account at a time, and any answer that does not prove the outcome
//! reports `unconfirmed` rather than `failed`. The caller's request id makes a repeated
//! claim idempotent at the provider.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{sync::Mutex as AsyncMutex, time::Instant};

/// Anthropic rate-limits the grants read, which shares its endpoint with the usage read
/// that native admission needs, so a read is served for an hour.
const TTL: Duration = Duration::from_secs(3600);
/// A fresh read asks the provider only when the last attempt is at least this old.
const FRESH_AFTER: Duration = Duration::from_secs(300);
/// A failed read is served this long, or for the provider's `Retry-After` up to [`TTL`].
const FAILURE_TTL: Duration = Duration::from_secs(60);
const CAPACITY: usize = 256;
/// The longest grant or credit id listed or claimed.
const ID_LIMIT: usize = 128;
const BODY_LIMIT: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub(crate) fn of(name: &str) -> Option<Self> {
        match name {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }
}

/// What a read or a claim needs from one account, resolved by the caller.
pub(crate) struct Account {
    pub provider: Provider,
    /// The credential id; reads are cached per id and token.
    pub id: String,
    pub token: String,
    pub client: wreq::Client,
    /// Claude organization UUID, required for a Claude claim.
    pub organization: Option<String>,
    /// ChatGPT account id for the Codex `ChatGPT-Account-Id` header.
    pub chatgpt_account: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Reset {
    /// Unique per row: one Claude grant can hold several resets.
    pub key: String,
    /// The grant or credit a claim spends.
    pub id: String,
    pub kind: &'static str,
    pub expires_at: Option<String>,
    pub usable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<&'static str>,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Status {
    pub eligible: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub resets: Vec<Reset>,
    pub checked_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Applied,
    NothingToReset,
    Gone,
    NotNext,
    Unconfirmed,
    Unavailable,
    Failed,
}

#[derive(Debug, PartialEq, Serialize)]
pub(crate) struct Claim {
    pub outcome: Outcome,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
}

impl Claim {
    pub(crate) fn new(outcome: Outcome) -> Self {
        let message = match outcome {
            Outcome::Applied => "Reset applied.",
            Outcome::NothingToReset => "This account's usage does not need a reset yet. Nothing was used.",
            Outcome::Gone => "That reset is no longer available.",
            Outcome::NotNext => "The provider wants another reset used first. Nothing was used.",
            Outcome::Unconfirmed => {
                "The provider did not confirm the reset. Check the account's resets again before retrying."
            }
            Outcome::Unavailable => "Resets cannot be used right now.",
            Outcome::Failed => "The provider refused the reset.",
        };
        Self {
            outcome,
            message: message.into(),
            until: None,
        }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Self::new(Outcome::Failed)
        }
    }
}

/// A refusal before anything reached the provider.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    Busy,
    Invalid(&'static str),
    /// External requests are disabled.
    External,
}

struct Endpoints {
    claude: String,
    codex: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            claude: "https://api.anthropic.com".into(),
            codex: "https://chatgpt.com/backend-api/wham".into(),
        }
    }
}

/// One account's reads, for one token.
struct Cached {
    token: [u8; 32],
    attempted: Instant,
    /// The last good read and when it stops being served.
    good: Option<(Status, Instant)>,
    /// The last failure and when it stops being served.
    failure: Option<(String, Instant)>,
    /// Until when the provider asked not to be read (`Retry-After`).
    backoff: Option<Instant>,
}

impl Cached {
    fn new(token: [u8; 32], now: Instant) -> Self {
        Self {
            token,
            attempted: now,
            good: None,
            failure: None,
            backoff: None,
        }
    }

    /// What to answer without asking the provider, or `None` to ask it.
    fn serve(&self, fresh: bool, now: Instant) -> Option<Result<Status, String>> {
        let good = self.good.as_ref().filter(|(_, until)| *until > now).map(|(s, _)| s);
        let failure = self.failure.as_ref().filter(|(_, until)| *until > now).map(|(e, _)| e);
        let waiting = self.backoff.is_some_and(|until| until > now) || now < self.attempted + FRESH_AFTER;
        if fresh && !waiting {
            return None;
        }
        // A failure is served at least as long as its backoff lasts.
        match (good, failure) {
            (Some(good), _) => Some(Ok(good.clone())),
            (None, Some(failure)) => Some(Err(failure.clone())),
            (None, None) => None,
        }
    }
}

struct Slot {
    value: Arc<AsyncMutex<Option<Cached>>>,
    used: Instant,
}

#[derive(Default)]
pub(crate) struct QuotaResets {
    cache: Mutex<HashMap<String, Slot>>,
    claiming: Mutex<HashSet<String>>,
    endpoints: Endpoints,
}

/// Holds an account's claim slot until the claim finishes.
struct ClaimGuard<'a> {
    claiming: &'a Mutex<HashSet<String>>,
    id: String,
}

impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        self.claiming
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}

struct ReadError {
    message: String,
    retry_after: Option<Duration>,
}

impl ReadError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retry_after: None,
        }
    }
}

impl QuotaResets {
    #[cfg(test)]
    pub(crate) fn with_base(base: &str) -> Self {
        Self {
            endpoints: Endpoints {
                claude: base.into(),
                codex: format!("{base}/backend-api/wham"),
            },
            ..Self::default()
        }
    }

    fn url(&self, provider: Provider, path: &str, deny_external: bool) -> Result<String, String> {
        let base = match provider {
            Provider::Claude => &self.endpoints.claude,
            Provider::Codex => &self.endpoints.codex,
        };
        if deny_external && !base.starts_with("http://127.0.0.1:") {
            return Err("External requests are disabled".into());
        }
        Ok(format!("{base}{path}"))
    }

    fn slot(&self, id: &str) -> Arc<AsyncMutex<Option<Cached>>> {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if !cache.contains_key(id) && cache.len() >= CAPACITY {
            let oldest = cache
                .iter()
                .filter(|(_, slot)| Arc::strong_count(&slot.value) == 1)
                .min_by_key(|(_, slot)| slot.used)
                .map(|(id, _)| id.clone());
            if let Some(oldest) = oldest {
                cache.remove(&oldest);
            }
        }
        let slot = cache.entry(id.to_owned()).or_insert_with(|| Slot {
            value: Default::default(),
            used: Instant::now(),
        });
        slot.used = Instant::now();
        slot.value.clone()
    }

    /// Moves every account's last attempt back past [`FRESH_AFTER`].
    #[cfg(test)]
    pub(crate) async fn age_attempts(&self) {
        let slots: Vec<_> = self
            .cache
            .lock()
            .unwrap()
            .values()
            .map(|slot| slot.value.clone())
            .collect();
        for slot in slots {
            if let Some(cached) = slot.lock().await.as_mut() {
                cached.attempted -= FRESH_AFTER;
            }
        }
    }

    /// Drops the cached read so the next read asks the provider.
    pub(crate) async fn forget(&self, id: &str) {
        let value = self
            .cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .map(|slot| slot.value.clone());
        if let Some(value) = value {
            *value.lock().await = None;
        }
    }

    /// The account's resets, from a read at most [`TTL`] old. With `fresh`, the provider
    /// is asked again unless it was asked in the last [`FRESH_AFTER`] or asked to wait; a
    /// failed fresh read keeps serving the last good one. Concurrent reads of one
    /// account share a single upstream call.
    pub(crate) async fn status(&self, account: &Account, fresh: bool, deny_external: bool) -> Result<Status, String> {
        // Checked before the cache, so turning external requests back on takes effect.
        self.url(account.provider, "", deny_external)?;
        let token: [u8; 32] = Sha256::digest(account.token.as_bytes()).into();
        let slot = self.slot(&account.id);
        let mut cached = slot.lock().await;
        let now = Instant::now();
        if let Some(answer) = cached
            .as_ref()
            .filter(|c| c.token == token)
            .and_then(|c| c.serve(fresh, now))
        {
            return answer.map(|status| unexpired(status, SystemTime::now()));
        }
        if cached.as_ref().is_some_and(|c| c.token != token) {
            *cached = None;
        }
        let entry = cached.get_or_insert_with(|| Cached::new(token, now));
        let read = self.read(account, deny_external).await;
        let now = Instant::now();
        entry.attempted = now;
        match read {
            Ok(status) => {
                entry.good = Some((status.clone(), now + TTL));
                entry.failure = None;
                entry.backoff = None;
                Ok(status)
            }
            Err(e) => {
                let wait = e.retry_after.map(|after| after.clamp(FAILURE_TTL, TTL));
                entry.backoff = wait.map(|wait| now + wait);
                entry.failure = Some((e.message.clone(), now + wait.unwrap_or(FAILURE_TTL)));
                match entry.good.as_ref().filter(|(_, until)| *until > now) {
                    Some((good, _)) => Ok(unexpired(good.clone(), SystemTime::now())),
                    None => Err(e.message),
                }
            }
        }
    }

    async fn read(&self, account: &Account, deny_external: bool) -> Result<Status, ReadError> {
        let request = match account.provider {
            Provider::Claude => {
                let url = self
                    .url(
                        Provider::Claude,
                        "/api/oauth/usage?cedar_ember=1&skip_spend=1",
                        deny_external,
                    )
                    .map_err(ReadError::new)?;
                claude_headers(account.client.get(url), account)
            }
            Provider::Codex => {
                let url = self
                    .url(Provider::Codex, "/rate-limit-reset-credits", deny_external)
                    .map_err(ReadError::new)?;
                codex_headers(account.client.get(url), account)
            }
        };
        let response = request
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|_| ReadError::new("The resets request failed"))?;
        let status = response.status().as_u16();
        if status != 200 {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| retry_after(v, SystemTime::now()));
            return Err(ReadError {
                message: match status {
                    401 => "The provider rejected the account's token (HTTP 401). Sign in again.".into(),
                    403 => "The provider denied access to resets (HTTP 403).".into(),
                    429 => "The provider is limiting reset checks (HTTP 429). Try again later.".into(),
                    _ => format!("The provider answered HTTP {status}."),
                },
                retry_after,
            });
        }
        let body = read_body(response)
            .await
            .ok_or_else(|| ReadError::new("The resets response was cut off or too large"))?;
        let body: Value =
            serde_json::from_slice(&body).map_err(|_| ReadError::new("The resets response is not JSON"))?;
        let at = SystemTime::now();
        Ok(match account.provider {
            Provider::Claude => claude_status(&body["cedar_ember"], at),
            Provider::Codex => codex_status(&body, at),
        })
    }

    /// Spends one reset. `reset` is the grant or credit id from [`Status`]; `request` is
    /// the caller's idempotency key, reused when retrying an unconfirmed claim.
    pub(crate) async fn claim(
        &self,
        account: &Account,
        reset: &str,
        request: &str,
        deny_external: bool,
    ) -> Result<Claim, Refusal> {
        if !token_like(request, 64) {
            return Err(Refusal::Invalid(
                "request_id must be 1 to 64 letters, digits, '-' or '_'",
            ));
        }
        let (url, body) = match account.provider {
            Provider::Claude => {
                if !token_like(reset, ID_LIMIT) {
                    return Err(Refusal::Invalid("reset_id is not a Claude grant id"));
                }
                let Some(organization) = account.organization.as_deref().filter(|o| uuid_like(o)) else {
                    return Err(Refusal::Invalid(
                        "This Claude account has no organization UUID. Sign in again through the dashboard.",
                    ));
                };
                let url = self
                    .url(
                        Provider::Claude,
                        &format!("/api/organizations/{organization}/reset_rate_limits"),
                        deny_external,
                    )
                    .map_err(|_| Refusal::External)?;
                (
                    url,
                    json!({"program": "cedar_ember", "grant_id": reset, "request_id": request}),
                )
            }
            Provider::Codex => {
                if !token_like(reset, ID_LIMIT) {
                    return Err(Refusal::Invalid("reset_id is not a Codex credit id"));
                }
                let url = self
                    .url(Provider::Codex, "/rate-limit-reset-credits/consume", deny_external)
                    .map_err(|_| Refusal::External)?;
                (url, json!({"credit_id": reset, "redeem_request_id": request}))
            }
        };
        let _guard = {
            let mut claiming = self.claiming.lock().unwrap_or_else(PoisonError::into_inner);
            if !claiming.insert(account.id.clone()) {
                return Err(Refusal::Busy);
            }
            ClaimGuard {
                claiming: &self.claiming,
                id: account.id.clone(),
            }
        };
        let request = match account.provider {
            Provider::Claude => claude_headers(account.client.post(url), account),
            Provider::Codex => codex_headers(account.client.post(url), account),
        };
        // The list is stale once the claim may land, even if this task never finishes.
        self.forget(&account.id).await;
        let response = request
            .header("content-type", "application/json")
            .body(body.to_string())
            .timeout(Duration::from_secs(30))
            .send()
            .await;
        // Whatever happened, the cached list may now be wrong.
        self.forget(&account.id).await;
        let Ok(response) = response else {
            return Ok(Claim::new(Outcome::Unconfirmed));
        };
        let status = response.status().as_u16();
        let body = read_body(response).await;
        Ok(match account.provider {
            Provider::Claude => claude_claim(status, body.as_deref()),
            Provider::Codex => codex_claim(status, body.as_deref()),
        })
    }
}

fn claude_headers(request: wreq::RequestBuilder, account: &Account) -> wreq::RequestBuilder {
    request
        .redirect(wreq::redirect::Policy::none())
        .bearer_auth(&account.token)
        .header("accept", "application/json")
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("user-agent", crate::claude_usage::USER_AGENT)
}

fn codex_headers(request: wreq::RequestBuilder, account: &Account) -> wreq::RequestBuilder {
    let request = request
        .redirect(wreq::redirect::Policy::none())
        .bearer_auth(&account.token)
        .header("accept", "application/json")
        .header("user-agent", cpa_exec::codex::USER_AGENT)
        .header("openai-beta", "codex-1")
        .header("originator", cpa_exec::codex::ORIGINATOR);
    match &account.chatgpt_account {
        Some(id) => request.header("chatgpt-account-id", id),
        None => request,
    }
}

/// The whole body up to [`BODY_LIMIT`]; `None` when cut off or larger.
async fn read_body(response: wreq::Response) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if chunk.len() > BODY_LIMIT - bytes.len() {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    Some(bytes)
}

/// `Retry-After` as delta seconds or an HTTP date.
fn retry_after(raw: &str, now: SystemTime) -> Option<Duration> {
    let raw = raw.trim();
    if let Ok(seconds) = raw.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = chrono::DateTime::parse_from_rfc2822(raw).ok()?;
    SystemTime::from(at).duration_since(now).ok()
}

/// `status` without the resets that expired while it was cached.
fn unexpired(mut status: Status, now: SystemTime) -> Status {
    status.resets.retain(|r| {
        r.expires_at
            .as_deref()
            .and_then(crate::quota_time::rfc3339)
            .is_none_or(|at| at > now)
    });
    status
}

fn token_like(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn uuid_like(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

fn rfc3339(time: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// An expiry as RFC 3339 text or Unix seconds.
fn expiry(value: &Value) -> Option<SystemTime> {
    match value {
        Value::String(s) => crate::quota_time::reset_time(s),
        Value::Number(n) => crate::quota_time::reset_time(&n.to_string()),
        _ => None,
    }
}

/// Anthropic's `cedar_ember` block of `/api/oauth/usage?cedar_ember=1`.
fn claude_status(block: &Value, at: SystemTime) -> Status {
    let eligible = block["eligible"].as_bool() == Some(true);
    let mut status = Status {
        eligible,
        reason: None,
        resets: Vec::new(),
        checked_at: rfc3339(at),
    };
    if !eligible {
        status.reason = Some(match block["ineligible_reason"].as_str() {
            Some("cli_version") => "Anthropic requires a newer Claude Code identity for resets.".into(),
            Some("surface") => "Anthropic offers resets only to Claude Code sign-ins.".into(),
            Some(other) => format!("Anthropic reports this account ineligible for resets ({other})."),
            None if block.is_null() => "Anthropic reported no reset information for this account.".into(),
            None => "Anthropic reports this account ineligible for resets.".into(),
        });
        return status;
    }
    let next = block["next_grant_id"].as_str().filter(|s| !s.is_empty());
    for grant in block["grants"].as_array().into_iter().flatten() {
        let Some(id) = grant["id"].as_str().filter(|s| token_like(s, ID_LIMIT)) else {
            continue;
        };
        let left = grant["resets_left"].as_f64().unwrap_or(0.0).floor();
        if left < 1.0 {
            continue;
        }
        let ends = expiry(&grant["ends_at"]);
        if ends.is_some_and(|ends| ends <= at) {
            continue;
        }
        let clears: Vec<&str> = grant["clears"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let gives_back = grant["gives_back_weekly"].as_bool() == Some(true);
        let kind = claude_kind(grant["type"].as_str(), &clears, gives_back);
        let blocked = if grant["paused"].as_bool() == Some(true) {
            Some("Paused by Anthropic")
        } else if next.is_some_and(|next| next != id) {
            Some("Anthropic wants another reset used first")
        } else if grant["usable_now"].as_bool() != Some(true) {
            Some("Not usable yet")
        } else {
            None
        };
        let detail = claude_detail(&clears, gives_back);
        // `resets_left` is a count; a grant cannot plausibly hold more than a handful.
        for n in 0..(left as usize).min(32) {
            status.resets.push(Reset {
                key: format!("{id}:{n}"),
                id: id.into(),
                kind,
                expires_at: ends.map(rfc3339),
                usable: blocked.is_none(),
                blocked,
                detail: detail.clone(),
            });
        }
    }
    status
}

/// What a grant refills, decided as Claude Code decides it: a grant that gives back the
/// week's Fable use is a Fable reset, then the grant's type, then the windows it clears.
fn claude_kind(kind: Option<&str>, clears: &[&str], gives_back_weekly: bool) -> &'static str {
    if gives_back_weekly {
        return "fable";
    }
    match kind {
        Some("session") => return "session",
        Some("weekly") => return "weekly",
        Some("full") => return "full",
        _ => {}
    }
    let session = clears.contains(&"five_hour");
    let other = clears.iter().any(|c| *c != "five_hour");
    match (session, other) {
        (true, false) => "session",
        (false, true) => "weekly",
        _ => "full",
    }
}

fn claude_detail(clears: &[&str], gives_back_weekly: bool) -> String {
    let names: Option<Vec<&str>> = clears
        .iter()
        .map(|c| match *c {
            "five_hour" => Some("5-hour"),
            "seven_day" => Some("weekly"),
            "seven_day_overage_included" => Some("Fable"),
            _ => None,
        })
        .collect();
    // An unknown window keeps the general wording rather than a list that understates.
    let mut detail = match names.as_deref() {
        Some([one]) => format!("Refills your {one} limit now"),
        Some([rest @ .., last]) if !rest.is_empty() => {
            format!("Refills your {} and {last} limits now", rest.join(", "))
        }
        _ => "Refills your Claude limits now".into(),
    };
    if gives_back_weekly && !clears.contains(&"seven_day") {
        detail.push_str(" and gives back what Fable used of your weekly limit");
    }
    detail + ". This cannot be undone."
}

/// ChatGPT's `/wham/rate-limit-reset-credits`. Its one reset type refills every limit.
fn codex_status(body: &Value, at: SystemTime) -> Status {
    let resets = body["credits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["status"].as_str().is_none_or(|s| s == "available"))
        .filter_map(|c| {
            let id = c["id"].as_str().filter(|id| token_like(id, 128))?;
            let ends = expiry(&c["expires_at"]);
            if ends.is_some_and(|ends| ends <= at) {
                return None;
            }
            Some(Reset {
                key: id.into(),
                id: id.into(),
                kind: "full",
                expires_at: ends.map(rfc3339),
                usable: true,
                blocked: None,
                detail: "Refills every Codex limit now. This cannot be undone.".into(),
            })
        })
        .collect();
    Status {
        eligible: true,
        reason: None,
        resets,
        checked_at: rfc3339(at),
    }
}

/// Status codes that prove the provider refused the claim before acting on it. A
/// timeout, a conflict, "too early" or a rate limit may mean the same request is still
/// being handled, so they stay unconfirmed and the request id is kept for a retry.
fn refused(status: u16, body: Option<&[u8]>) -> Option<Claim> {
    match status {
        401 => Some(Claim::failed(
            "The provider rejected the account's token (HTTP 401). Sign in again.",
        )),
        403 if body.is_some_and(|b| String::from_utf8_lossy(b).contains("scope requirement")) => Some(Claim::failed(
            "This sign-in cannot use resets. Sign in again through the dashboard.",
        )),
        400..=499 if !matches!(status, 408 | 409 | 425 | 429) => Some(Claim::failed(format!(
            "The provider refused the reset (HTTP {status})."
        ))),
        _ => None,
    }
}

fn claude_claim(status: u16, body: Option<&[u8]>) -> Claim {
    if let Some(claim) = refused(status, body) {
        return claim;
    }
    let Some(body) = body
        .filter(|_| (200..300).contains(&status))
        .and_then(|b| serde_json::from_slice::<Value>(b).ok())
    else {
        return Claim::new(Outcome::Unconfirmed);
    };
    let result = body["result"].as_str().unwrap_or_default();
    let code = if result == "unavailable" {
        body["reason"].as_str().unwrap_or_default()
    } else {
        result
    };
    let outcome = match code {
        "reset" | "already_used" => Outcome::Applied,
        "not_limited" => Outcome::NothingToReset,
        "ineligible" | "no_grant" | "unknown_grant" => Outcome::Gone,
        "not_next_grant" => Outcome::NotNext,
        "cooldown" | "paused" | "low_priority_active" => Outcome::Unavailable,
        _ => Outcome::Unconfirmed,
    };
    let mut claim = Claim::new(outcome);
    if outcome == Outcome::Unavailable {
        claim.until = expiry(&body["cooldown_until"]).map(rfc3339);
    }
    claim
}

/// Every consume answer arrives as HTTP 200 with the outcome in `code`.
fn codex_claim(status: u16, body: Option<&[u8]>) -> Claim {
    if let Some(claim) = refused(status, body) {
        return claim;
    }
    let code = body
        .filter(|_| (200..300).contains(&status))
        .and_then(|b| serde_json::from_slice::<Value>(b).ok())
        .and_then(|b| b["code"].as_str().map(str::to_owned));
    Claim::new(match code.as_deref() {
        Some("reset" | "already_redeemed") => Outcome::Applied,
        Some("nothing_to_reset") => Outcome::NothingToReset,
        Some("no_credit") => Outcome::Gone,
        _ => Outcome::Unconfirmed,
    })
}

#[cfg(test)]
#[path = "quota_resets_tests.rs"]
mod tests;
