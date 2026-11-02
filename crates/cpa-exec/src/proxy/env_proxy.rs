/// Go's `http.ProxyFromEnvironment` (golang.org/x/net/http/httpproxy), read once like
/// Go's `envProxyFunc`: `HTTP_PROXY` for http and `HTTPS_PROXY` for https targets (upper
/// case first), never `ALL_PROXY`; `localhost` and loopback addresses always bypass;
/// `NO_PROXY` adds exclusions, and `*` disables both proxies.
///
/// ponytail: wreq's exclusion matcher stands in for Go's. Port-specific `NO_PROXY`
/// entries are dropped (Go bypasses only that port), a leading-dot entry also matches
/// the bare domain, and the CGI rule ignores `HTTP_PROXY` instead of failing requests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EnvProxy {
    pub(super) http: Option<String>,
    pub(super) https: Option<String>,
    /// wreq `NoProxy` list including Go's implicit loopback bypass; `None` for `*`.
    pub(super) no_proxy: Option<String>,
}

impl EnvProxy {
    pub(super) fn current() -> &'static Self {
        static ENV: std::sync::OnceLock<EnvProxy> = std::sync::OnceLock::new();
        ENV.get_or_init(|| Self::from_lookup(|name| std::env::var(name).ok()))
    }

    /// `FromEnvironment` + `config.init` over a variable lookup.
    pub(crate) fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let any = |names: [&str; 2]| names.iter().find_map(|n| get(n).filter(|v| !v.is_empty()));
        let cgi = get("REQUEST_METHOD").is_some_and(|v| !v.is_empty());
        let http = any(["HTTP_PROXY", "http_proxy"])
            .filter(|_| !cgi)
            .and_then(|v| env_proxy_url(&v));
        let https = any(["HTTPS_PROXY", "https_proxy"]).and_then(|v| env_proxy_url(&v));
        let mut entries: Vec<String> = ["localhost", "127.0.0.0/8", "::1", "::ffff:127.0.0.0/104"]
            .map(String::from)
            .into();
        let mut all = false;
        for entry in any(["NO_PROXY", "no_proxy"]).unwrap_or_default().split(',') {
            let entry = entry.trim().to_lowercase();
            if entry.is_empty() {
                continue;
            }
            if entry == "*" {
                all = true;
                break;
            }
            let bare_ip = entry.parse::<std::net::IpAddr>().is_ok() || entry.contains('/');
            let has_port = !bare_ip
                && entry
                    .rsplit_once(':')
                    .is_some_and(|(_, port)| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()));
            if has_port {
                continue;
            }
            entries.push(
                entry
                    .strip_prefix('*')
                    .filter(|e| e.starts_with('.'))
                    .unwrap_or(&entry)
                    .to_owned(),
            );
        }
        Self {
            http,
            https,
            no_proxy: (!all).then(|| entries.join(",")),
        }
    }

    pub(super) fn apply(&self, builder: wreq::ClientBuilder) -> wreq::Result<wreq::ClientBuilder> {
        let mut builder = builder.no_proxy();
        let Some(no_proxy) = &self.no_proxy else {
            return Ok(builder);
        };
        let exclusions = || wreq::NoProxy::from_string(no_proxy);
        if let Some(url) = &self.http {
            builder = builder.proxy(wreq::Proxy::http(url.as_str())?.no_proxy(exclusions()));
        }
        if let Some(url) = &self.https {
            builder = builder.proxy(wreq::Proxy::https(url.as_str())?.no_proxy(exclusions()));
        }
        Ok(builder)
    }
}

/// httpproxy `parseProxy`: a value without scheme or host is retried as `http://`.
fn env_proxy_url(raw: &str) -> Option<String> {
    let parsed = url::Url::parse(raw)
        .ok()
        .filter(|u| u.host_str().is_some_and(|h| !h.is_empty()))
        .or_else(|| url::Url::parse(&format!("http://{raw}")).ok())?;
    Some(wreq_proxy_url(parsed.as_str().trim_end_matches('/')))
}

/// Go's SOCKS5 dialer always sends the hostname to the proxy, so `socks5` resolves
/// remotely there; wreq does that only for `socks5h`.
pub(super) fn wreq_proxy_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("socks5") => format!("socks5h://{rest}"),
        _ => url.to_owned(),
    }
}
