/// Go `textproto.CanonicalMIMEHeaderKey`.
pub fn canonical_header(name: &str) -> String {
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
    if !valid {
        return name.to_owned();
    }
    let mut upper = true;
    name.chars()
        .map(|c| {
            let out = if upper {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            };
            upper = c == '-';
            out
        })
        .collect()
}

/// Headers in the order Go's HTTP/1.1 client writes them: Host, User-Agent (Go's
/// default when unset), Content-Length, the remaining request headers sorted by key
/// with their values in order, then the transport's Accept-Encoding.
#[derive(Clone, Default)]
pub struct GoHeaders {
    headers: Vec<(String, String)>,
    /// Go `http.Transport.DisableCompression`: the transport never asks for gzip.
    compression_disabled: bool,
    /// Send the first request's target exactly as the caller wrote the URL.
    pub(super) exact_target: bool,
}

impl GoHeaders {
    pub fn new() -> Self {
        Self::default()
    }

    /// `http.Header.Set`.
    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        let name = canonical_header(name);
        self.headers.retain(|(n, _)| *n != name);
        self.headers.push((name, value.into()));
    }

    /// Appends a value under a key that keeps its exact spelling (a raw `http.Header`
    /// map key in Go, written as stored).
    pub fn add_raw(&mut self, name: &str, value: impl Into<String>) {
        self.headers.push((name.to_owned(), value.into()));
    }

    /// Go `http.Transport.DisableCompression` (Devin's transport): no automatic
    /// `Accept-Encoding: gzip`, so responses are never decoded transparently.
    pub fn disable_compression(&mut self) {
        self.compression_disabled = true;
    }

    /// Go's `URL.RequestURI()`: the first request's target is the caller's URL as
    /// written, so `.` and `..` segments and the caller's escaping reach the upstream
    /// (Go's `PathEscape` output). Without it the URL is normalized as WHATWG parses it.
    /// A relative redirect from that request resolves against the path as written, as
    /// Go's `ResolveReference` does.
    pub fn exact_target(&mut self) {
        self.exact_target = true;
    }

    /// The headers as Go's `http.Header` holds them before sending (`Header.Clone()`, what
    /// request logging records): set names canonical, raw names as written, values in
    /// order. The transport's own lines (Host from the URL, the default User-Agent,
    /// Content-Length, Accept-Encoding) are not included.
    pub fn pairs(&self) -> &[(String, String)] {
        &self.headers
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        let name = canonical_header(name);
        self.headers.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_str())
    }

    /// Removes every value of `name` (`http.Header.Del`) and returns the first.
    pub fn take(&mut self, name: &str) -> Option<String> {
        let name = canonical_header(name);
        let mut first = None;
        self.headers.retain(|(n, v)| {
            if *n == name {
                first.get_or_insert_with(|| v.clone());
                false
            } else {
                true
            }
        });
        first
    }

    /// Applies the headers with their wire spelling. `order` is an exact header order
    /// (Claude's ordered request writer); `None` writes Go net/http's order. Returns
    /// whether the transport asked for gzip itself (no explicit Accept-Encoding or
    /// Range), which is the only case Go decodes transparently.
    pub fn apply(self, builder: wreq::RequestBuilder, order: Option<&[String]>) -> (wreq::RequestBuilder, bool) {
        self.apply_gzip(builder, order, true, false)
    }

    /// [`GoHeaders::apply`]; `gzip_allowed` is false for HEAD, which Go's transport
    /// never asks to compress, and `http2` picks Go's HTTP/2 default User-Agent.
    pub(super) fn apply_gzip(
        mut self,
        builder: wreq::RequestBuilder,
        order: Option<&[String]>,
        gzip_allowed: bool,
        http2: bool,
    ) -> (wreq::RequestBuilder, bool) {
        let auto_gzip = gzip_allowed
            && !self.compression_disabled
            && self.get("Accept-Encoding").is_none()
            && self.get("Range").is_none();
        // A custom Host header becomes the request Host (util.applyCustomHeaders).
        let host = self.take("Host").filter(|h| !h.is_empty());
        self.take("Content-Length");
        match self.get("User-Agent") {
            // net/http's defaultUserAgent, or x/net/http2's on an HTTP/2 connection.
            None => {
                let ua = if http2 {
                    "Go-http-client/2.0"
                } else {
                    "Go-http-client/1.1"
                };
                self.headers.push(("User-Agent".into(), ua.into()));
            }
            // Go writes no User-Agent line for an empty value (`Header["User-Agent"] = []string{""}`).
            Some("") => {
                self.take("User-Agent");
            }
            Some(_) => {}
        }
        let mut wire = wreq::header::OrigHeaderMap::new();
        match order {
            Some(order) => {
                for name in order {
                    wire.insert(name.clone());
                }
            }
            None => {
                wire.insert("Host");
                wire.insert("User-Agent");
                wire.insert("Content-Length");
                let mut rest: Vec<&String> = self
                    .headers
                    .iter()
                    .map(|(n, _)| n)
                    .filter(|n| *n != "User-Agent")
                    .collect();
                rest.sort();
                rest.dedup();
                for name in rest {
                    wire.insert(name.clone());
                }
            }
        }
        if auto_gzip {
            wire.insert("Accept-Encoding");
            self.headers.push(("Accept-Encoding".into(), "gzip".into()));
        }
        let mut builder = builder.orig_headers(wire).default_headers(false);
        if let Some(host) = host {
            builder = builder.header("Host", host);
        }
        for (name, value) in self.headers {
            builder = builder.header(name, value);
        }
        (builder, auto_gzip)
    }
}
