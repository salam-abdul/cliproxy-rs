#[tokio::test]
async fn post_send_header_and_body_failures_are_terminal_without_hidden_replay() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for response in [
        "",
        "HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\npartial",
        "HTTP/1.1 503 Busy\r\nContent-Length: 20\r\n\r\npartial",
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/fixture", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(socket.read_u8().await.unwrap());
            }
            let mut body = [0; 6];
            socket.read_exact(&mut body).await.unwrap();
            assert_eq!(&body, b"secret");
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
            drop(socket);
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                    .await
                    .is_err(),
                "inference was replayed"
            );
        });
        let result = send(&default_client(), &url, GoHeaders::new(), "secret", None).await;
        let error = match result {
            Err(error) => error,
            Ok(upstream) => upstream
                .body
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .find_map(Result::err)
                .unwrap(),
        };
        assert_eq!(error.scope, FailureScope::Request);
        assert!(error.retry_after.is_none());
        server.await.unwrap();
    }
}

#[tokio::test]
async fn connection_failure_after_redirect_is_terminal() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_url = format!("http://{}/target", target.local_addr().unwrap());
    drop(target);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/fixture", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(socket.read_u8().await.unwrap());
        }
        let mut body = [0; 6];
        socket.read_exact(&mut body).await.unwrap();
        assert_eq!(&body, b"secret");
        socket
            .write_all(
                format!(
                    "HTTP/1.1 307 Redirect\r\nLocation: {target_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "first hop was replayed"
        );
    });
    let error = send(&default_client(), &url, GoHeaders::new(), "secret", None)
        .await
        .err()
        .unwrap();
    assert_eq!(error.scope, FailureScope::Request);
    assert!(error.retry_after.is_none());
    server.await.unwrap();
}

#[tokio::test]
async fn successful_body_reads_are_terminal_in_both_http_adapters() {
    for status in [200, 206, 299, 302, 400, 503] {
        for scanned in [false, true] {
            let body = futures_util::stream::iter([
                Ok(Bytes::from_static(b"partial")),
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "synthetic truncation",
                )),
            ])
            .boxed();
            let raw = RawUpstream {
                status,
                version: http::Version::HTTP_11,
                headers: HeaderMap::new(),
                body,
            };
            let upstream = if scanned { raw.scanned() } else { raw.sanitized() };
            let out = upstream.body.collect::<Vec<_>>().await;
            assert_eq!(out[0].as_ref().unwrap().as_ref(), b"partial");
            let error = out[1].as_ref().unwrap_err();
            assert_eq!(error.scope, FailureScope::Request, "status={status}, scanned={scanned}");
            assert_eq!(error.status, 502);
            assert_eq!(
                error.body.as_ref(),
                if scanned {
                    b"unexpected EOF".as_slice()
                } else {
                    b"upstream request failed"
                }
            );
            assert!(error.headers.is_empty());
        }
    }
}

use bytes::Bytes;
use cpa_core::exec::{ExecError, FailureScope, UpstreamStage, UsageSink};
use futures_util::StreamExt;
use http::HeaderMap;

use super::env_proxy::wreq_proxy_url;
use super::exchange::go_resolve_path;
use super::timing::BODY_PIECE;
use super::*;
use std::sync::Arc;

/// A body read line by line fails with Go's scanner text `unexpected EOF` only when
/// the upstream ended it early; a reset keeps the generic transport text. Every
/// reply sends five of the hundred bytes or a partial chunked body, then, once the
/// client holds the response, closes or resets the connection.
#[tokio::test]
async fn scanned_bodies_name_only_an_early_end_unexpected_eof() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    const LENGTH: &str = "HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nhello";
    const CHUNKED: &str = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n";
    for (head, reset, want) in [
        (LENGTH, false, "unexpected EOF"),
        (CHUNKED, false, "unexpected EOF"),
        (LENGTH, true, "upstream request failed"),
        (CHUNKED, true, "upstream request failed"),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let (holds_response, response_held) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = tokio::io::BufReader::new(socket);
            // The request has no body: its head ends with an empty line.
            let mut line = String::new();
            while socket.read_line(&mut line).await.unwrap() > 0 && line != "\r\n" {
                line.clear();
            }
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
            let _ = response_held.await;
            if reset {
                #[allow(deprecated)]
                socket.get_ref().set_linger(Some(std::time::Duration::ZERO)).unwrap();
            }
        });
        let client = wreq::Client::builder().build().unwrap();
        let upstream = send_scanned(&client, &url, GoHeaders::new(), Bytes::new(), None)
            .await
            .unwrap();
        holds_response.send(()).unwrap();
        let mut read = lines(upstream.body, 1 << 20);
        let error = loop {
            match read.next().await.expect("a read error") {
                Ok(_) => {}
                Err(error) => break error,
            }
        };
        assert_eq!(
            (
                String::from_utf8_lossy(&error.body).into_owned(),
                error.status,
                error.scope
            ),
            (want.to_owned(), 502, FailureScope::Request),
            "{head:?} reset={reset}"
        );
    }
}

#[test]
fn proxy_parsing_scheme_mapping_and_redaction() {
    assert_eq!(Proxy::parse(" "), Proxy::Inherit);
    assert_eq!(Proxy::parse("DIRECT"), Proxy::Direct);
    assert_eq!(Proxy::parse("none"), Proxy::Direct);
    assert_eq!(Proxy::parse("http://p:1"), Proxy::Url("http://p:1".into()));
    assert_eq!(Proxy::parse("socks5://u:p@p:1"), Proxy::Url("socks5://u:p@p:1".into()));
    assert_eq!(Proxy::parse("ftp://p"), Proxy::Invalid);
    assert_eq!(Proxy::parse("p:1"), Proxy::Invalid);
    assert_eq!(wreq_proxy_url("socks5://h:1"), "socks5h://h:1");
    assert_eq!(wreq_proxy_url("SOCKS5://h:1"), "socks5h://h:1");
    assert_eq!(wreq_proxy_url("socks5h://h:1"), "socks5h://h:1");
    assert_eq!(wreq_proxy_url("http://h:1"), "http://h:1");
    assert_eq!(redact("http://user:secret@p:1/"), "http://xxxxx@p:1/");
    assert!(!redact("socks5://user:secret@p:1").contains("secret"));
}

#[test]
fn effective_proxy_precedence() {
    let cfg = Config::parse("requests:\n  proxy-url: http://global:1\n").unwrap();
    let mut c = Credential::from_file(
        std::path::Path::new("/a"),
        std::path::Path::new("/a/c.json"),
        serde_json::json!({"type":"claude","proxy_url":"socks5://meta:2"})
            .as_object()
            .unwrap()
            .clone(),
    )
    .unwrap();
    assert_eq!(Proxy::effective(&c, &cfg), Proxy::Url("socks5://meta:2".into()));
    c.attributes.insert("proxy_url".into(), "direct".into());
    assert_eq!(Proxy::effective(&c, &cfg), Proxy::Direct);
    c.attributes.clear();
    c.metadata.remove("proxy_url");
    assert_eq!(Proxy::effective(&c, &cfg), Proxy::Url("http://global:1".into()));
}

#[test]
fn environment_proxies_follow_go_httpproxy() {
    let env = |vars: &[(&str, &str)]| {
        let vars: std::collections::HashMap<String, String> =
            vars.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        EnvProxy::from_lookup(|name| vars.get(name).cloned())
    };
    let e = env(&[
        ("ALL_PROXY", "http://all:1"),
        ("http_proxy", "lower:2"),
        ("HTTP_PROXY", "upper:3"),
        ("https_proxy", "socks5://s:4"),
        ("no_proxy", " Example.com, *.corp.test, 10.0.0.0/8, host:8080, ::2 "),
    ]);
    assert_eq!(
        e.http.as_deref(),
        Some("http://upper:3"),
        "upper case first; schemeless is http"
    );
    assert_eq!(e.https.as_deref(), Some("socks5h://s:4"));
    assert_eq!(
        e.no_proxy.as_deref(),
        Some("localhost,127.0.0.0/8,::1,::ffff:127.0.0.0/104,example.com,.corp.test,10.0.0.0/8,::2")
    );
    assert_eq!(
        env(&[("ALL_PROXY", "http://all:1")]),
        env(&[]),
        "ALL_PROXY is never read"
    );
    assert_eq!(env(&[("HTTP_PROXY", "http://p:1"), ("NO_PROXY", "a,*")]).no_proxy, None);
    assert_eq!(
        env(&[("HTTP_PROXY", "http://p:1"), ("REQUEST_METHOD", "GET")]).http,
        None
    );
}

#[tokio::test]
async fn environment_proxy_bypasses_loopback_and_proxies_the_rest() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    async fn first_line(listener: tokio::net::TcpListener) -> String {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0; 4096];
        let n = socket.read(&mut buf).await.unwrap();
        let _ = socket
            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
            .await;
        String::from_utf8_lossy(&buf[..n]).lines().next().unwrap().to_owned()
    }
    let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (proxy_addr, target_addr) = (proxy.local_addr().unwrap(), target.local_addr().unwrap());
    let env = EnvProxy::from_lookup(|name| (name == "HTTP_PROXY").then(|| format!("http://{proxy_addr}")));
    let client = env
        .apply(wreq::Client::builder().redirect(wreq::redirect::Policy::none()))
        .unwrap()
        .build()
        .unwrap();
    let direct = tokio::spawn(first_line(target));
    client.post(format!("http://{target_addr}/v1")).send().await.unwrap();
    assert_eq!(
        direct.await.unwrap(),
        "POST /v1 HTTP/1.1",
        "loopback never uses the proxy"
    );
    let proxied = tokio::spawn(first_line(proxy));
    client.post("http://upstream.invalid/v1").send().await.unwrap();
    assert_eq!(proxied.await.unwrap(), "POST http://upstream.invalid/v1 HTTP/1.1");
}

#[test]
fn canonical_header_matches_go() {
    assert_eq!(canonical_header("x-msh-device-id"), "X-Msh-Device-Id");
    assert_eq!(canonical_header("content-TYPE"), "Content-Type");
    assert_eq!(canonical_header("bad header"), "bad header");
}

fn go_fixture() -> serde_json::Value {
    serde_json::from_str(include_str!("../tests/fixtures/proxy_go.json")).unwrap()
}

/// What a local upstream saw of one request.
#[derive(Debug, Default, Clone, PartialEq)]
struct Saw {
    proto: String,
    client_alpn: Option<Vec<String>>,
    user_agent: String,
}

/// Answers one HTTP/1.1 request on `io` with `ok`, recording its User-Agent.
async fn serve_h1<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(mut io: S, saw: Arc<Mutex<Saw>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if io.read(&mut byte).await.unwrap_or(0) == 0 {
            return;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    {
        let mut s = saw.lock().unwrap();
        s.proto = head
            .lines()
            .next()
            .unwrap_or_default()
            .rsplit(' ')
            .next()
            .unwrap_or_default()
            .to_owned();
        s.user_agent = head
            .lines()
            .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("user-agent")))
            .map(|(_, v)| v.trim().to_owned())
            .unwrap_or_default();
    }
    let _ = io
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
        .await;
    let _ = io.shutdown().await;
}

/// A local upstream: TLS offering the `alpn` protocols (HTTP/2 when the client picks
/// it), or plain HTTP/1.1 for `None`. Returns its address, CA PEM and observations.
async fn upstream(alpn: Option<&'static [u8]>) -> (SocketAddr, Vec<u8>, Arc<Mutex<Saw>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let saw = Arc::new(Mutex::new(Saw::default()));
    let Some(alpn) = alpn else {
        let seen = saw.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                tokio::spawn(serve_h1(tcp, seen.clone()));
            }
        });
        return (addr, Vec::new(), saw);
    };
    let (ca, mut builder) = crate::test_tls::builder(&["upstream.test.invalid".to_owned()]);
    let offered = saw.clone();
    builder.set_alpn_select_callback(move |_, client| {
        // The client's offer, decoded from the wire's length-prefixed names.
        let (mut names, mut rest) = (Vec::new(), client);
        while let Some((&len, tail)) = rest.split_first() {
            let (name, tail) = tail.split_at(usize::from(len).min(tail.len()));
            names.push(String::from_utf8_lossy(name).into_owned());
            rest = tail;
        }
        offered.lock().unwrap().client_alpn = Some(names);
        btls::ssl::select_next_proto(alpn, client).ok_or(btls::ssl::AlpnError::NOACK)
    });
    let acceptor = builder.build();
    let seen = saw.clone();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let ssl = btls::ssl::Ssl::new(acceptor.context()).unwrap();
            let mut tls = tokio_btls::SslStream::new(ssl, tcp).unwrap();
            if std::pin::Pin::new(&mut tls).accept().await.is_err() {
                continue;
            }
            let seen = seen.clone();
            tokio::spawn(async move {
                if tls.ssl().selected_alpn_protocol() != Some(b"h2") {
                    return serve_h1(tls, seen).await;
                }
                let mut conn = http2::server::handshake(tls).await.unwrap();
                while let Some(Ok((request, mut respond))) = conn.accept().await {
                    {
                        let mut s = seen.lock().unwrap();
                        s.proto = "HTTP/2.0".into();
                        s.user_agent = request
                            .headers()
                            .get("user-agent")
                            .map(|v| v.to_str().unwrap().to_owned())
                            .unwrap_or_default();
                    }
                    let response = http::Response::builder().status(200).body(()).unwrap();
                    let mut body = respond.send_response(response, false).unwrap();
                    body.send_data(Bytes::from_static(b"ok"), true).unwrap();
                }
            });
        }
    });
    (addr, ca, saw)
}

/// An HTTP CONNECT proxy tunnelling every request to `target`.
async fn connect_proxy(target: SocketAddr) -> SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if client.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                if !head.starts_with(b"CONNECT ") {
                    return;
                }
                let mut upstream = tokio::net::TcpStream::connect(target).await.unwrap();
                client
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await
                    .unwrap();
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    addr
}

/// Go's `http.DefaultTransport` keeps idle connections per host; the standard clients
/// (API-key base URLs, OpenAI-compatible hosts) reuse theirs over HTTP/2 and HTTP/1.1.
#[tokio::test]
async fn go_clients_reuse_connections() {
    use std::sync::atomic::Ordering::SeqCst;
    let host = "upstream-reuse.test";
    for alpn in [&b"\x02h2\x08http/1.1"[..], b"\x08http/1.1"] {
        let (ca, addr, accepted) = crate::test_tls::counting_upstream(host, alpn).await;
        let clients = GoClients::new(Hooks {
            trust: Some(CertStore::from_pem_stack(ca).unwrap()),
            resolve: vec![(host.to_owned(), addr)],
        });
        let url = format!("https://{host}:{}/v1/messages", addr.port());
        for _ in 0..3 {
            let upstream = send(&clients.get(&Proxy::Direct), &url, GoHeaders::new(), "{}", None)
                .await
                .unwrap();
            let body: Vec<Bytes> = upstream.body.map(Result::unwrap).collect().await;
            assert_eq!(body.concat(), b"ok");
        }
        assert_eq!(accepted.load(SeqCst), 1, "ALPN {alpn:?}");
    }
}

/// Go's cloned default transport (proxyutil, ForceAttemptHTTP2) offers h2 and
/// http/1.1 to every TLS upstream and speaks HTTP/2 wherever the upstream picks it,
/// directly or through a CONNECT proxy; plain HTTP stays HTTP/1.1. Go's default
/// User-Agent names the protocol (tests/reference/proxy/main.go `protocols`).
#[tokio::test]
async fn go_clients_negotiate_http2_like_go() {
    let fixture = go_fixture();
    let cases = fixture["protocols"].as_array().unwrap();
    assert_eq!(cases.len(), 5);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let alpn: Option<&'static [u8]> = match case["server"].as_str().unwrap() {
            "h2" => Some(b"\x02h2\x08http/1.1"),
            "h1" => Some(b"\x08http/1.1"),
            _ => None,
        };
        let (addr, ca, saw) = upstream(alpn).await;
        let host = "upstream.test.invalid";
        let proxy = match case["proxy"].as_str().unwrap() {
            "http" => Proxy::Url(format!("http://{}", connect_proxy(addr).await)),
            _ => Proxy::Direct,
        };
        let clients = GoClients::new(Hooks {
            trust: (!ca.is_empty()).then(|| CertStore::from_pem_stack(ca).unwrap()),
            resolve: vec![(host.to_owned(), addr)],
        });
        let scheme = if alpn.is_some() { "https" } else { "http" };
        let route = |_: &url::Url| {
            Ok(Route {
                client: clients.get(&proxy),
                order: None,
            })
        };
        let want = Saw {
            proto: case["proto"].as_str().unwrap().to_owned(),
            client_alpn: case["client_alpn"]
                .as_array()
                .map(|a| a.iter().map(|p| p.as_str().unwrap().to_owned()).collect()),
            user_agent: case["user_agent"].as_str().unwrap().to_owned(),
        };
        // The second request knows the origin's protocol; the first assumes HTTP/1.1
        // for the default User-Agent only (see `expects_http2`).
        for attempt in ["first", "second"] {
            let upstream = send_request(
                &route,
                wreq::Method::GET,
                &format!("{scheme}://{host}:{}/", addr.port()),
                GoHeaders::new(),
                None,
                Some(std::time::Duration::from_secs(10)),
            )
            .await
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
            assert_eq!(upstream.status, 200, "{name}");
            assert_eq!(read_all(upstream.body, 16, false).await.unwrap(), "ok", "{name}");
            let mut want = want.clone();
            if attempt == "first" && want.proto == "HTTP/2.0" {
                want.user_agent = "Go-http-client/1.1".into();
            }
            assert_eq!(*saw.lock().unwrap(), want, "{name} ({attempt} request)");
        }
    }
}

/// Every case runs the real `bufio.Scanner` in tests/reference/proxy/main.go.
#[tokio::test]
async fn lines_follow_go_bufio_scanner() {
    for case in go_fixture()["lines"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let mut items: Vec<Result<Bytes, ExecError>> = case["chunks"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| Ok(Bytes::from(c.as_str().unwrap().to_owned())))
            .collect();
        let failed = case["io_error"].as_bool().unwrap();
        if failed {
            items.push(Err(ExecError::local(502, FailureScope::Transport, "read failed")));
        }
        let body = futures_util::stream::iter(items);
        // A failed reader is never read again; otherwise the body ends (EOF).
        let body = if failed {
            body.chain(futures_util::stream::pending()).boxed()
        } else {
            body.boxed()
        };
        let max = case["max"].as_u64().unwrap() as usize;
        let got: Vec<_> = lines(body, max).collect().await;
        // Go with a separate terminal read differs only where the ponytail says.
        let separate = (case["separate_tokens"].clone(), case["separate_error"].clone());
        let attached = (case["tokens"].clone(), case["error"].clone());
        let differs = ["exact-max-unterminated", "exact-max-then-io-error"].contains(&name);
        assert_eq!(separate != attached, differs, "{name}: Go's two terminal modes");
        let (tokens, errors): (Vec<_>, Vec<_>) = got.into_iter().partition(Result::is_ok);
        let tokens: Vec<String> = tokens
            .into_iter()
            .map(|t| String::from_utf8(t.unwrap().to_vec()).unwrap())
            .collect();
        let want: Vec<&str> = case["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap())
            .collect();
        assert_eq!(tokens, want, "{name}");
        let error = errors.into_iter().map(|e| e.unwrap_err()).collect::<Vec<_>>();
        match case["error"].as_str().unwrap() {
            "" => assert!(error.is_empty(), "{name}"),
            "too_long" => {
                assert_eq!(error.len(), 1, "{name}");
                assert_eq!(error[0].body, "bufio.Scanner: token too long", "{name}");
            }
            _ => {
                assert_eq!(error.len(), 1, "{name}");
                assert_eq!(error[0].body, "read failed", "{name}");
            }
        }
    }
}

/// Go's own table (net/url url_test.go `resolvePathTests`), plus merged `//` paths.
#[test]
fn resolve_path_matches_go() {
    for (base, reference, want) in [
        ("a/b", ".", "/a/"),
        ("a/b", "c", "/a/c"),
        ("a/b", "..", "/"),
        ("a/", "..", "/"),
        ("a/", "../..", "/"),
        ("a/b/c", "..", "/a/"),
        ("a/b/c", "../d", "/a/d"),
        ("a/b/c", ".././d", "/a/d"),
        ("a/b", "./..", "/"),
        ("a/./b", ".", "/a/"),
        ("a/../", ".", "/"),
        ("a/.././b", "c", "/c"),
        ("//api/item", "next", "//api/next"),
        ("", "next", "/next"),
        ("", "", ""),
    ] {
        assert_eq!(go_resolve_path(base, reference), want, "{base:?} + {reference:?}");
    }
}

/// Go's `http.Client` redirect hops for GET and POST (tests/reference/proxy/main.go),
/// replayed through [`request`] against the same scripted servers.
#[tokio::test]
async fn redirects_follow_go_http_client() {
    type Hops = Arc<Mutex<Vec<serde_json::Value>>>;
    async fn record(
        hops: Hops,
        script: Arc<serde_json::Value>,
        other: Arc<String>,
        request: axum::extract::Request,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        let (parts, body) = request.into_parts();
        let body = axum::body::to_bytes(body, 1 << 20).await.unwrap();
        let header = |name: &str| {
            parts
                .headers
                .get_all(name)
                .iter()
                .map(|v| v.to_str().unwrap().to_owned())
                .collect::<Vec<_>>()
                .join(",")
        };
        let path = parts.uri.path().to_owned();
        let target = parts.uri.path_and_query().map_or("/", |p| p.as_str()).to_owned();
        hops.lock().unwrap().push(serde_json::json!({
            "user_agent": header("user-agent"),
            "has_user_agent": parts.headers.contains_key("user-agent"),
            "accept_encoding": header("accept-encoding"),
            "method": parts.method.as_str(),
            "path": target,
            "host": header("host"),
            "referer": header("referer"),
            "authorization": header("authorization"),
            "content_type": header("content-type"),
            "content_length": header("content-length"),
            "body": String::from_utf8_lossy(&body),
        }));
        let Some(step) = script.get(&path) else {
            return (http::StatusCode::OK, "done").into_response();
        };
        let status = http::StatusCode::from_u16(step["status"].as_u64().unwrap() as u16).unwrap();
        let location = step["location"].as_str().unwrap().replacen("OTHER", &other, 1);
        if location.is_empty() {
            return status.into_response();
        }
        (status, [(http::header::LOCATION, location)]).into_response()
    }
    let fixture = go_fixture();
    let cases = fixture["redirects"].as_array().unwrap();
    // Go always sends the URL as written; Rust does with exact_target, and normalizes
    // otherwise, which only dot segments and escaping can tell apart.
    let runs = cases.iter().flat_map(|case| {
        let start = case["start"].as_str().unwrap();
        let both = start == "/start";
        [(case, true)].into_iter().chain(both.then_some((case, false)))
    });
    for (case, exact) in runs {
        let name = format!("{} (exact: {exact})", case["name"].as_str().unwrap());
        let main = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let second = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (main_addr, other_port) = (main.local_addr().unwrap(), second.local_addr().unwrap().port());
        let other = Arc::new(format!("http://localhost:{other_port}"));
        let hops: Hops = Arc::default();
        let script = Arc::new(case["script"].clone());
        for listener in [main, second] {
            let (hops, script, other) = (hops.clone(), script.clone(), other.clone());
            let app = axum::Router::new().fallback(move |request: axum::extract::Request| {
                record(hops.clone(), script.clone(), other.clone(), request)
            });
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        }
        let mut headers = GoHeaders::new();
        for (k, v) in case["headers"].as_object().unwrap() {
            headers.set(k, v.as_str().unwrap());
        }
        if exact {
            headers.exact_target();
        }
        if case["disable_compression"].as_bool().unwrap() {
            headers.disable_compression();
        }
        let base = format!("http://{main_addr}");
        let method = wreq::Method::from_bytes(case["method"].as_str().unwrap().as_bytes()).unwrap();
        let body = case["body"].as_str().map(|b| Bytes::from(b.to_owned()));
        let result = request(
            &default_client(),
            method,
            &format!("{base}{}", case["start"].as_str().unwrap()),
            headers,
            body,
            Some(std::time::Duration::from_secs(10)),
        )
        .await;
        match &result {
            Ok(upstream) => assert_eq!(u64::from(upstream.status), case["status"].as_u64().unwrap(), "{name}"),
            Err(_) => assert!(case["error"].as_bool().unwrap(), "{name}: unexpected error"),
        }
        let got: Vec<serde_json::Value> = hops
            .lock()
            .unwrap()
            .iter()
            .map(|h| {
                let mut h = h.clone();
                let host = h["host"]
                    .as_str()
                    .unwrap()
                    .replace(&main_addr.to_string(), "BASE")
                    .replace(&format!("localhost:{other_port}"), "OTHER");
                h["host"] = host.into();
                h["referer"] = h["referer"].as_str().unwrap().replacen(&base, "BASE", 1).into();
                h
            })
            .collect();
        assert_eq!(&got, case["hops"].as_array().unwrap(), "{name}");
    }
}

/// Go's `Client.Timeout` bounds the whole exchange: redirect hops share one deadline
/// instead of each getting the full timeout.
#[tokio::test]
async fn timeout_bounds_all_redirect_hops_together() {
    use axum::response::IntoResponse;
    const TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1000);
    const HOP: std::time::Duration = std::time::Duration::from_millis(600);
    async fn get(target: String) -> Result<RawUpstream, SendError> {
        let client = default_client();
        let route = |_: &url::Url| {
            Ok(Route {
                client: client.clone(),
                order: None,
            })
        };
        send_request_raw(
            &route,
            wreq::Method::GET,
            &target,
            GoHeaders::new(),
            None,
            Some(TIMEOUT),
        )
        .await
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().fallback(|uri: http::Uri| async move {
        tokio::time::sleep(HOP).await;
        match uri.path() {
            "/1" => (http::StatusCode::FOUND, [(http::header::LOCATION, "/2")]).into_response(),
            "/2" => (http::StatusCode::FOUND, [(http::header::LOCATION, "/3")]).into_response(),
            _ => "done".into_response(),
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // One slow hop within the timeout still succeeds, body included.
    let upstream = get(format!("http://{addr}/3")).await.unwrap();
    assert_eq!(upstream.status, 200);
    let body: Vec<Bytes> = upstream.body.map(Result::unwrap).collect().await;
    assert_eq!(body.concat(), b"done");
    // Each hop is under the timeout, but the second ends past the shared deadline.
    match get(format!("http://{addr}/1")).await {
        Err(SendError::Local { error, url }) => {
            assert_eq!(error.scope, FailureScope::Request);
            assert_eq!(url, format!("http://{addr}/2"));
        }
        Err(other) => panic!("{other:?}"),
        Ok(upstream) => panic!("followed both redirects: {}", upstream.status),
    }
}

/// Once the deadline has passed, Go sends nothing: a 307 that would resend a POST
/// body to its target ends without a request, with the error a transport timeout
/// gives callers.
#[tokio::test]
async fn no_hop_is_sent_after_the_deadline() {
    use axum::response::IntoResponse;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    const TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let app = axum::Router::new().fallback(move |uri: http::Uri| {
        let seen = seen.clone();
        async move {
            if uri.path() == "/start" {
                return (
                    http::StatusCode::TEMPORARY_REDIRECT,
                    [(http::header::LOCATION, "/target")],
                )
                    .into_response();
            }
            seen.fetch_add(1, SeqCst);
            "done".into_response()
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = default_client();
    // The redirect handling uses up the budget: choosing the second hop's client
    // takes longer than the whole timeout.
    let route = |next: &url::Url| {
        if next.path() == "/target" {
            std::thread::sleep(TIMEOUT);
        }
        Ok(Route {
            client: client.clone(),
            order: None,
        })
    };
    let start = format!("http://{addr}/start");
    let post = || Some(Bytes::from_static(b"{}"));
    let sent = send_request_raw(
        &route,
        wreq::Method::POST,
        &start,
        GoHeaders::new(),
        post(),
        Some(TIMEOUT),
    )
    .await;
    match sent {
        Err(SendError::Timeout { url }) => assert_eq!(url, format!("http://{addr}/target")),
        Err(other) => panic!("{other:?}"),
        Ok(upstream) => panic!("followed the redirect: {}", upstream.status),
    }
    // The same error as a transport timeout through `send_request`.
    let error = send_request(
        &route,
        wreq::Method::POST,
        &start,
        GoHeaders::new(),
        post(),
        Some(TIMEOUT),
    )
    .await
    .err()
    .unwrap();
    // Both an invalid request and an elapsed exchange deadline are terminal.
    let any = default_client().get("not a url").send().await.unwrap_err();
    let transport = crate::upstream::transport_error(any);
    assert_eq!(
        (error.status, error.scope, error.body),
        (transport.status, transport.scope, transport.body)
    );
    assert_eq!(hits.load(SeqCst), 0, "the redirect target got a request");
}

/// The stages a timed exchange reported, in order.
#[derive(Default)]
struct Stages(Mutex<Vec<UpstreamStage>>);

impl cpa_core::exec::UsageObserver for Stages {
    fn response_body(&self, _: cpa_core::format::Format, _: &[u8]) {}
    fn response_line(&self, _: cpa_core::format::Format, _: &[u8]) {}
    fn request(&self, _: cpa_core::format::Format, _: &[u8]) {}
    fn upstream_stage(&self, stage: UpstreamStage) {
        self.0.lock().unwrap().push(stage);
    }
}

/// A timed request goes out byte for byte as an untimed one (the same head with its
/// `Content-Length`, the same body), and reports its stages in order, its sizes, and
/// whether a connection was dialled for it.
#[tokio::test]
async fn timed_requests_send_the_same_bytes_and_report_their_stages() {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
    let wire = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let seen = wire.clone();
    tokio::spawn(async move {
        while let Ok((conn, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut conn = tokio::io::BufReader::new(conn);
                loop {
                    let (mut raw, mut length) = (Vec::new(), 0);
                    loop {
                        let mut line = String::new();
                        if conn.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        raw.extend_from_slice(line.as_bytes());
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            length = v.trim().parse().unwrap();
                        }
                        if line == "\r\n" {
                            break;
                        }
                    }
                    let mut body = vec![0; length];
                    conn.read_exact(&mut body).await.unwrap();
                    raw.extend_from_slice(&body);
                    seen.lock().unwrap().push(raw);
                    let reply = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
                    conn.get_mut().write_all(reply).await.unwrap();
                }
            });
        }
    });
    // Several pieces, and a tail that is not a whole piece.
    let body = Bytes::from(vec![b'a'; 2 * BODY_PIECE + 123]);
    let clients = GoClients::new(Hooks::default()).timed();
    let route = |_: &url::Url| {
        Ok(Route {
            client: clients.get(&Proxy::Direct),
            order: None,
        })
    };
    let headers = || {
        let mut h = GoHeaders::new();
        h.set("Content-Type", "application/json");
        h
    };
    let stages = Arc::new(Stages::default());
    let sink = UsageSink::new(stages.clone());
    let read = |up: Upstream| async move {
        let chunks: Vec<_> = up.body.collect().await;
        chunks.into_iter().map(Result::unwrap).collect::<Vec<_>>().concat()
    };
    let plain = send_routed(&route, &url, headers(), body.clone(), None, None)
        .await
        .unwrap();
    assert_eq!(read(plain).await, b"hello");
    assert!(
        stages.0.lock().unwrap().is_empty(),
        "an untimed request reports nothing"
    );
    // A new client, so the first timed request dials.
    let clients = GoClients::new(Hooks::default()).timed();
    let route = |_: &url::Url| {
        Ok(Route {
            client: clients.get(&Proxy::Direct),
            order: None,
        })
    };
    for dialled in [true, false] {
        let timed = send_routed(&route, &url, headers(), body.clone(), None, Some(&sink))
            .await
            .unwrap();
        assert_eq!(read(timed).await, b"hello");
        let reported = std::mem::take(&mut *stages.0.lock().unwrap());
        assert_eq!(
            reported,
            [
                UpstreamStage::Started,
                UpstreamStage::Hop { hop: 0, body: true },
                UpstreamStage::Connected { hop: 0 },
                UpstreamStage::BodySent {
                    hop: 0,
                    bytes: body.len() as u64
                },
                UpstreamStage::Headers { dialled },
                UpstreamStage::FirstByte,
                UpstreamStage::BodyRead { bytes: 5 },
            ]
        );
    }
    let wire = wire.lock().unwrap();
    assert_eq!(wire.len(), 3);
    assert!(String::from_utf8_lossy(&wire[0]).contains(&format!("Content-Length: {}", body.len())));
    assert_eq!(wire[0], wire[1], "timed and untimed requests differ on the wire");
    assert_eq!(wire[0], wire[2]);
}

#[test]
fn client_cache_is_bounded_lru() {
    let clients = GoClients::new(Hooks::default());
    for i in 0..=CACHE_CAPACITY {
        clients.get(&Proxy::Url(format!("http://p{i}:1")));
    }
    assert_eq!(clients.cache.lock().unwrap().len(), CACHE_CAPACITY);
    assert!(
        clients
            .cache
            .lock()
            .unwrap()
            .iter()
            .all(|(p, _)| *p != Proxy::Url("http://p0:1".into()))
    );
    clients.get(&Proxy::Url("http://p1:1".into()));
    assert_eq!(clients.cache.lock().unwrap()[0].0, Proxy::Url("http://p1:1".into()));
}
