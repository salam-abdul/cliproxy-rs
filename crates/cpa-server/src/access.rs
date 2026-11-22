//! Client-key auth, matching CLIProxyAPI's config access provider
//! (internal/access/config_access/provider.go, sdk/access/manager.go).
//!
//! Candidates, in order: `Authorization` (bearer token or the raw value), `X-Goog-Api-Key`,
//! `X-Api-Key`, then the first `key` and `auth_token` query values. Comparison is on raw
//! bytes, like Go strings, in constant time per key. With no keys configured the
//! provider is unregistered and access is open, which [`refuse_open_access`] allows on
//! an exposed listener only with `access.allow-unauthenticated`. Deliberate difference:
//! on a loopback listener open access admits only requests that name this machine and
//! come from no other origin.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::uri::Authority;
use axum::http::{HeaderMap, Uri, header};
use axum::middleware::Next;
use axum::response::Response;
use cpa_core::config::Config;
use cpa_core::config::go_url::query_get;
use cpa_core::exec::Caller;
use subtle::ConstantTimeEq;

use crate::runtime::Runtime;

/// Whether the listener `config` describes takes connections from other machines: its
/// host is neither a loopback IP nor `localhost` (empty listens on every interface), or
/// trusted proxies are set, since a tunnel or reverse proxy in front of a loopback
/// listener is the documented way to expose it.
pub fn listener_exposed(config: &Config) -> bool {
    !config.trusted_proxies.is_empty() || !loopback_host(&config.host)
}

/// `localhost` or a loopback IP, without brackets.
fn loopback_host(host: &str) -> bool {
    host.strip_suffix('.').unwrap_or(host).eq_ignore_ascii_case("localhost")
        || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Whether `authority` (a `Host` value or a URI authority) names this machine.
fn loopback_authority(authority: &[u8]) -> bool {
    let Ok(authority) = Authority::try_from(authority) else {
        return false;
    };
    let host = authority.host();
    !authority.as_str().contains('@')
        && loopback_host(host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host))
}

const FOREIGN_HOST: &str = "Host is not this machine: add a client key to access.api-keys to use another name";
const FOREIGN_ORIGIN: &str =
    "Origin is not this machine: add a client key to access.api-keys to call from another site";

/// Open access on a loopback listener is meant for programs on this machine. A browser
/// here carries any page's requests to it, so a request that names another host (DNS
/// rebinding) or comes from another origin is refused. An exposed listener is open only
/// by `access.allow-unauthenticated`, which admits everyone already, and in Home mode
/// (`home`) Home authenticates every client itself.
fn open_access(config: &Config, home: bool, headers: &HeaderMap, uri: &Uri) -> Result<(), &'static str> {
    if home || listener_exposed(config) {
        return Ok(());
    }
    let hosts = headers.get_all(header::HOST).iter().map(|v| v.as_bytes());
    let named = uri.authority().map(|a| a.as_str().as_bytes());
    if !hosts.chain(named).all(loopback_authority) {
        return Err(FOREIGN_HOST);
    }
    let local = |origin: &[u8]| {
        origin
            .strip_prefix(b"http://")
            .or_else(|| origin.strip_prefix(b"https://"))
            .is_some_and(loopback_authority)
    };
    if !headers.get_all(header::ORIGIN).iter().all(|v| local(v.as_bytes())) {
        return Err(FOREIGN_ORIGIN);
    }
    Ok(())
}

/// Refuses `config` when it would let anyone who reaches an exposed listener use the
/// configured accounts: no client keys and no `access.allow-unauthenticated: true`.
pub fn refuse_open_access(config: &Config, exposed: bool) -> anyhow::Result<()> {
    if exposed && config.api_keys.is_empty() && !config.allow_unauthenticated {
        anyhow::bail!(
            "access.api-keys is empty and the listener is reachable from other machines \
             (server.host is not a loopback address, or server.trusted-proxies is set): \
             add a client key to access.api-keys, or set access.allow-unauthenticated: true \
             to serve the proxy API without client keys"
        );
    }
    Ok(())
}

/// Client-key authentication for HTTP routes.
pub async fn require_client_key(State(rt): State<Arc<Runtime>>, req: Request, next: Next) -> Response {
    let (mut req, access) = authenticate_request(&rt, req);
    let caller = match access {
        Ok(Access::Open) => Caller {
            principal: String::new(),
            source: "",
        },
        Ok(Access::Granted { caller, .. }) => caller,
        // gin `AbortWithStatusJSON(status, gin.H{"error": msg})`.
        Err(denied) => {
            let body = cpa_common::json::Object::new()
                .str("error", denied.message)
                .finish_string();
            return crate::respond::gin_json(denied.status, body);
        }
    };
    req.extensions_mut().insert(caller);
    next.run(req).await
}

pub(crate) fn authenticate(keys: &[String], headers: &HeaderMap, query: &str) -> Result<Caller, &'static str> {
    if keys.is_empty() {
        return Ok(Caller {
            principal: String::new(),
            source: "",
        });
    }
    let header = |name: &str| headers.get(name).map(|v| v.as_bytes()).unwrap_or_default();
    let authorization = header("authorization");
    let query_key = query_get(query, "key").unwrap_or_default();
    let query_token = query_get(query, "auth_token").unwrap_or_default();
    let bearer = bearer(authorization);
    let candidates: [(&[u8], &'static str); 5] = [
        (bearer, "authorization"),
        (header("x-goog-api-key"), "x-goog-api-key"),
        (header("x-api-key"), "x-api-key"),
        (&query_key, "query-key"),
        (&query_token, "query-auth-token"),
    ];
    // "Missing" is judged on the raw values, so `Authorization: Bearer ` is invalid, not missing.
    if authorization.is_empty() && candidates[1..].iter().all(|(v, _)| v.is_empty()) {
        return Err("Missing API key");
    }
    candidates
        .into_iter()
        .filter(|(value, _)| !value.is_empty())
        .find_map(|(value, source)| {
            keys.iter()
                .find(|k| bool::from(k.as_bytes().ct_eq(value)))
                .map(|k| Caller {
                    principal: k.clone(),
                    source,
                })
        })
        .ok_or("Invalid API key")
}

/// Go's extractBearerToken: "Bearer x" yields "x"; anything else is used verbatim.
fn bearer(header: &[u8]) -> &[u8] {
    match header.iter().position(|&b| b == b' ') {
        Some(i) if header[..i].eq_ignore_ascii_case(b"bearer") => header[i + 1..].trim_ascii(),
        _ => header,
    }
}

pub(crate) enum Access {
    /// No provider registered: Go's manager returns no result and sets nothing.
    Open,
    /// Go `sdkaccess.Result`: the caller and the provider identifier that accepted it.
    Granted { caller: Caller, provider: &'static str },
}

/// Go `sdkaccess.AuthError` as the middlewares answer it.
pub(crate) struct Denied {
    pub status: u16,
    pub message: &'static str,
}

pub(crate) fn authenticate_request(rt: &Runtime, req: Request) -> (Request, Result<Access, Denied>) {
    let config = rt.config();
    let result = if config.api_keys.is_empty() {
        open_access(&config, rt.remote_dispatch().is_some(), req.headers(), req.uri())
            .map(|()| Access::Open)
            .map_err(|message| Denied { status: 403, message })
    } else {
        authenticate(&config.api_keys, req.headers(), req.uri().query().unwrap_or_default())
            .map(|caller| Access::Granted {
                caller,
                provider: "config-inline",
            })
            .map_err(|message| Denied { status: 401, message })
    };
    (req, result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth_with(keys: &[&str], query: &str, headers: &[(&'static str, &str)]) -> Result<Caller, &'static str> {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            map.insert(*k, v.parse().unwrap());
        }
        let keys: Vec<String> = keys.iter().map(|k| (*k).to_owned()).collect();
        authenticate(&keys, &map, query)
    }

    fn auth(query: &str, headers: &[(&'static str, &str)]) -> Result<Caller, &'static str> {
        auth_with(&["good"], query, headers)
    }

    #[test]
    fn query_matches_go_parse_query() {
        let get = |q, n| query_get(q, n).map(|v| String::from_utf8(v).unwrap());
        assert_eq!(
            get("key=wrong&key=good", "key").as_deref(),
            Some("wrong"),
            "first value wins"
        );
        assert_eq!(get("%6bey=good", "key").as_deref(), Some("good"), "names are decoded");
        assert_eq!(
            get("key=%zz&key=good", "key").as_deref(),
            Some("good"),
            "malformed pair skipped"
        );
        assert_eq!(
            get("key=a;b&key=good", "key").as_deref(),
            Some("good"),
            "semicolon pair skipped"
        );
        assert_eq!(get("key=a+b%2Bc", "key").as_deref(), Some("a b+c"));
        assert_eq!(get("key", "key").as_deref(), Some(""));
        assert_eq!(
            query_get("key=%FF&key=good", "key"),
            Some(vec![0xFF]),
            "invalid UTF-8 still wins first"
        );
    }

    #[test]
    fn candidates_and_errors_match_go() {
        assert_eq!(auth("key=wrong&key=good", &[]), Err("Invalid API key"));
        assert_eq!(auth("key=%FF&key=good", &[]), Err("Invalid API key"));
        assert_eq!(auth("%6bey=good", &[]).unwrap().source, "query-key");
        assert_eq!(auth("", &[]), Err("Missing API key"));
        assert_eq!(auth("", &[("authorization", "Bearer ")]), Err("Invalid API key"));
        assert_eq!(auth("", &[("authorization", "good")]).unwrap().source, "authorization");
        let caller = auth("", &[("x-api-key", "nope"), ("x-goog-api-key", "good")]).unwrap();
        assert_eq!((caller.principal.as_str(), caller.source), ("good", "x-goog-api-key"));
        assert_eq!(auth_with(&[], "", &[]).unwrap().principal, "", "no keys means open");
    }

    #[test]
    fn an_exposed_listener_needs_client_keys_or_the_opt_in() {
        let check = |yaml: &str| {
            let config = Config::parse(yaml).unwrap();
            refuse_open_access(&config, listener_exposed(&config)).map_err(|e| e.to_string())
        };
        for exposed in [
            "server: {host: ''}\n",
            "server: {host: 0.0.0.0}\n",
            "server: {host: '::'}\n",
            "server: {host: 192.0.2.10}\n",
            "server: {host: localhost.example}\n",
            "server: {host: 127.0.0.1, trusted-proxies: [127.0.0.1]}\n",
            "server: {host: localhost, trusted-proxies: [127.0.0.1]}\n",
            "server: {host: '::1', trusted-proxies: ['::1']}\n",
        ] {
            let refused = check(exposed).expect_err(exposed);
            assert!(
                refused.contains("add a client key to access.api-keys")
                    && refused.contains("set access.allow-unauthenticated: true"),
                "{exposed}: {refused}"
            );
            check(&format!("{exposed}access: {{allow-unauthenticated: true}}\n")).unwrap();
            check(&format!("{exposed}access: {{api-keys: [fake-client]}}\n")).unwrap();
        }
        for loopback in [
            "server: {host: 127.0.0.1}\n",
            "server: {host: 127.0.0.2}\n",
            "server: {host: '::1'}\n",
            "server: {host: localhost}\n",
            "server: {host: LocalHost.}\n",
        ] {
            check(loopback).unwrap();
        }
    }

    #[test]
    fn open_access_on_loopback_admits_only_this_machine() {
        type Headers<'a> = &'a [(&'static str, &'a str)];
        let check = |yaml: &str, headers: Headers, uri: &str| {
            let mut map = HeaderMap::new();
            for (k, v) in headers {
                map.append(*k, v.parse().unwrap());
            }
            open_access(&Config::parse(yaml).unwrap(), false, &map, &uri.parse().unwrap())
        };
        const LOOPBACK: &str = "server: {host: 127.0.0.1}\n";
        let local: [Headers; 8] = [
            &[],
            &[("host", "127.0.0.1:8317")],
            &[("host", "localhost")],
            &[("host", "LocalHost.:8317")],
            &[("host", "[::1]:8317")],
            &[("host", "127.0.0.1:8317"), ("origin", "http://127.0.0.1:8317")],
            &[("host", "localhost:8317"), ("origin", "http://localhost:5173")],
            &[("host", "[::1]:8317"), ("origin", "https://[::1]")],
        ];
        for headers in local {
            assert_eq!(check(LOOPBACK, headers, "/v1/models"), Ok(()), "{headers:?}");
        }
        let foreign_hosts: [Headers; 5] = [
            &[("host", "rebind.example:8317")],
            &[("host", "localhost.example")],
            &[("host", "192.0.2.10:8317")],
            &[("host", "user@localhost")],
            &[("host", "localhost"), ("host", "rebind.example")],
        ];
        for headers in foreign_hosts {
            assert_eq!(check(LOOPBACK, headers, "/v1/models"), Err(FOREIGN_HOST), "{headers:?}");
        }
        assert_eq!(
            check(LOOPBACK, &[], "http://rebind.example/v1/models"),
            Err(FOREIGN_HOST),
            "an HTTP/2 authority counts as the host"
        );
        for origin in [
            "https://app.example",
            "http://localhost.example",
            "null",
            "file://",
            "chrome-extension://abcdefgh",
            "localhost:8317",
            "http://user@localhost",
        ] {
            let headers: Headers = &[("host", "127.0.0.1:8317"), ("origin", origin)];
            assert_eq!(check(LOOPBACK, headers, "/v1/models"), Err(FOREIGN_ORIGIN), "{origin}");
        }
        let two: Headers = &[("origin", "http://localhost"), ("origin", "https://app.example")];
        assert_eq!(check(LOOPBACK, two, "/v1/models"), Err(FOREIGN_ORIGIN));
        // An exposed listener is open to everyone by its own opt-in.
        let anyone: Headers = &[("host", "proxy.example"), ("origin", "https://app.example")];
        for exposed in [
            "server: {host: 0.0.0.0}\naccess: {allow-unauthenticated: true}\n",
            "server: {host: 127.0.0.1, trusted-proxies: [127.0.0.1]}\naccess: {allow-unauthenticated: true}\n",
        ] {
            assert_eq!(check(exposed, anyone, "/v1/models"), Ok(()), "{exposed}");
        }
        // Home authenticates every client, whatever the request names.
        let mut named = HeaderMap::new();
        named.insert("host", "proxy.example".parse().unwrap());
        named.insert("origin", "https://app.example".parse().unwrap());
        let (config, uri) = (Config::parse(LOOPBACK).unwrap(), "/v1/models".parse().unwrap());
        assert_eq!(open_access(&config, true, &named, &uri), Ok(()));
        assert_eq!(open_access(&config, false, &named, &uri), Err(FOREIGN_HOST));
    }

    #[test]
    fn replacement_char_key_does_not_match_raw_ff() {
        assert_eq!(auth_with(&["a\u{FFFD}"], "key=a%FF", &[]), Err("Invalid API key"));
        assert_eq!(
            auth_with(&["a\u{FFFD}"], "key=a%EF%BF%BD", &[]).unwrap().principal,
            "a\u{FFFD}"
        );
    }
}
