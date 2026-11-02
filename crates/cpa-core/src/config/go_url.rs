//! Go `net/url.Parse` (Go 1.26.2+/1.27 `parseHost`: brackets only at the start of the host,
//! strict host colons for http and https), with Go's error kinds, plus Go's query and path
//! unescaping and `url.ParseQuery`. Config, credential, proxy and endpoint checks use it
//! where Go branches on a parse error; the `url` crate follows WHATWG and disagrees with
//! Go on inputs such as `https://%61uth.x.ai`, embedded tabs and `https:auth.x.ai`.

use std::net::IpAddr;

/// What callers read from a parsed URL.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoUrl {
    /// Lowercased scheme, empty for a relative reference.
    pub scheme: String,
    /// `URL.Host`: host and port as parsed (unescaped, IPv6 in brackets); empty without
    /// an authority.
    pub host: String,
    /// `URL.Hostname()`: host without port or IPv6 brackets.
    pub hostname: String,
    /// `URL.Port()`: the numeric port, empty when absent.
    pub port: String,
    /// `URL.User`.
    pub user: Option<Userinfo>,
    /// `URL.RawQuery`.
    pub raw_query: String,
    /// `URL.ForceQuery`: a `?` with nothing after it.
    pub force_query: bool,
    /// The fragment as written (`URL.EscapedFragment` for valid input).
    pub fragment: String,
    /// The path as written (`URL.EscapedPath` for valid input); empty for an opaque URL.
    pub raw_path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Userinfo {
    /// As written before the `@`.
    pub raw: String,
    pub username: Vec<u8>,
    pub password: Option<Vec<u8>>,
}

/// A `url.Parse` failure: the string Go quotes in `parse "...": ` and the cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub url: String,
    pub kind: ErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind {
    ControlCharacter,
    MissingScheme,
    ColonInFirstSegment,
    InvalidUserinfo,
    MissingBracket,
    InvalidIpLiteral,
    /// The `:port` text after the host.
    InvalidPort(Vec<u8>),
    /// The malformed `%` escape (up to three bytes).
    InvalidEscape(Vec<u8>),
    InvalidHostCharacter(u8),
    /// `netip.ParseAddr` refused the unescaped bracketed host.
    InvalidHost(String),
}

impl ParseError {
    /// Go's `*url.Error` text, with `quote` as `strconv.Quote`.
    pub fn message(&self, quote: impl Fn(&[u8]) -> String) -> String {
        let cause = match &self.kind {
            ErrorKind::ControlCharacter => "net/url: invalid control character in URL".to_owned(),
            ErrorKind::MissingScheme => "missing protocol scheme".to_owned(),
            ErrorKind::ColonInFirstSegment => "first path segment in URL cannot contain colon".to_owned(),
            ErrorKind::InvalidUserinfo => "net/url: invalid userinfo".to_owned(),
            ErrorKind::MissingBracket => "missing ']' in host".to_owned(),
            ErrorKind::InvalidIpLiteral => "invalid IP-literal".to_owned(),
            ErrorKind::InvalidPort(port) => format!("invalid port {} after host", quote(port)),
            ErrorKind::InvalidEscape(escape) => format!("invalid URL escape {}", quote(escape)),
            ErrorKind::InvalidHostCharacter(c) => format!("invalid character {} in host name", quote(&[*c])),
            // ponytail: netip.ParseAddr's detailed error text is not ported; the error says only
            // that the address is invalid. Port the text if a caller shows it.
            ErrorKind::InvalidHost(text) => {
                format!(
                    "invalid host: ParseAddr({}): unable to parse IP",
                    quote(text.as_bytes())
                )
            }
        };
        format!("parse {}: {cause}", quote(self.url.as_bytes()))
    }
}

type Result<T> = std::result::Result<T, ErrorKind>;

/// Go `url.Parse`.
pub fn parse(raw: &str) -> std::result::Result<GoUrl, ParseError> {
    let (u, frag) = raw.split_once('#').unwrap_or((raw, ""));
    let mut url = parse_inner(u).map_err(|kind| ParseError {
        url: u.to_owned(),
        kind,
    })?;
    if !frag.is_empty() {
        unescape_bytes(frag, Mode::Other).map_err(|kind| ParseError {
            url: raw.to_owned(),
            kind,
        })?;
    }
    url.fragment = frag.to_owned();
    Ok(url)
}

fn parse_inner(raw: &str) -> Result<GoUrl> {
    if raw.bytes().any(|b| b < b' ' || b == 0x7f) {
        return Err(ErrorKind::ControlCharacter);
    }
    if raw == "*" {
        return Ok(GoUrl {
            raw_path: "*".into(),
            ..GoUrl::default()
        });
    }
    let (scheme, rest) = get_scheme(raw)?;
    let mut url = GoUrl {
        scheme: scheme.to_ascii_lowercase(),
        ..GoUrl::default()
    };
    url.force_query = rest.ends_with('?') && rest.matches('?').count() == 1;
    let rest = if url.force_query {
        &rest[..rest.len() - 1]
    } else {
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        url.raw_query = query.to_owned();
        rest
    };
    if !rest.starts_with('/') {
        if !url.scheme.is_empty() {
            // A rootless path is opaque: no host, nothing else validated.
            return Ok(url);
        }
        if rest.split('/').next().unwrap_or_default().contains(':') {
            return Err(ErrorKind::ColonInFirstSegment);
        }
    }
    let mut path = rest;
    if (!url.scheme.is_empty() || !rest.starts_with("///")) && rest.starts_with("//") {
        let authority = &rest[2..];
        let (authority, tail) = authority.find('/').map_or((authority, ""), |i| authority.split_at(i));
        (url.user, url.host) = parse_authority(&url.scheme, authority)?;
        path = tail;
    }
    unescape_bytes(path, Mode::Other)?;
    url.raw_path = path.to_owned();
    (url.hostname, url.port) = split_host_port(&url.host);
    Ok(url)
}

fn get_scheme(raw: &str) -> Result<(&str, &str)> {
    for (i, c) in raw.bytes().enumerate() {
        match c {
            b'a'..=b'z' | b'A'..=b'Z' => {}
            b'0'..=b'9' | b'+' | b'-' | b'.' if i > 0 => {}
            b':' if i == 0 => return Err(ErrorKind::MissingScheme),
            b':' => return Ok((&raw[..i], &raw[i + 1..])),
            _ => return Ok(("", raw)),
        }
    }
    Ok(("", raw))
}

fn parse_authority(scheme: &str, authority: &str) -> Result<(Option<Userinfo>, String)> {
    let at = authority.rfind('@');
    let host = parse_host(scheme, at.map_or(authority, |i| &authority[i + 1..]))?;
    let Some(at) = at else {
        return Ok((None, host));
    };
    let raw = &authority[..at];
    let valid = raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-._:~!$&'()*+,;=%@".contains(c));
    if !valid {
        return Err(ErrorKind::InvalidUserinfo);
    }
    let (username, password) = match raw.split_once(':') {
        None => (unescape_bytes(raw, Mode::Other)?, None),
        Some((name, password)) => (
            unescape_bytes(name, Mode::Other)?,
            Some(unescape_bytes(password, Mode::Other)?),
        ),
    };
    let user = Userinfo {
        raw: raw.to_owned(),
        username,
        password,
    };
    Ok((Some(user), host))
}

fn parse_host(scheme: &str, host: &str) -> Result<String> {
    match host.rfind('[') {
        Some(open) if open > 0 => Err(ErrorKind::InvalidIpLiteral),
        Some(_) => {
            let close = host.rfind(']').ok_or(ErrorKind::MissingBracket)?;
            let colon_port = &host[close + 1..];
            if !valid_optional_port(colon_port) {
                return Err(ErrorKind::InvalidPort(colon_port.into()));
            }
            let port = unescape_bytes(colon_port, Mode::Host)?;
            let inner = &host[1..close];
            let mut unescaped = match inner.find("%25") {
                Some(zone) => {
                    let mut out = unescape_bytes(&inner[..zone], Mode::Host)?;
                    out.extend(unescape_bytes(&inner[zone..], Mode::Zone)?);
                    out
                }
                None => unescape_bytes(inner, Mode::Host)?,
            };
            // `netip.ParseAddr`: IPv6, optionally with a non-empty zone; IPv4 is refused.
            let text = String::from_utf8_lossy(&unescaped).into_owned();
            let (address, zone) = text
                .split_once('%')
                .map_or((text.as_str(), None), |(a, z)| (a, Some(z)));
            match address.parse::<IpAddr>() {
                Ok(IpAddr::V6(_)) if zone != Some("") => {}
                Ok(IpAddr::V4(_)) if zone.is_none() => return Err(ErrorKind::InvalidIpLiteral),
                _ => return Err(ErrorKind::InvalidHost(text)),
            }
            let mut out = Vec::with_capacity(unescaped.len() + port.len() + 2);
            out.push(b'[');
            out.append(&mut unescaped);
            out.push(b']');
            out.extend(port);
            Ok(String::from_utf8_lossy(&out).into_owned())
        }
        None => {
            if let Some(first) = host.find(':') {
                // Strict colons only for http and https; other schemes (database host
                // lists) take the port after the last colon.
                let i = if matches!(scheme, "http" | "https") {
                    first
                } else {
                    host.rfind(':').unwrap_or(first)
                };
                if !valid_optional_port(&host[i..]) {
                    return Err(ErrorKind::InvalidPort(host[i..].into()));
                }
            }
            unescape_bytes(host, Mode::Host).map(|b| String::from_utf8_lossy(&b).into_owned())
        }
    }
}

/// `splitHostPort` behind `URL.Hostname()` and `URL.Port()`.
pub fn split_host_port(host: &str) -> (String, String) {
    let (mut name, mut port) = (host, "");
    if let Some(colon) = name.rfind(':')
        && valid_optional_port(&name[colon..])
    {
        port = &name[colon + 1..];
        name = &name[..colon];
    }
    if let Some(inner) = name.strip_prefix('[').and_then(|n| n.strip_suffix(']')) {
        name = inner;
    }
    (name.to_owned(), port.to_owned())
}

fn valid_optional_port(port: &str) -> bool {
    port.is_empty()
        || port
            .strip_prefix(':')
            .is_some_and(|p| p.bytes().all(|b| b.is_ascii_digit()))
}

#[derive(PartialEq)]
enum Mode {
    Host,
    Zone,
    /// `encodeQueryComponent`: `+` is a space.
    Query,
    Other,
}

fn host_byte_ok(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!\"$&'()*+,-.:;<=>[]_~".contains(&b)
}

/// Go `unescape` validation and decoding: `%XX` must be two hex digits (never a sign).
fn unescape_bytes(s: &str, mode: Mode) -> Result<Vec<u8>> {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let escape = &b[i..b.len().min(i + 3)];
            let bad = || ErrorKind::InvalidEscape(escape.to_vec());
            let (hi, lo) = match escape {
                [_, hi, lo] => (hex(*hi).ok_or_else(bad)?, hex(*lo).ok_or_else(bad)?),
                _ => return Err(bad()),
            };
            let is_25 = escape == b"%25";
            if mode == Mode::Host && hi < 8 && !is_25 {
                return Err(bad());
            }
            let v = hi << 4 | lo;
            if mode == Mode::Zone && !is_25 && v != b' ' && !host_byte_ok(v) {
                return Err(bad());
            }
            out.push(v);
            i += 3;
            continue;
        }
        if mode == Mode::Query && b[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        if matches!(mode, Mode::Host | Mode::Zone) && b[i] < 0x80 && !host_byte_ok(b[i]) {
            return Err(ErrorKind::InvalidHostCharacter(b[i]));
        }
        out.push(b[i]);
        i += 1;
    }
    Ok(out)
}

/// Go `url.QueryUnescape`; `None` for a malformed escape.
pub fn query_unescape(s: &str) -> Option<Vec<u8>> {
    unescape_bytes(s, Mode::Query).ok()
}

/// Go `url.PathUnescape`; `None` for a malformed escape.
pub fn path_unescape(s: &str) -> Option<Vec<u8>> {
    unescape_bytes(s, Mode::Other).ok()
}

/// Decodes each well-formed `%XX` and keeps every other byte, `+` included: a request
/// path the HTTP server has already accepted.
pub fn percent_decode_lenient(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b.get(i + 1..i + 3) {
            Some(&[hi, lo]) if b[i] == b'%' && hi.is_ascii_hexdigit() && lo.is_ascii_hexdigit() => {
                let hex = |c: u8| (c as char).to_digit(16).unwrap_or_default() as u8;
                out.push(hex(hi) << 4 | hex(lo));
                i += 3;
            }
            _ => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    out
}

/// Go `shouldEscape`-style percent-encoding: unreserved bytes and `keep` stay, a space is
/// `+` when `plus_space`, every other byte is `%XX`.
fn escape(s: &[u8], keep: impl Fn(u8) -> bool, plus_space: bool) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &b in s {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') || keep(b) {
            out.push(char::from(b));
        } else if b == b' ' && plus_space {
            out.push('+');
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(b >> 4)]));
            out.push(char::from(HEX[usize::from(b & 15)]));
        }
    }
    out
}

/// Go `url.QueryEscape`: unreserved bytes kept, space as `+`, everything else `%XX`.
pub fn query_escape(s: impl AsRef<[u8]>) -> String {
    escape(s.as_ref(), |_| false, true)
}

/// Every byte but the unreserved ones as `%XX`, a space as `%20`: for decoders that
/// read only `%XX`.
pub fn escape_unreserved(s: impl AsRef<[u8]>) -> String {
    escape(s.as_ref(), |_| false, false)
}

/// Go `url.PathEscape`: a single path segment.
pub fn path_escape(s: impl AsRef<[u8]>) -> String {
    escape(
        s.as_ref(),
        |b| matches!(b, b'$' | b'&' | b'+' | b':' | b'=' | b'@'),
        false,
    )
}

/// Go `url.Values.Encode` for single-valued pairs: sorted by key, `key=value` joined with
/// `&`, both query-escaped.
pub fn values_encode(pairs: &[(&str, &str)]) -> String {
    let mut sorted: Vec<&(&str, &str)> = pairs.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    sorted
        .iter()
        .map(|(k, v)| format!("{}={}", query_escape(k), query_escape(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Go `url.ParseQuery` pairs in order, decoded. Pairs with a `;` or a malformed escape in
/// the name or value are skipped, as Go does.
pub fn parse_query(query: &str) -> impl Iterator<Item = (Vec<u8>, Vec<u8>)> + '_ {
    query
        .split('&')
        .filter(|p| !p.is_empty() && !p.contains(';'))
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            Some((query_unescape(k)?, query_unescape(v)?))
        })
}

/// `url.ParseQuery(query).Get(name)`: the first value whose decoded name matches.
pub fn query_get(query: &str, name: &str) -> Option<Vec<u8>> {
    parse_query(query).find_map(|(k, v)| (k == name.as_bytes()).then_some(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_and_rejects_like_go() {
        let host = |s: &str| parse(s).ok().map(|u| u.hostname);
        assert_eq!(
            host("https://u:p@API.Kimi.AI:8443/coding").as_deref(),
            Some("API.Kimi.AI")
        );
        assert_eq!(host("//sub.kimi.ai/x").as_deref(), Some("sub.kimi.ai"));
        assert_eq!(host("api.kimi.com/coding").as_deref(), Some(""));
        assert_eq!(host("https://[::1]:8080/x").as_deref(), Some("::1"));
        assert_eq!(host("https://[fe80::1%25en0]/").as_deref(), Some("fe80::1%en0"));
        for bad in [
            "https://api.kimi.ai/%zz",
            "https://api.kimi.com:abc/v1",
            "https://a:1:2/",
            "https://[1.2.3.4]/",
            "https://[::1/",
            "https://a b/",
            "https://a%41/",
            "https://u{@h/",
            ":x",
            "1a:b/c",
            "http://h/\x01",
            "http://h/#%zz",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert_eq!(parse("turn:%zz").map(|u| u.scheme), Ok("turn".into()));
        assert!(parse("postgres://a:1,b:2/db").is_ok());
        assert!(parse("http://h/?%zz").is_ok(), "Go leaves the query unvalidated");
    }

    #[test]
    fn host_rules_follow_go_parse_host() {
        let kind = |s: &str| parse(s).err().map(|e| e.kind);
        // A `[` anywhere but the start of the host is never an IP literal.
        assert_eq!(kind("https://a[::1]/"), Some(ErrorKind::InvalidIpLiteral));
        assert_eq!(kind(r"https://evil\[::1%25.x.ai]/"), Some(ErrorKind::InvalidIpLiteral));
        // An empty zone is refused by netip.ParseAddr.
        assert_eq!(
            kind("http://[fe80::1%25]/"),
            Some(ErrorKind::InvalidHost("fe80::1%".into()))
        );
        // Strict colons apply to http and https only.
        assert_eq!(kind("http://a:1:2/"), Some(ErrorKind::InvalidPort(b":1:2".to_vec())));
        let url = parse("socks5://u:p%40ss@a:1:1080").unwrap();
        assert_eq!(
            (url.host.as_str(), url.hostname.as_str(), url.port.as_str()),
            ("a:1:1080", "a:1", "1080")
        );
        let user = url.user.unwrap();
        assert_eq!(
            (user.raw.as_str(), user.username.as_slice(), user.password.as_deref()),
            ("u:p%40ss", &b"u"[..], Some(&b"p@ss"[..]))
        );
        assert!(parse("mongodb://a:1,b:2/db").is_ok());
    }

    #[test]
    fn errors_carry_go_text() {
        let quote = |b: &[u8]| format!("{:?}", String::from_utf8_lossy(b));
        let message = |s: &str| parse(s).unwrap_err().message(quote);
        assert_eq!(
            message("http://h/%zz"),
            r#"parse "http://h/%zz": invalid URL escape "%zz""#
        );
        assert_eq!(
            message("http://h/#%z"),
            r#"parse "http://h/#%z": invalid URL escape "%z""#
        );
        assert_eq!(
            message("https://a:x/"),
            r#"parse "https://a:x/": invalid port ":x" after host"#
        );
        assert_eq!(message(":x"), r#"parse ":x": missing protocol scheme"#);
    }

    #[test]
    fn unescape_requires_two_hex_digits() {
        for bad in ["%+5", "%-5", "%zz", "%5", "%"] {
            assert_eq!(query_unescape(bad), None, "{bad}");
            assert_eq!(path_unescape(bad), None, "{bad}");
            assert_eq!(percent_decode_lenient(bad), bad.as_bytes(), "{bad}");
        }
        assert_eq!(query_unescape("a%2Bb+c%FF").unwrap(), b"a+b c\xff");
        assert_eq!(path_unescape("a%2Bb+c").unwrap(), b"a+b+c");
        assert_eq!(percent_decode_lenient("a%2fb+%zz%41"), b"a/b+%zzA");
    }

    #[test]
    fn escaping_follows_go() {
        // url.QueryEscape("a b/c?d=é~") and url.Values{"z": {"1"}, "a b": {"x&y"}}.Encode().
        assert_eq!(query_escape("a b/c?d=\u{e9}~"), "a+b%2Fc%3Fd%3D%C3%A9~");
        assert_eq!(values_encode(&[("z", "1"), ("a b", "x&y")]), "a+b=x%26y&z=1");
        assert_eq!(escape_unreserved("a b+c/\u{ff}"), "a%20b%2Bc%2F%C3%BF");
        assert_eq!(query_escape(b"\xff"), "%FF");
        // url.PathEscape("a b/c;d,e?f$&+:=@").
        assert_eq!(path_escape("a b/c;d,e?f$&+:=@"), "a%20b%2Fc%3Bd%2Ce%3Ff$&+:=@");
    }

    #[test]
    fn parse_query_skips_malformed_pairs_like_go() {
        let pairs: Vec<_> = parse_query("a=1&b=%+5&c;=2&&d&a=%32&e=%zz").collect();
        assert_eq!(
            pairs,
            [
                (b"a".to_vec(), b"1".to_vec()),
                (b"d".to_vec(), vec![]),
                (b"a".to_vec(), b"2".to_vec())
            ]
        );
        assert_eq!(query_get("x=%-1&x=y+z", "x").unwrap(), b"y z");
        assert_eq!(query_get("x=1", "y"), None);
    }
}
