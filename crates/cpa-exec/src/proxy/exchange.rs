use std::sync::Mutex;

use bytes::Bytes;
use cpa_core::exec::{ExecError, FailureScope, UpstreamStage, UsageSink};
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use http::HeaderMap;

use super::GoHeaders;
use super::timing::{Counted, DIALLED, TimedBody};

/// How one request hop is sent: its client, and an exact header order for Claude's
/// native transport (`None` is Go net/http's order). Go's `http.Client` picks the
/// round tripper per hop, so a redirect can change both.
pub struct Route {
    pub client: wreq::Client,
    pub order: Option<Vec<String>>,
}

/// A response with Go net/http semantics: the body is decoded only for gzip the
/// transport requested itself, and is never framed or buffered here.
pub struct Upstream {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: BoxStream<'static, Result<Bytes, ExecError>>,
}

/// Largest error body kept from an upstream rejection.
// ponytail: Go reads error bodies without a bound; 16 MiB keeps a hostile upstream from
// exhausting a small VPS while covering any real provider error. Raise it if one is larger.
pub const MAX_ERROR_BODY: usize = 16 << 20;

fn body_error(_: std::io::Error) -> ExecError {
    ExecError::local(502, FailureScope::Request, "upstream request failed")
}

/// POSTs `body` the way Go's `http.Client.Do` does, following redirects itself: at
/// most ten requests, 301/302/303 turn a POST into a body-less GET, 307/308 resend the
/// body, the initial headers are copied to every hop (credentials only within the
/// original domain), a custom Host survives only relative redirects, and the previous
/// URL becomes `Referer`. `timeout` bounds the exchange.
pub async fn send(
    client: &wreq::Client,
    url: &str,
    headers: GoHeaders,
    body: impl Into<Bytes>,
    timeout: Option<std::time::Duration>,
) -> Result<Upstream, ExecError> {
    request(client, wreq::Method::POST, url, headers, Some(body.into()), timeout).await
}

/// [`send`] for any method: `http.NewRequest(method, url, body)` then `Client.Do`, with
/// Go's redirect rules for that method (a GET follows 301-303 and 307/308 as a GET).
/// `None` is Go's nil body.
pub async fn request(
    client: &wreq::Client,
    method: wreq::Method,
    url: &str,
    headers: GoHeaders,
    body: Option<Bytes>,
    timeout: Option<std::time::Duration>,
) -> Result<Upstream, ExecError> {
    let route = |_: &url::Url| {
        Ok(Route {
            client: client.clone(),
            order: None,
        })
    };
    send_request(&route, method, url, headers, body, timeout).await
}

/// [`send`] with the client and header order chosen per hop. With `timing`, the
/// exchange reports its stages to the attempt's usage record ([`UpstreamStage`]).
pub async fn send_routed(
    route: &(dyn Fn(&url::Url) -> Result<Route, ExecError> + Sync),
    url: &str,
    headers: GoHeaders,
    body: Bytes,
    timeout: Option<std::time::Duration>,
    timing: Option<&UsageSink>,
) -> Result<Upstream, ExecError> {
    let raw = exchange(
        route,
        wreq::Method::POST,
        url,
        headers,
        Some(body),
        timeout,
        true,
        timing,
    )
    .await;
    upstream(raw)
}

/// [`send_routed`] for any method. `None` is Go's nil body: no body and no
/// Content-Length (management `api-call`).
pub async fn send_request(
    route: &(dyn Fn(&url::Url) -> Result<Route, ExecError> + Sync),
    method: wreq::Method,
    url: &str,
    headers: GoHeaders,
    body: Option<Bytes>,
    timeout: Option<std::time::Duration>,
) -> Result<Upstream, ExecError> {
    upstream(exchange(route, method, url, headers, body, timeout, true, None).await)
}

/// What [`send_request`] makes of an exchange: errors and body errors sanitised.
fn upstream(raw: Result<RawUpstream, SendError>) -> Result<Upstream, ExecError> {
    raw.map(RawUpstream::sanitized).map_err(sanitize)
}

/// The error [`send_request`] reports for a [`SendError`].
fn sanitize(error: SendError) -> ExecError {
    match error {
        SendError::Transport { error, .. } => crate::upstream::transport_error(error),
        SendError::Local { error, .. } => error,
        // A prior redirect may already have sent inference; keep deadlines terminal.
        SendError::Timeout { .. } => ExecError::local(502, FailureScope::Request, "upstream request failed"),
    }
}

/// Why [`send_request_raw`] failed: the transport's own error (which may name the URL,
/// so callers decide what to show), a locally generated one, or `Timeout`: the
/// deadline passed before a hop was sent, so nothing was sent, and callers report it
/// as they report the transport's own timeout. `url` is the URL Go's client names in
/// its `*url.Error`: the failing hop (the caller's URL as written for the first
/// request), or the rejected `Location` when the redirect limit is hit.
// ponytail: wreq has no public constructor for its timeout error, so `Timeout` cannot be
// a `Transport`; each renderer repeats what it makes of one. Fold it into `Transport`
// if wreq ever exposes that constructor.
#[derive(Debug)]
pub enum SendError {
    Transport { error: wreq::Error, url: String },
    Local { error: ExecError, url: String },
    Timeout { url: String },
}

/// [`Upstream`] with the transport's raw body errors and the response's protocol
/// version (Go's response reader keeps framing headers by version).
pub struct RawUpstream {
    pub status: u16,
    pub version: http::Version,
    pub headers: HeaderMap,
    pub body: BoxStream<'static, Result<Bytes, std::io::Error>>,
}

impl RawUpstream {
    /// The [`Upstream`] [`send_request`] returns: body errors sanitised.
    pub(super) fn sanitized(self) -> Upstream {
        Upstream {
            status: self.status,
            headers: self.headers,
            body: crate::upstream::body_after_response(self.body.map(|r| r.map_err(body_error)).boxed()),
        }
    }

    /// [`Self::sanitized`] for a body read line by line, as Go's `bufio.Scanner` reads
    /// it: a body the upstream ended early fails with the scanner's `unexpected EOF`;
    /// every other read failure keeps `upstream request failed`.
    // ponytail: Go names resets and timeouts by their own error text, which can carry
    // the upstream URL; those keep this crate's generic text. Keep the client error text
    // if callers need Go's.
    pub(super) fn scanned(self) -> Upstream {
        let map = |error: std::io::Error| {
            if ended_early(&error) {
                ExecError::local(502, FailureScope::Transport, "unexpected EOF")
            } else {
                body_error(error)
            }
        };
        Upstream {
            status: self.status,
            headers: self.headers,
            body: crate::upstream::body_after_response(self.body.map(move |r| r.map_err(map)).boxed()),
        }
    }
}

/// Whether a body read failed because the upstream ended the body before its length or
/// final chunk (the transport's `UnexpectedEof`, Go's `io.ErrUnexpectedEOF`), rather
/// than a reset or a timeout.
fn ended_early(error: &std::io::Error) -> bool {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = cause {
        if current
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof)
        {
            return true;
        }
        cause = current.source();
    }
    false
}

/// [`send_request`] without error sanitising, for callers that report Go's own error
/// text (the plugin host's `host.http.*`).
pub async fn send_request_raw(
    route: &(dyn Fn(&url::Url) -> Result<Route, ExecError> + Sync),
    method: wreq::Method,
    url: &str,
    headers: GoHeaders,
    body: Option<Bytes>,
    timeout: Option<std::time::Duration>,
) -> Result<RawUpstream, SendError> {
    exchange(route, method, url, headers, body, timeout, true, None).await
}

/// [`send`] for a response body the caller reads line by line: a body cut short fails
/// with Go's `unexpected EOF` ([`RawUpstream::scanned`]).
pub async fn send_scanned(
    client: &wreq::Client,
    url: &str,
    headers: GoHeaders,
    body: Bytes,
    timeout: Option<std::time::Duration>,
) -> Result<Upstream, ExecError> {
    let route = |_: &url::Url| {
        Ok(Route {
            client: client.clone(),
            order: None,
        })
    };
    exchange(
        &route,
        wreq::Method::POST,
        url,
        headers,
        Some(body),
        timeout,
        true,
        None,
    )
    .await
    .map(RawUpstream::scanned)
    .map_err(sanitize)
}

/// [`request`] without following redirects: a client whose `CheckRedirect` returns
/// `http.ErrUseLastResponse` hands back the first answer, 3xx included. Untimed: it
/// reports no stages (Antigravity's grounding redirects).
pub async fn request_once(
    client: &wreq::Client,
    method: wreq::Method,
    url: &str,
    headers: GoHeaders,
    body: Option<Bytes>,
    timeout: Option<std::time::Duration>,
) -> Result<Upstream, ExecError> {
    let route = |_: &url::Url| {
        Ok(Route {
            client: client.clone(),
            order: None,
        })
    };
    upstream(exchange(&route, method, url, headers, body, timeout, false, None).await)
}

/// One request and, with `follow_redirects`, the redirects it is answered with, as
/// Go's `Client.Do`; without, the first answer, 3xx included. With `timing`, the
/// exchange reports its stages to the attempt's usage record ([`UpstreamStage`]).
#[allow(clippy::too_many_arguments)]
async fn exchange(
    route: &(dyn Fn(&url::Url) -> Result<Route, ExecError> + Sync),
    method: wreq::Method,
    url: &str,
    headers: GoHeaders,
    body: Option<Bytes>,
    timeout: Option<std::time::Duration>,
    follow_redirects: bool,
    timing: Option<&UsageSink>,
) -> Result<RawUpstream, SendError> {
    let local = |error: ExecError, url: &str| SendError::Local {
        error,
        url: url.to_owned(),
    };
    let initial = url::Url::parse(url).map_err(|_| {
        local(
            ExecError::local(500, FailureScope::Request, "invalid upstream URL"),
            url,
        )
    })?;
    let explicit_referer = headers.get("Referer").map(str::to_owned);
    // The caller's URL as written, for the first request only (GoHeaders::exact_target);
    // a URL with user info falls back to the parsed form (Go strips it from Referer).
    let exact =
        (headers.exact_target && initial.username().is_empty() && initial.password().is_none()).then(|| url.to_owned());
    let mut current = initial.clone();
    // req.Host: the custom Host of the current hop, if any.
    let mut host = headers.get("Host").filter(|h| !h.is_empty()).map(str::to_owned);
    let mut method = method;
    // Go's redirect `includeBody` says whether a 301/302/303 has dropped the body; it
    // starts true even for a nil body.
    let has_body = body.is_some();
    let mut include_body = true;
    let body = body.unwrap_or_default();
    let mut strip_sensitive = false;
    let mut hop_headers = headers.clone();
    let mut sent: u32 = 0;
    // Go's `Client.Timeout`: one deadline for every hop, taken before the first. Cost:
    // one `Instant` per request and one clock read per hop.
    // ponytail: wreq starts the body's share of a hop timeout afresh when the headers
    // arrive, so the final body can take up to that hop's budget again (the whole
    // exchange stays under twice `timeout`). Bounding it by the deadline needs a second
    // timer per response; add one if a caller ever reads long bodies under a timeout.
    let deadline = timeout.and_then(|t| std::time::Instant::now().checked_add(t));
    if let Some(timing) = timing {
        timing.upstream_stage(UpstreamStage::Started);
    }
    let (response, auto_gzip, dialled) = loop {
        // The URL Go's client reports for this hop.
        let hop_url = if sent == 0 { url.to_owned() } else { current.to_string() };
        let hop = route(&current).map_err(|mut error| {
            if sent > 0 {
                error.scope = FailureScope::Request;
            }
            local(error, &hop_url)
        })?;
        let builder = hop
            .client
            .request(
                method.clone(),
                exact.as_deref().filter(|_| sent == 0).unwrap_or(current.as_str()),
            )
            .redirect(wreq::redirect::Policy::none());
        // Go's standard transport (no exact order) speaks HTTP/2 wherever ALPN picks it.
        let go_transport = hop.order.is_none();
        let (builder, auto_gzip) = hop_headers.clone().apply_gzip(
            builder,
            hop.order.as_deref(),
            method != wreq::Method::HEAD,
            go_transport && expects_http2(&current),
        );
        let sends_body = has_body && include_body && !body.is_empty();
        // A timed body is watched where the hop speaks HTTP/1.1: Claude's native
        // transport, and plain `http://`, which Go's transport never upgrades (no h2c).
        // ponytail: an `https://` hop on the standard transport may speak HTTP/2, where
        // wreq resends a request a graceful GOAWAY refused only if its body is a plain
        // one, so those hops report no `Connected` or `BodySent`. Watching them needs a
        // resendable watched body in wreq.
        let watched = timing.filter(|_| !go_transport || current.scheme() == "http");
        let builder = if let (true, Some(timing)) = (sends_body, watched) {
            builder.body(wreq::Body::wrap(TimedBody::new(body.clone(), timing.clone(), sent)))
        } else if sends_body {
            builder.body(body.clone())
        } else if matches!(method.as_str(), "POST" | "PUT" | "PATCH") {
            // transferWriter.shouldSendContentLength: these methods always carry a length,
            // so a nil or empty body still says Content-Length: 0.
            builder.header(http::header::CONTENT_LENGTH, "0")
        } else {
            builder
        };
        // Last, so the hop's setup counts too. Go sends nothing once the deadline has
        // passed; wreq would send even with a zero timeout: it polls the request before
        // its timer, which fires on a 1 ms tick.
        let builder = match deadline.map(|d| d.saturating_duration_since(std::time::Instant::now())) {
            Some(remaining) if remaining.is_zero() => return Err(SendError::Timeout { url: hop_url }),
            Some(remaining) => builder.timeout(remaining),
            None => builder,
        };
        // The hop goes out now: what an earlier hop measured no longer applies. Its
        // body reports nothing before the client polls it.
        if let Some(timing) = timing {
            timing.upstream_stage(UpstreamStage::Hop {
                hop: sent,
                body: sends_body,
            });
        }
        // A timed hop learns from the connection hook whether it dialled.
        let (response, dialled) = match timing {
            Some(_) => {
                DIALLED
                    .scope(std::cell::Cell::new(false), async {
                        let response = builder.send().await;
                        (response, DIALLED.with(std::cell::Cell::get))
                    })
                    .await
            }
            None => (builder.send().await, false),
        };
        let response = response.map_err(|error| {
            if sent == 0 {
                SendError::Transport {
                    error,
                    url: hop_url.clone(),
                }
            } else {
                // An earlier redirect hop may have consumed the inference request.
                local(
                    ExecError::local(502, FailureScope::Request, "upstream request failed"),
                    &hop_url,
                )
            }
        })?;
        if go_transport {
            remember_protocol(&current, response.version() == http::Version::HTTP_2);
        }
        sent += 1;
        let status = response.status().as_u16();
        let location = response
            .headers()
            .get(http::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        if !follow_redirects || !matches!(status, 301 | 302 | 303 | 307 | 308) || location.is_empty() {
            break (response, auto_gzip, dialled);
        }
        let from_exact = exact.as_deref().filter(|_| sent == 1);
        let next = match from_exact {
            Some(raw) => resolve_exact(raw, &location, &current),
            None => current.join(&location),
        }
        .map_err(|_| {
            local(
                ExecError::local(
                    500,
                    FailureScope::Request,
                    format!("failed to parse Location header {location:?}"),
                ),
                &hop_url,
            )
        })?;
        // defaultCheckRedirect: len(via) >= 10.
        if sent >= 10 {
            return Err(local(
                ExecError::local(500, FailureScope::Request, "stopped after 10 redirects"),
                &location,
            ));
        }
        if (301..=303).contains(&status) {
            include_body = false;
            if method != wreq::Method::GET && method != wreq::Method::HEAD {
                method = wreq::Method::GET;
            }
        }
        if !strip_sensitive && initial.host_str() != next.host_str() {
            let (sub, parent) = (
                next.host_str().unwrap_or_default(),
                initial.host_str().unwrap_or_default(),
            );
            let subdomain = !sub.contains([':', '%'])
                && sub.len() > parent.len()
                && sub.ends_with(parent)
                && sub.as_bytes()[sub.len() - parent.len() - 1] == b'.';
            strip_sensitive = sub != parent && !subdomain;
        }
        hop_headers = headers.clone();
        if strip_sensitive {
            for name in [
                "Authorization",
                "Www-Authenticate",
                "Cookie",
                "Cookie2",
                "Proxy-Authorization",
                "Proxy-Authenticate",
            ] {
                hop_headers.take(name);
            }
        }
        if !include_body {
            for name in [
                "Content-Encoding",
                "Content-Language",
                "Content-Location",
                "Content-Type",
            ] {
                hop_headers.take(name);
            }
        }
        // Only a relative Location keeps the current hop's custom Host (go.dev/issue/22233).
        hop_headers.take("Host");
        host = host.filter(|_| url::Url::parse(&location).is_err());
        if let Some(host) = &host {
            hop_headers.set("Host", host.clone());
        }
        hop_headers.take("Referer");
        if !(current.scheme() == "https" && next.scheme() == "http") {
            let referer = explicit_referer
                .clone()
                .or_else(|| from_exact.map(str::to_owned))
                .unwrap_or_else(|| {
                    let mut last = current.clone();
                    let _ = last.set_username("");
                    let _ = last.set_password(None);
                    last.to_string()
                });
            hop_headers.set("Referer", referer);
        }
        current = next;
    };
    let status = response.status().as_u16();
    let version = response.version();
    let mut headers = response.headers().clone();
    let gzip = auto_gzip
        && headers
            .get(http::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("gzip"));
    let stream = response.bytes_stream().map(|r| r.map_err(std::io::Error::other));
    let stream = match timing {
        Some(timing) => {
            timing.upstream_stage(UpstreamStage::Headers { dialled });
            futures_util::future::Either::Left(Counted::new(stream, timing.clone()))
        }
        None => futures_util::future::Either::Right(stream),
    };
    let body = if gzip {
        headers.remove(http::header::CONTENT_ENCODING);
        headers.remove(http::header::CONTENT_LENGTH);
        let reader = tokio::io::BufReader::new(tokio_util::io::StreamReader::new(stream));
        let mut decoder = async_compression::tokio::bufread::GzipDecoder::new(reader);
        decoder.multiple_members(true);
        tokio_util::io::ReaderStream::new(decoder).boxed()
    } else {
        stream.boxed()
    };
    Ok(RawUpstream {
        status,
        version,
        headers,
        body,
    })
}

/// Origins (`scheme://host:port`) and whether their last answer came over HTTP/2.
/// Go's default User-Agent names the negotiated protocol, which wreq only reports
/// after the request is written.
fn origin_protocols() -> &'static Mutex<std::collections::HashMap<String, bool>> {
    static PROTOCOLS: std::sync::OnceLock<Mutex<std::collections::HashMap<String, bool>>> = std::sync::OnceLock::new();
    PROTOCOLS.get_or_init(Mutex::default)
}

fn origin(url: &url::Url) -> String {
    format!(
        "{}://{}:{}",
        url.scheme(),
        url.host_str().unwrap_or_default(),
        url.port_or_known_default().unwrap_or_default()
    )
}

/// Whether Go's transport would speak HTTP/2 to `url`: what its origin last answered
/// with; never over plain HTTP (no h2c).
// ponytail: an origin not seen yet counts as HTTP/1.1, so the first request to an h2
// upstream says Go-http-client/1.1 where Go says /2.0 (only callers that set no
// User-Agent). The protocol itself is always the negotiated one. Set the header after
// ALPN if that first request must match.
fn expects_http2(url: &url::Url) -> bool {
    url.scheme() == "https"
        && origin_protocols()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&origin(url))
            .copied()
            .unwrap_or(false)
}

fn remember_protocol(url: &url::Url, http2: bool) {
    let mut protocols = origin_protocols()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Bounded: forgetting only brings back the first-request assumption.
    if protocols.len() >= 1024 {
        protocols.clear();
    }
    protocols.insert(origin(url), http2);
}

/// Go `URL.ResolveReference` against the base URL as written (`Response.Location`):
/// the scheme and authority stay; the path is Go's `resolvePath` over the written base
/// path, so dot segments merge as written and a merged `//` path stays a path; the base
/// query survives only a reference with neither path nor query. Absolute and
/// network-path references ignore the base.
// ponytail: the next hop is a `url::Url`, which reads `%2e` as a dot segment where Go
// keeps it; only the first request goes out exactly as written. Build the next hop
// from the raw Location if a redirect target carries `%2e`.
fn resolve_exact(raw: &str, location: &str, current: &url::Url) -> Result<url::Url, url::ParseError> {
    if location.starts_with("//") || url::Url::parse(location).is_ok() {
        return current.join(location);
    }
    let (reference, fragment) = match location.split_once('#') {
        Some((reference, fragment)) => (reference, Some(fragment)),
        None => (location, None),
    };
    let (ref_path, ref_query) = match reference.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (reference, None),
    };
    // Go's parse order: the fragment, then the query at the first `?`, then the
    // authority (up to the first `/`) after the scheme.
    let base = raw.split_once('#').map_or(raw, |(base, _)| base);
    let (base, base_query) = match base.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (base, None),
    };
    let rest = base.find("://").map_or(base, |i| &base[i + 3..]);
    let base_path = rest.find('/').map_or("", |i| &rest[i..]);
    let mut next = current.clone();
    next.set_path(&go_resolve_path(base_path, ref_path));
    if ref_path.is_empty() && ref_query.is_none() {
        next.set_query(base_query);
    } else {
        next.set_query(ref_query);
    }
    next.set_fragment(fragment);
    Ok(next)
}

/// Go `net/url.resolvePath`: merge `reference` with `base`, then remove dot segments.
pub(super) fn go_resolve_path(base: &str, reference: &str) -> String {
    let full = if reference.is_empty() {
        base.to_owned()
    } else if !reference.starts_with('/') {
        let i = base.rfind('/').map_or(0, |i| i + 1);
        format!("{}{reference}", &base[..i])
    } else {
        reference.to_owned()
    };
    if full.is_empty() {
        return String::new();
    }
    let mut dst = String::with_capacity(full.len() + 1);
    dst.push('/');
    let mut first = true;
    let mut last = "";
    for elem in full.split('/') {
        last = elem;
        match elem {
            "." => first = false,
            ".." => {
                match dst[1..].rfind('/') {
                    Some(i) => dst.truncate(i + 1),
                    None => dst.truncate(1),
                }
                first = dst.len() == 1;
            }
            _ => {
                if !first {
                    dst.push('/');
                }
                dst.push_str(elem);
                first = false;
            }
        }
    }
    if last == "." || last == ".." {
        dst.push('/');
    }
    // An initial `/` was written; never two.
    if dst.len() > 1 && dst.as_bytes()[1] == b'/' {
        dst.remove(0);
    }
    dst
}

/// `io.ReadAll` up to `limit` bytes; with `lossy`, a read error keeps what arrived
/// (Go's error paths ignore it).
pub async fn read_all(
    mut body: BoxStream<'static, Result<Bytes, ExecError>>,
    limit: usize,
    lossy: bool,
) -> Result<Bytes, ExecError> {
    let mut out = bytes::BytesMut::new();
    while out.len() < limit {
        match body.next().await {
            Some(Ok(chunk)) => out.extend_from_slice(&chunk[..chunk.len().min(limit - out.len())]),
            Some(Err(_)) if lossy => break,
            Some(Err(error)) => return Err(error),
            None => break,
        }
    }
    Ok(out.freeze())
}
