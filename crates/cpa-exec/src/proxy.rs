//! Proxy-aware HTTP clients with Go net/http semantics, shared by every executor
//! (helps/proxy_helpers.go, sdk/proxyutil, net/http client behaviour).
//!
//! - [`Proxy`]: the effective proxy for a request (credential, then
//!   `requests.proxy-url`), parsed like `proxyutil.Parse`.
//! - [`GoClients`]: clients keyed by effective proxy in a bounded 64-entry LRU, with
//!   Go's standard-transport proxy rules (empty inherits environment proxies, `direct`
//!   and `none` bypass them, an unusable URL falls back to the default transport).
//! - [`send`] and [`GoHeaders`]: one exchange the way `http.Client.Do` performs it:
//!   HTTP/1.1 header line order, custom Host, manual redirects with Referer and
//!   credential stripping, and transparent gzip only when Go would have asked for it.
//! - [`lines`]: `bufio.Scanner` line semantics for event streams.
//!
//! Claude's first-party Node/OpenSSL profile lives in tls.rs; it uses [`Proxy`] and
//! [`Hooks`] from here so both kinds of client share proxy and test routing.
//!
//! ponytail: no request-scoped proxy override (Go `RequestProxyURL` / `opts.ProxyURL`);
//! ExecRequest carries none. Add a field to the execution envelope when a route needs it.

use std::net::SocketAddr;
use std::sync::Mutex;

use cpa_core::config::Config;
use cpa_core::credential::Credential;
use wreq::tls::trust::CertStore;

mod env_proxy;
mod exchange;
mod headers;
mod scanner;
mod timing;

pub(crate) use env_proxy::EnvProxy;
use env_proxy::wreq_proxy_url;
pub use exchange::{
    MAX_ERROR_BODY, RawUpstream, Route, SendError, Upstream, read_all, request, request_once, send, send_request,
    send_request_raw, send_routed, send_scanned,
};
pub use headers::{GoHeaders, canonical_header};
pub use scanner::lines;
pub(crate) use timing::watch_connections;

/// Bounded like Go's executor transport caches.
pub(crate) const CACHE_CAPACITY: usize = 64;

/// Test-only routing: extra trust roots replace the default store, and resolve
/// overrides pin logical hosts to local addresses while URL, Host and SNI stay
/// unchanged. Production uses [`Hooks::default`].
#[derive(Clone, Default)]
pub struct Hooks {
    pub trust: Option<CertStore>,
    pub resolve: Vec<(String, SocketAddr)>,
}

impl Hooks {
    pub(crate) fn apply(&self, mut builder: wreq::ClientBuilder) -> wreq::ClientBuilder {
        if let Some(trust) = &self.trust {
            builder = builder.tls_cert_store(trust.clone());
        }
        for (host, addr) in &self.resolve {
            builder = builder.resolve(host.clone(), *addr);
        }
        builder
    }
}

/// Effective proxy for one request (`effectiveProxyURL` + `proxyutil.Parse`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Proxy {
    /// Nothing configured.
    Inherit,
    /// `direct` / `none`: bypass every proxy, including environment ones.
    Direct,
    /// http, https, socks5 or socks5h URL, as configured.
    Url(String),
    /// Unparseable or unsupported: Go logs it and behaves as if none were configured.
    Invalid,
}

impl Proxy {
    pub fn parse(raw: &str) -> Self {
        let raw = raw.trim();
        if raw.is_empty() {
            return Self::Inherit;
        }
        if raw.eq_ignore_ascii_case("direct") || raw.eq_ignore_ascii_case("none") {
            return Self::Direct;
        }
        match url::Url::parse(raw) {
            Ok(u)
                if u.host_str().is_some_and(|h| !h.is_empty())
                    && matches!(u.scheme(), "http" | "https" | "socks5" | "socks5h") =>
            {
                Self::Url(raw.into())
            }
            _ => Self::Invalid,
        }
    }

    /// Credential `proxy_url` (attribute, then file metadata), then `requests.proxy-url`.
    pub fn effective(credential: &Credential, cfg: &Config) -> Self {
        Self::parse(&Self::effective_url(credential, cfg))
    }

    /// The setting `effective` parses, trimmed (Go `proxyURLForAuth`).
    pub fn effective_url(credential: &Credential, cfg: &Config) -> String {
        let own = credential
            .attributes
            .get("proxy_url")
            .map(String::as_str)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| credential.str("proxy_url"))
            .map(str::trim)
            .unwrap_or_default();
        if !own.is_empty() {
            return own.to_owned();
        }
        cfg.document
            .get("requests")
            .and_then(|r| r.get("proxy-url"))
            .and_then(serde_yaml_ng::Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned()
    }

    /// Applies this proxy to a client. `inherit_env` mirrors Go's standard transport,
    /// which honours environment proxies when nothing is configured; the Claude uTLS
    /// transports dial directly instead.
    pub fn apply(&self, builder: wreq::ClientBuilder, inherit_env: bool) -> wreq::Result<wreq::ClientBuilder> {
        match self {
            Proxy::Url(url) => Ok(builder.proxy(wreq::Proxy::all(wreq_proxy_url(url).as_str())?)),
            Proxy::Inherit | Proxy::Invalid if inherit_env => EnvProxy::current().apply(builder),
            _ => Ok(builder.no_proxy()),
        }
    }
}

/// `proxyutil.Redact`: a proxy URL safe for logs (no user info).
pub fn redact(raw: &str) -> String {
    match url::Url::parse(raw.trim()) {
        Ok(mut u) => {
            if !u.username().is_empty() || u.password().is_some() {
                let _ = u.set_username("xxxxx");
                let _ = u.set_password(None);
            }
            u.to_string()
        }
        Err(_) => "<invalid proxy url>".into(),
    }
}

/// Go standard-transport clients keyed by effective proxy, most recently used first.
pub struct GoClients {
    hooks: Hooks,
    /// Injected default (tests point it at local mocks); used for Inherit/Invalid.
    default: Option<wreq::Client>,
    cache: Mutex<Vec<(Proxy, wreq::Client)>>,
    /// Never offer HTTP/2 (Go transports with `ForceAttemptHTTP2` off and an empty
    /// `TLSNextProto`, as Antigravity's).
    http1: bool,
    /// Idle connections kept per host and how long (Go `MaxIdleConnsPerHost` and
    /// `IdleConnTimeout`); `None` keeps wreq's defaults.
    pool: Option<(usize, Option<std::time::Duration>)>,
    /// The clients tell timed requests whether they dialled ([`watch_connections`]).
    timed: bool,
}

impl GoClients {
    pub fn new(hooks: Hooks) -> Self {
        Self {
            hooks,
            default: None,
            cache: Mutex::default(),
            http1: false,
            pool: None,
            timed: false,
        }
    }

    /// Clients keeping at most `max_idle_per_host` idle connections per host (0 closes
    /// each connection after its response), each for at most `idle_timeout`. Additive
    /// (Antigravity's connection pool settings).
    pub fn pool(mut self, max_idle_per_host: usize, idle_timeout: Option<std::time::Duration>) -> Self {
        self.pool = Some((max_idle_per_host, idle_timeout));
        self
    }

    /// Clients that speak only HTTP/1.1. Additive (Antigravity).
    pub fn http1_only(mut self) -> Self {
        self.http1 = true;
        self
    }

    /// Every unproxied request uses `client` (local mock servers in tests).
    pub fn with_default(client: wreq::Client) -> Self {
        Self {
            hooks: Hooks::default(),
            default: Some(client),
            cache: Mutex::default(),
            http1: false,
            pool: None,
            timed: false,
        }
    }

    /// Clients for an executor that times its requests (`send_routed` with a usage
    /// sink): they report whether a request dialled its connection.
    pub fn timed(mut self) -> Self {
        self.timed = true;
        self
    }

    pub fn get(&self, proxy: &Proxy) -> wreq::Client {
        if let (Some(default), Proxy::Inherit | Proxy::Invalid) = (&self.default, proxy) {
            return default.clone();
        }
        if matches!(proxy, Proxy::Invalid) {
            tracing::warn!("unusable proxy configuration; using the default transport");
        }
        self.try_get(proxy).unwrap_or_else(|| {
            tracing::warn!(proxy = %match proxy { Proxy::Url(u) => redact(u), _ => String::new() }, "proxy client failed; using the default transport");
            self.default.clone().unwrap_or_else(|| plain_client(self.timed))
        })
    }

    /// The client for exactly `proxy`, or `None` when it cannot be built; never falls
    /// back to another transport (management `api-call` applies Go's own fallbacks).
    pub fn try_get(&self, proxy: &Proxy) -> Option<wreq::Client> {
        let mut cache = self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(i) = cache.iter().position(|(p, _)| p == proxy) {
            let entry = cache.remove(i);
            let client = entry.1.clone();
            cache.insert(0, entry);
            return Some(client);
        }
        let mut builder = wreq::Client::builder()
            .redirect(wreq::redirect::Policy::none())
            .retry(wreq::retry::Policy::never());
        if self.timed {
            builder = watch_connections(builder);
        }
        let mut builder = self.hooks.apply(builder);
        if self.http1 {
            builder = builder.http1_only();
        }
        if let Some((max_idle, idle_timeout)) = self.pool {
            builder = builder.pool_max_idle_per_host(max_idle).pool_idle_timeout(idle_timeout);
        }
        let built = proxy.apply(builder, true).and_then(wreq::ClientBuilder::build);
        let client = built.ok()?;
        cache.insert(0, (proxy.clone(), client.clone()));
        cache.truncate(CACHE_CAPACITY);
        Some(client)
    }
}

/// A plain client whose redirects are followed by [`send`], not by wreq, with Go's
/// environment proxy rules.
pub fn default_client() -> wreq::Client {
    plain_client(false)
}

fn plain_client(timed: bool) -> wreq::Client {
    let mut builder = wreq::Client::builder()
        .redirect(wreq::redirect::Policy::none())
        .retry(wreq::retry::Policy::never());
    if timed {
        builder = watch_connections(builder);
    }
    EnvProxy::current()
        .apply(builder)
        .and_then(wreq::ClientBuilder::build)
        .expect("default HTTP client")
}

#[cfg(test)]
#[path = "proxy_tests.rs"]
mod tests;
