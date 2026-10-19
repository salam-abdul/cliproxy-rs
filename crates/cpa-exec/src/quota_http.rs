use crate::proxy::SendError;
use crate::xai_url::{self as go_url, GoUrl};
use cpa_common::gostr::quote;
use std::collections::BTreeMap;
type Header = BTreeMap<String, Vec<String>>;

pub fn go_preflight(url: &str, parsed: &GoUrl, headers: &Header) -> Result<(), String> {
    let is_http = matches!(parsed.scheme.as_str(), "http" | "https");
    if is_http && let Some(invalid) = invalid_header(headers) {
        return Err(format!("net/http: invalid header {invalid}"));
    }
    if !is_http {
        return Err(format!("unsupported protocol scheme {}", quote(&parsed.scheme)));
    }
    if parsed.hostname.is_empty() && parsed.port.is_empty() {
        return Err("http: no Host in request URL".into());
    }
    if !go_url::same_authority(url) {
        return Err("unsupported URL: hosts differ between parsers".into());
    }
    Ok(())
}

/// The `*url.Error` text for a request Go's client refused: `Get "url": cause`.
pub fn go_url_error_text(method: &str, url: &str, cause: &str) -> String {
    format!("{} {}: {cause}", url_error_op(method), quote(strip_password(url)))
}

/// The error Go's response body reader reports for `err`.
pub fn go_read_error_text(err: &std::io::Error) -> String {
    go_read_error(err)
}

/// The `*url.Error` text Go's `http.Client.Do` returns for a failed send
/// (`Get "url": cause`), password masked.
pub fn go_url_error(method: &str, err: SendError) -> String {
    let (url, cause) = send_error_parts(err);
    format!("{} {}: {cause}", url_error_op(method), quote(strip_password(&url)))
}

/// Go `urlErrorOp`: the method with only its first letter upper case.
fn url_error_op(method: &str) -> String {
    if method.is_empty() {
        return "Get".into();
    }
    if method.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        let lower = method.to_ascii_lowercase();
        return format!("{}{}", &method[..1], &lower[1..]);
    }
    method.to_owned()
}

/// Go `stripPassword`: a password in the URL's authority (after `scheme://`, or a
/// leading `//`) prints as `***`.
fn strip_password(raw: &str) -> String {
    let start = match raw.find("://") {
        Some(i) => i + 3,
        None if raw.starts_with("//") => 2,
        None => return raw.to_owned(),
    };
    let rest = &raw[start..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let Some((userinfo, host)) = rest[..end].rsplit_once('@') else {
        return raw.to_owned();
    };
    match userinfo.split_once(':') {
        Some((user, _)) => format!("{}{user}:***@{host}{}", &raw[..start], &rest[end..]),
        None => raw.to_owned(),
    }
}

/// Go `validateHeaders` on the request: the first invalid name (or value, named by
/// its key) in key order.
fn invalid_header(headers: &Header) -> Option<String> {
    let token = |b: u8| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b);
    headers.iter().find_map(|(name, values)| {
        if name.is_empty() || !name.bytes().all(token) {
            return Some(format!("field name {}", quote(name)));
        }
        values
            .iter()
            .any(|v| v.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f))
            .then(|| format!("field value for {}", quote(name)))
    })
}

/// The error sources below `err`, looking through `io::Error` wrappers.
fn error_chain<'a>(err: &'a (dyn std::error::Error + 'static)) -> Vec<&'a (dyn std::error::Error + 'static)> {
    let mut out = Vec::new();
    let mut next = Some(err);
    while let Some(e) = next {
        out.push(e);
        next = match e.downcast_ref::<std::io::Error>().and_then(std::io::Error::get_ref) {
            Some(inner) => Some(inner as &(dyn std::error::Error + 'static)),
            None => e.source(),
        };
    }
    out
}

/// The URL Go's client names for a failed exchange, and the cause it reports.
// ponytail: the common transport failures are spelled as Go spells them (a refused
// dial, a connection closed before the response); any other reads as the transport's
// innermost error. A refused dial names the hop's host, which for a host name is not
// the address Go dialed.
fn send_error_parts(err: SendError) -> (String, String) {
    let (err, hop) = match err {
        SendError::Local { error, url } => return (url, String::from_utf8_lossy(&error.body).into_owned()),
        // What `transport_cause` reads in the transport's own timeout (wreq's text).
        SendError::Timeout { url } => return (url, "operation timed out".into()),
        SendError::Transport { error, url } => (error, url),
    };
    let cause = transport_cause(&err, go_url::parse(&hop).ok().as_ref());
    (hop, cause)
}

pub(crate) fn transport_cause(err: &wreq::Error, url: Option<&GoUrl>) -> String {
    let chain = error_chain(err);
    for e in &chain {
        if let Some(url) = url
            && e.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::ConnectionRefused)
        {
            let port = if url.port.is_empty() {
                if url.scheme == "https" { "443" } else { "80" }
            } else {
                url.port.as_str()
            };
            let host = if url.hostname.contains(':') {
                format!("[{}]", url.hostname)
            } else {
                url.hostname.clone()
            };
            return format!("dial tcp {host}:{port}: connect: connection refused");
        }
        if e.to_string().contains("connection closed before message completed") {
            return "EOF".into();
        }
    }
    chain.last().map_or_else(|| err.to_string(), ToString::to_string)
}

/// The error Go's body reader reports.
// ponytail: a truncated body (`unexpected EOF`) and a bad gzip header are spelled as Go
// spells them; any other failure reads as the innermost error.
pub(crate) fn go_read_error(err: &std::io::Error) -> String {
    let chain = error_chain(err);
    for e in &chain {
        if e.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof)
        {
            return "unexpected EOF".into();
        }
        if e.to_string().contains("invalid gzip header") {
            return "gzip: invalid header".into();
        }
    }
    chain.last().map_or_else(|| err.to_string(), ToString::to_string)
}
