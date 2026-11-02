//! Connection-level proxy dialing, Go's `proxyutil.BuildDialer`: SOCKS5 as
//! golang.org/x/net/proxy speaks it, and HTTP or HTTPS `CONNECT` (`httpConnectDialer`).
//! The media relay's TCP tunnels reach the upstream's candidate through it
//! (tcp_proxy.go).
//!
//! The proxy URL is read the way Go's `net/url` reads it (credentials are bytes, and
//! which URLs fail differs from the WHATWG parser), and the `CONNECT` reply the way
//! `http.ReadResponse` does.

use std::net::SocketAddr;

use btls::ssl::{SslConnector, SslMethod, SslVersion};
use btls::x509::verify::X509CheckFlags;
use cpa_core::config::go_url;
use futures_util::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// A byte stream to the dialed target.
pub(super) trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}
pub(super) type Conn = Box<dyn Stream>;

/// Go's `proxy.ContextDialer`; dropping the future cancels the dial.
pub(super) trait Dial: Send + Sync {
    fn dial(&self, target: SocketAddr) -> BoxFuture<'_, Result<Conn, String>>;
}

/// Go `url.Userinfo`: username, and the password when the URL had a ':'.
type User = (Vec<u8>, Option<Vec<u8>>);

enum Kind {
    Socks5,
    /// `CONNECT`, over TLS to the proxy for `https`.
    Connect(Option<SslConnector>),
}

/// The dialer for one `http`, `https`, `socks5` or `socks5h` proxy.
pub(super) struct ProxyDialer {
    kind: Kind,
    /// `u.Hostname()` and the port Go dials (`u.Port()` or the scheme's default).
    host: String,
    port: String,
    user: Option<User>,
}

/// `proxyutil.BuildDialer`: `None` when the setting means no proxy (empty, `direct`,
/// `none`), else the dialer or `proxyutil.Parse`'s error.
pub(super) fn build(raw: &str) -> Result<Option<ProxyDialer>, String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("direct") || raw.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    let url = go_url::parse(raw).map_err(|_| "parse proxy URL failed".to_owned())?;
    if url.scheme.is_empty() || url.host.is_empty() {
        return Err("proxy URL missing scheme/host".into());
    }
    let (host, port) = (url.hostname, url.port);
    let (kind, default_port) = match url.scheme.as_str() {
        "socks5" | "socks5h" => (Kind::Socks5, "1080"),
        "http" => (Kind::Connect(None), "80"),
        "https" => (
            Kind::Connect(Some(
                connector().map_err(|e| format!("HTTPS proxy TLS setup failed: {e}"))?,
            )),
            "443",
        ),
        other => return Err(format!("unsupported proxy scheme: {other}")),
    };
    Ok(Some(ProxyDialer {
        kind,
        host,
        port: if port.is_empty() { default_port.into() } else { port },
        user: url.user.map(|u| (u.username, u.password)),
    }))
}

impl ProxyDialer {
    async fn connect(&self, target: SocketAddr) -> Result<Conn, String> {
        let proxy = join_host_port(&self.host, &self.port);
        // Go dials the port text; Rust needs it as a number first.
        let port: Result<u16, String> = self
            .port
            .parse()
            .map_err(|_| format!("dial tcp: address {}: invalid port", self.port));
        match &self.kind {
            Kind::Socks5 => {
                let fail = |e: String| format!("socks connect tcp {proxy}->{target}: {e}");
                let mut conn = tcp(&self.host, port.map_err(fail)?)
                    .await
                    .map_err(|e| fail(e.to_string()))?;
                socks5(&mut conn, target, self.user.as_ref()).await.map_err(fail)?;
                Ok(Box::new(conn))
            }
            Kind::Connect(tls) => {
                let fail = |e: String| format!("dial HTTP proxy failed: {e}");
                let tcp = tcp(&self.host, port.map_err(fail)?)
                    .await
                    .map_err(|e| fail(e.to_string()))?;
                let conn: Conn = match tls {
                    None => Box::new(tcp),
                    Some(connector) => Box::new(handshake(connector, &self.host, tcp).await?),
                };
                // Go `proxyAuthorization`: Basic over the raw `user:password` bytes.
                let auth = self.user.as_ref().map(|(user, password)| {
                    use base64::Engine as _;
                    let plain = [user.as_slice(), b":", password.as_deref().unwrap_or_default()].concat();
                    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(plain))
                });
                http_connect(conn, target, auth.as_deref()).await
            }
        }
    }

    /// Trusts `root` besides the system roots (tests serve a self-signed proxy).
    #[cfg(test)]
    pub fn trusting(mut self, root: btls::x509::X509) -> Self {
        if let Kind::Connect(Some(_)) = self.kind {
            let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
            builder.cert_store_mut().add_cert(root).unwrap();
            builder.set_alpn_protos(b"\x08http/1.1").unwrap();
            self.kind = Kind::Connect(Some(builder.build()));
        }
        self
    }

    /// What the dialer read from the URL: host, port, and the userinfo bytes.
    #[cfg(test)]
    pub fn parts(&self) -> (&str, &str, Option<&User>) {
        (&self.host, &self.port, self.user.as_ref())
    }
}

impl Dial for ProxyDialer {
    fn dial(&self, target: SocketAddr) -> BoxFuture<'_, Result<Conn, String>> {
        Box::pin(self.connect(target))
    }
}

/// Go's `proxyScheme`: the lower-cased scheme for log lines, or `proxy`.
pub(super) fn proxy_scheme(raw: &str) -> String {
    match raw.trim().find("://") {
        Some(index) if index > 0 => raw.trim()[..index].to_ascii_lowercase(),
        _ => "proxy".into(),
    }
}

/// Go's TCP dial of `host:port`; an empty host is the local system.
async fn tcp(host: &str, port: u16) -> std::io::Result<TcpStream> {
    if host.is_empty() {
        TcpStream::connect((std::net::Ipv4Addr::UNSPECIFIED, port)).await
    } else {
        TcpStream::connect((host, port)).await
    }
}

/// Go `net.JoinHostPort`.
fn join_host_port(host: &str, port: &str) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// x/net `socks.Dialer.connect` with `UsernamePassword.Authenticate`.
async fn socks5(conn: &mut TcpStream, target: SocketAddr, user: Option<&User>) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    let greeting: &[u8] = if user.is_some() { &[5, 2, 0, 2] } else { &[5, 1, 0] };
    conn.write_all(greeting).await.map_err(io)?;
    let mut reply = [0u8; 2];
    conn.read_exact(&mut reply).await.map_err(io)?;
    if reply[0] != 5 {
        return Err(format!("unexpected protocol version {}", reply[0]));
    }
    if reply[1] == 0xff {
        return Err("no acceptable authentication methods".into());
    }
    if let Some((name, password)) = user {
        let password = password.as_deref().unwrap_or_default();
        match reply[1] {
            0 => {}
            2 => {
                if name.is_empty() || name.len() > 255 || password.len() > 255 {
                    return Err("invalid username/password".into());
                }
                let mut request = vec![1, name.len() as u8];
                request.extend_from_slice(name);
                request.push(password.len() as u8);
                request.extend_from_slice(password);
                conn.write_all(&request).await.map_err(io)?;
                conn.read_exact(&mut reply).await.map_err(io)?;
                if reply[0] != 1 {
                    return Err("invalid username/password version".into());
                }
                if reply[1] != 0 {
                    return Err("username/password authentication failed".into());
                }
            }
            other => return Err(format!("unsupported authentication method {other}")),
        }
    }
    let mut request = vec![5, 1, 0];
    match target.ip() {
        std::net::IpAddr::V4(ip) => {
            request.push(1);
            request.extend_from_slice(&ip.octets());
        }
        std::net::IpAddr::V6(ip) => {
            request.push(4);
            request.extend_from_slice(&ip.octets());
        }
    }
    request.extend_from_slice(&target.port().to_be_bytes());
    conn.write_all(&request).await.map_err(io)?;
    let mut head = [0u8; 4];
    conn.read_exact(&mut head).await.map_err(io)?;
    if head[0] != 5 {
        return Err(format!("unexpected protocol version {}", head[0]));
    }
    if head[1] != 0 {
        return Err(format!("unknown error {}", socks_reply(head[1])));
    }
    if head[2] != 0 {
        return Err("non-zero reserved field".into());
    }
    let bound = match head[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut len = [0u8; 1];
            conn.read_exact(&mut len).await.map_err(io)?;
            usize::from(len[0])
        }
        other => return Err(format!("unknown address type {other}")),
    };
    let mut rest = vec![0u8; bound + 2];
    conn.read_exact(&mut rest).await.map_err(io)?;
    Ok(())
}

/// x/net `socks.Reply.String`.
fn socks_reply(code: u8) -> String {
    match code {
        1 => "general SOCKS server failure".into(),
        2 => "connection not allowed by ruleset".into(),
        3 => "network unreachable".into(),
        4 => "host unreachable".into(),
        5 => "connection refused".into(),
        6 => "TTL expired".into(),
        7 => "command not supported".into(),
        8 => "address type not supported".into(),
        other => format!("unknown code: {other}"),
    }
}

/// The client TLS settings Go's `httpConnectDialer` uses for an `https` proxy: system
/// roots, TLS 1.2 or later, ALPN http/1.1.
fn connector() -> Result<SslConnector, btls::error::ErrorStack> {
    let mut builder = SslConnector::builder(SslMethod::tls())?;
    builder.set_min_proto_version(Some(SslVersion::TLS1_2))?;
    builder.set_alpn_protos(b"\x08http/1.1")?;
    Ok(builder.build())
}

async fn handshake(
    connector: &SslConnector,
    host: &str,
    tcp: TcpStream,
) -> Result<tokio_btls::SslStream<TcpStream>, String> {
    let fail = |e: &dyn std::fmt::Display| format!("HTTPS proxy TLS handshake failed: {e}");
    let mut ssl = connector
        .configure()
        .and_then(|c| c.into_ssl(host))
        .map_err(|e| fail(&e))?;
    // Go matches names against SANs only (BoringSSL would also try the subject CN).
    ssl.param_mut()
        .set_hostflags(X509CheckFlags::NO_PARTIAL_WILDCARDS | X509CheckFlags::NEVER_CHECK_SUBJECT);
    let mut stream = tokio_btls::SslStream::new(ssl, tcp).map_err(|e| fail(&e))?;
    std::pin::Pin::new(&mut stream).connect().await.map_err(|e| fail(&e))?;
    Ok(stream)
}

/// `httpConnectDialer.DialContext` after the proxy connection is up: the request as Go's
/// `Request.Write` renders it, then `http.ReadResponse`. Bytes the proxy sent after its
/// headers stay in front of the tunnel (Go's `bufferedConn`).
async fn http_connect(mut conn: Conn, target: SocketAddr, auth: Option<&str>) -> Result<Conn, String> {
    let mut request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nUser-Agent: Go-http-client/1.1\r\n");
    if let Some(auth) = auth {
        request.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    request.push_str("\r\n");
    conn.write_all(request.as_bytes())
        .await
        .map_err(|e| format!("write CONNECT request failed: {e}"))?;
    let mut reader = Lines {
        conn,
        buf: Vec::with_capacity(512),
        pos: 0,
    };
    let (code, status) = read_response(&mut reader)
        .await
        .map_err(|e| format!("read CONNECT response failed: {e}"))?;
    if code != 200 {
        return Err(format!("proxy CONNECT returned status {status}"));
    }
    let Lines { conn, mut buf, pos } = reader;
    if pos == buf.len() {
        return Ok(conn);
    }
    Ok(Box::new(Prefixed {
        early: buf.split_off(pos),
        offset: 0,
        inner: conn,
    }))
}

// ponytail: Go's reader has no limit on a response head; 64 KiB bounds a hostile proxy.
const MAX_HEAD: usize = 64 << 10;

/// `bufio.Reader.ReadLine` over the proxy connection.
struct Lines {
    conn: Conn,
    buf: Vec<u8>,
    pos: usize,
}

impl Lines {
    /// Reads more; false at EOF.
    async fn fill(&mut self) -> Result<bool, String> {
        if self.buf.len() > MAX_HEAD {
            return Err("message too large".into());
        }
        let mut chunk = [0u8; 1024];
        let n = self.conn.read(&mut chunk).await.map_err(|e| e.to_string())?;
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(n > 0)
    }

    /// A line without its `\n` or `\r\n`; a last unterminated line at EOF; `None` at EOF.
    async fn line(&mut self) -> Result<Option<Vec<u8>>, String> {
        loop {
            if let Some(nl) = self.buf[self.pos..].iter().position(|&b| b == b'\n') {
                let mut line = self.buf[self.pos..self.pos + nl].to_vec();
                self.pos += nl + 1;
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(line));
            }
            if !self.fill().await? {
                if self.pos == self.buf.len() {
                    return Ok(None);
                }
                let line = self.buf[self.pos..].to_vec();
                self.pos = self.buf.len();
                return Ok(Some(line));
            }
        }
    }

    async fn peek(&mut self) -> Result<Option<u8>, String> {
        while self.pos == self.buf.len() {
            if !self.fill().await? {
                return Ok(None);
            }
        }
        Ok(Some(self.buf[self.pos]))
    }
}

/// Go 1.26 `http.ReadResponse` for a `CONNECT` request: the status code and `resp.Status`.
async fn read_response(reader: &mut Lines) -> Result<(i64, String), String> {
    let eof = || "unexpected EOF".to_owned();
    let line = reader.line().await?.ok_or_else(eof)?;
    let line = String::from_utf8_lossy(&line).into_owned();
    let Some((proto, status)) = line.split_once(' ') else {
        return Err(format!("malformed HTTP response {}", quote(line.as_bytes())));
    };
    let status = status.trim_start_matches(' ');
    let code = status.split(' ').next().unwrap_or_default();
    let number = code.parse::<i64>().ok().filter(|n| code.len() == 3 && *n >= 0);
    let Some(number) = number else {
        return Err(format!("malformed HTTP status code {}", quote(code.as_bytes())));
    };
    let Some((major, minor)) = http_version(proto) else {
        return Err(format!("malformed HTTP version {}", quote(proto.as_bytes())));
    };
    let headers = read_headers(reader).await?;
    transfer_checks(&headers, major, minor)?;
    Ok((number, status.to_owned()))
}

/// `ParseHTTPVersion`.
fn http_version(proto: &str) -> Option<(u8, u8)> {
    let v = proto.as_bytes();
    (v.len() == 8 && proto.starts_with("HTTP/") && v[5].is_ascii_digit() && v[6] == b'.' && v[7].is_ascii_digit())
        .then(|| (v[5] - b'0', v[7] - b'0'))
}

/// textproto `readMIMEHeader`: canonical keys with their values, in order.
async fn read_headers(reader: &mut Lines) -> Result<Vec<(String, String)>, String> {
    let eof = || "unexpected EOF".to_owned();
    if let Some(b' ' | b'\t') = reader.peek().await? {
        let line = reader.line().await?.unwrap_or_default();
        return Err(format!("malformed MIME header initial line: {}", quote(&line)));
    }
    let mut headers = Vec::new();
    loop {
        let line = reader.line().await?.ok_or_else(eof)?;
        if line.is_empty() {
            return Ok(headers);
        }
        if !line.contains(&b':') {
            return Err(format!("malformed MIME header: missing colon: {}", quote(&line)));
        }
        let mut kv = trim(&line).to_vec();
        // Continuation lines (leading space or tab) fold into one, joined by a space.
        while let Some(b' ' | b'\t') = reader.peek().await? {
            while let Some(b' ' | b'\t') = reader.peek().await? {
                reader.pos += 1;
            }
            kv.push(b' ');
            match reader.line().await? {
                Some(more) => kv.extend_from_slice(trim(&more)),
                None => break,
            }
        }
        let colon = kv.iter().position(|&b| b == b':').unwrap_or(kv.len());
        let (key, value) = (&kv[..colon], kv.get(colon + 1..).unwrap_or_default());
        let malformed = || format!("malformed MIME header line: {}", quote(&kv));
        let key = canonical_key(key).ok_or_else(malformed)?;
        // Go's validHeaderValueByte: no control bytes but tab.
        if value.iter().any(|&c| (c < b' ' && c != b'\t') || c == 0x7f) {
            return Err(malformed());
        }
        let value = value
            .iter()
            .position(|&c| c != b' ' && c != b'\t')
            .map_or(&[][..], |start| &value[start..]);
        headers.push((key, String::from_utf8_lossy(value).into_owned()));
    }
}

/// `readTransfer`'s checks that can fail a `CONNECT` response.
fn transfer_checks(headers: &[(String, String)], major: u8, minor: u8) -> Result<(), String> {
    let values = |name: &str| -> Vec<&str> {
        headers
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .collect()
    };
    let list = |items: &[&str]| {
        format!(
            "[{}]",
            items.iter().map(|i| quote(i.as_bytes())).collect::<Vec<_>>().join(" ")
        )
    };
    // HTTP/0.0 counts as 1.1.
    let (major, minor) = if (major, minor) == (0, 0) {
        (1, 1)
    } else {
        (major, minor)
    };
    let mut chunked = false;
    let encodings = values("Transfer-Encoding");
    if !encodings.is_empty() && (major, minor) >= (1, 1) {
        if encodings.len() != 1 {
            return Err(format!("too many transfer encodings: {}", list(&encodings)));
        }
        if !encodings[0].eq_ignore_ascii_case("chunked") {
            return Err(format!(
                "unsupported transfer encoding: {}",
                quote(encodings[0].as_bytes())
            ));
        }
        chunked = true;
    }
    let lengths = values("Content-Length");
    if let Some(first) = lengths.first().map(|l| trim_string(l)) {
        if lengths[1..].iter().any(|l| trim_string(l) != first) {
            return Err(format!(
                "http: message cannot contain multiple Content-Length headers; got {}",
                list(&lengths)
            ));
        }
        if first.is_empty() {
            return Err(format!("invalid empty Content-Length {}", quote(b"")));
        }
        let valid = first.bytes().all(|b| b.is_ascii_digit()) && first.parse::<u64>().is_ok_and(|n| n < 1 << 63);
        if !valid {
            return Err(format!("bad Content-Length {}", quote(first.as_bytes())));
        }
    }
    if chunked {
        for value in values("Trailer") {
            for element in value.split(',').map(trim_string).filter(|e| !e.is_empty()) {
                let key = canonical_key(element.as_bytes()).unwrap_or_else(|| element.to_owned());
                if matches!(key.as_str(), "Transfer-Encoding" | "Trailer" | "Content-Length") {
                    return Err(format!("bad trailer key {}", quote(key.as_bytes())));
                }
            }
        }
    }
    Ok(())
}

/// textproto `trim`: spaces and tabs at both ends.
fn trim(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&c| c != b' ' && c != b'\t').unwrap_or(s.len());
    let end = s
        .iter()
        .rposition(|&c| c != b' ' && c != b'\t')
        .map_or(start, |e| e + 1);
    &s[start..end.max(start)]
}

/// textproto `TrimString`: ASCII whitespace at both ends.
fn trim_string(s: &str) -> &str {
    s.trim_matches([' ', '\t', '\n', '\r'])
}

/// `canonicalMIMEHeaderKey`: `None` for bytes a field name cannot hold; a key with a
/// space is accepted as written (go.dev/issue/34540).
fn canonical_key(key: &[u8]) -> Option<String> {
    let field = |c: u8| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c);
    if key.is_empty() || key.iter().any(|&c| !field(c) && c != b' ') {
        return None;
    }
    if key.contains(&b' ') {
        return Some(String::from_utf8_lossy(key).into_owned());
    }
    let mut upper = true;
    Some(
        key.iter()
            .map(|&c| {
                let c = if upper {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                };
                upper = c == b'-';
                c as char
            })
            .collect(),
    )
}

/// Go `%q` (`strconv.Quote`) for the bytes in an error message.
pub(super) fn quote(bytes: &[u8]) -> String {
    let mut out = String::from("\"");
    for c in String::from_utf8_lossy(bytes).chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A connection whose first bytes were already read (Go's `bufferedConn`).
struct Prefixed {
    early: Vec<u8>,
    offset: usize,
    inner: Conn,
}

impl AsyncRead for Prefixed {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.offset < self.early.len() {
            let n = buf.remaining().min(self.early.len() - self.offset);
            let start = self.offset;
            buf.put_slice(&self.early[start..start + n]);
            self.offset += n;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Prefixed {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
