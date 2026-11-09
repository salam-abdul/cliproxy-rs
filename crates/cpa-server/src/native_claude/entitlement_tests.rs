use super::*;
use axum::body::Bytes;
use btls::{
    asn1::Asn1Time,
    bn::BigNum,
    ec::{EcGroup, EcKey},
    hash::MessageDigest,
    nid::Nid,
    pkey::PKey,
    x509::{X509, X509NameBuilder},
};

const REQUEST: &[u8] =
    br#"{ "model" : "claude-opus-test", "metadata":{"session_id":"synthetic-session"}, "messages": [] }"#;
const SUCCESS: &[u8] = br#"{ "content":[], "usage":{"input_tokens":1} }"#;

#[derive(Clone)]
struct Reply {
    status: u16,
    body: Vec<u8>,
    broken: bool,
}
type Seen = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

struct Fixture {
    state: Arc<State>,
    url: String,
    seen: Seen,
    tasks: Vec<JoinHandle<()>>,
    dir: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
fn tls() -> Arc<SslAcceptor> {
    let key =
        PKey::from_ec_key(EcKey::generate(&EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap()).unwrap()).unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_nid(Nid::COMMONNAME, "localhost").unwrap();
    let name = name.build();
    let mut cert = X509::builder().unwrap();
    cert.set_version(2).unwrap();
    cert.set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    cert.set_subject_name(&name).unwrap();
    cert.set_issuer_name(&name).unwrap();
    cert.set_pubkey(&key).unwrap();
    cert.set_not_before(&Asn1Time::days_from_now(0).unwrap()).unwrap();
    cert.set_not_after(&Asn1Time::days_from_now(1).unwrap()).unwrap();
    cert.sign(&key, MessageDigest::sha256()).unwrap();
    let mut tls = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
    tls.set_private_key(&key).unwrap();
    tls.set_certificate(&cert.build()).unwrap();
    Arc::new(tls.build())
}
impl Fixture {
    async fn new(reply: Reply) -> Self {
        Self::build(reply, None).await
    }
    async fn with_history(reply: Reply, history: &Path) -> Self {
        Self::build(reply, Some(history)).await
    }
    async fn build(reply: Reply, history: Option<&Path>) -> Self {
        let dir = std::env::temp_dir().join(format!("native-entitlement-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let tls = tls();
        let acceptor = tls.clone();
        let captured = seen.clone();
        let upstream = tokio::spawn(async move {
            while let Ok((io, _)) = listener.accept().await {
                let tls = acceptor.clone();
                let seen = captured.clone();
                let reply = reply.clone();
                tokio::spawn(async move {
                    let mut io = tokio_btls::SslStream::new(Ssl::new(tls.context()).unwrap(), io).unwrap();
                    std::pin::Pin::new(&mut io).accept().await.unwrap();
                    let service = service_fn(move |req: Request<Incoming>| {
                        let seen = seen.clone();
                        let reply = reply.clone();
                        async move {
                            let token = req.headers()["authorization"].to_str().unwrap().to_owned();
                            let body = axum::body::to_bytes(Body::new(req.into_body()), 65536).await.unwrap();
                            seen.lock().unwrap().push((token.clone(), body.to_vec()));
                            if cpa_common::gostr::contains(&body, b"MODE_STALL") {
                                tokio::time::sleep(Duration::from_secs(5)).await;
                            }
                            let response = if token == "Bearer token-a"
                                && cpa_common::gostr::contains(&body, b"claude-opus-test")
                            {
                                let zstd = reply.body.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]);
                                let body = if reply.broken {
                                    let stream = futures_util::stream::once(async move {
                                        Ok::<_, std::io::Error>(Bytes::from(reply.body))
                                    })
                                    .chain(futures_util::stream::once(async {
                                        tokio::time::sleep(Duration::from_millis(50)).await;
                                        Err(std::io::Error::other("synthetic disconnect"))
                                    }));
                                    Body::from_stream(stream)
                                } else {
                                    Body::from(reply.body)
                                };
                                let mut response = Response::builder()
                                    .status(reply.status)
                                    .header("content-type", "application/json")
                                    .header("x-request-id", "synthetic-id");
                                if zstd {
                                    response = response.header("content-encoding", "zstd");
                                }
                                response.body(body).unwrap()
                            } else {
                                Response::builder()
                                    .header("content-type", "application/json")
                                    .body(Body::from(SUCCESS))
                                    .unwrap()
                            };
                            Ok::<_, std::convert::Infallible>(response)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(io), service)
                        .await;
                });
            }
        });
        let quota = json!({"five_hour":{"utilization":1},"seven_day":{"utilization":1}});
        let native: Config = serde_json::from_value(json!({
            "port":0,"httpProxyPort":0,"database":dir.join("routing.json"),"events":dir.join("events.jsonl"),
            "proxySecret":dir.join("secret"),"key":dir.join("key"),"cert":dir.join("cert"),"authDir":dir,
            "fixtureAccounts":[{"id":"a","token":"token-a","quota":quota},{"id":"b","token":"token-b","quota":quota}],
            "upstream":{"hostname":"127.0.0.1","port":port}
        }))
        .unwrap();
        let mut rt_yaml = format!("auth-dir: {}\n", dir.display());
        if let Some(history) = history {
            rt_yaml.push_str(&format!(
                "observability: {{history: {{enable: true, dir: '{}'}}}}\n",
                history.display()
            ));
        }
        let rt_config = cpa_core::config::Config::parse(&rt_yaml).unwrap();
        let rt = Arc::new(crate::testing::runtime(
            rt_config,
            vec![],
            cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new(format!("https://127.0.0.1:{port}")).unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        let state = Arc::new(State {
            events: diagnostics::Writer::start(native.events.clone(), rt.native_events.lost.clone()).unwrap(),
            history: Some(
                history::History::start(
                    native.events.with_extension("sqlite"),
                    Arc::new(std::sync::atomic::AtomicU64::new(0)),
                )
                .unwrap(),
            ),
            config: native,
            rt,
            client: wreq::Client::builder()
                .no_proxy()
                .tls_cert_verification(false)
                .retry(wreq::retry::Policy::never())
                .redirect(wreq::redirect::Policy::none())
                .no_gzip()
                .no_brotli()
                .no_deflate()
                .no_zstd()
                .build()
                .unwrap(),
            tls: std::sync::RwLock::new(tls),
            secret: "synthetic-secret".into(),
            routing: Default::default(),
            sessions: Default::default(),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router_state = state.clone();
        let router = tokio::spawn(async move {
            while let Ok((io, _)) = listener.accept().await {
                tokio::spawn(connection(io, router_state.clone(), false));
            }
        });
        Self {
            state,
            url,
            seen,
            tasks: vec![upstream, router],
            dir,
        }
    }
    async fn request(&self) -> wreq::Response {
        self.request_body(REQUEST).await
    }
    async fn request_body(&self, body: &'static [u8]) -> wreq::Response {
        wreq::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .retry(wreq::retry::Policy::never())
            .build()
            .unwrap()
            .post(format!("{}/v1/messages", self.url))
            .body(body)
            .send()
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn native_entitlement_failover_cools_only_rejected_account_model_and_preserves_request_bytes() {
    let fixture = Fixture::new(Reply {
        status: 403,
        body: br#"{"error":{"code":"model_access_denied"}}"#.to_vec(),
        broken: false,
    })
    .await;
    let response = fixture.request().await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap(), SUCCESS);
    {
        let seen = fixture.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], ("Bearer token-a".into(), REQUEST.to_vec()));
        assert_eq!(seen[1], ("Bearer token-b".into(), REQUEST.to_vec()));
        let routing = fixture.state.routing.lock().unwrap();
        assert!(routing.cooldowns.model_until("a", "claude-opus-test") > now());
        assert_eq!(routing.cooldowns.until("a", "claude-sonnet-test"), 0);
        assert_eq!(routing.cooldowns.account_until("a"), 0);
        assert_eq!(routing.cooldowns.until("b", "claude-opus-test"), 0);
    }
    let sonnet = br#"{"model":"claude-sonnet-test","metadata":{"session_id":"synthetic-sonnet"},"messages":[]}"#;
    assert_eq!(fixture.request_body(sonnet).await.bytes().await.unwrap(), SUCCESS);
    assert_eq!(fixture.seen.lock().unwrap()[2].0, "Bearer token-a");
}

#[tokio::test]
async fn native_auth_permission_and_quota_rejections_keep_account_failover() {
    for (status, kind) in [
        (401, "authentication_error"),
        (403, "permission_error"),
        (429, "rate_limit_error"),
    ] {
        let fixture = Fixture::new(Reply {
            status,
            body: serde_json::to_vec(&json!({"error":{"type":kind,"message":"Forbidden"}})).unwrap(),
            broken: false,
        })
        .await;
        assert_eq!(fixture.request().await.bytes().await.unwrap(), SUCCESS);
        assert_eq!(fixture.seen.lock().unwrap().len(), 2);
        let routing = fixture.state.routing.lock().unwrap();
        assert!(routing.cooldowns.account_until("a") > now());
        assert_eq!(routing.cooldowns.model_until("a", "claude-opus-test"), 0);
    }
}

#[tokio::test]
async fn native_unrecognized_errors_forward_exact_bytes_without_replay() {
    for body in [
        br#"{ "error" : { "message" : "Forbidden" } }"#.to_vec(),
        br#"{"error":{"code":"model_access_denied"}"#.to_vec(),
        vec![b'x'; 70000],
    ] {
        let fixture = Fixture::new(Reply {
            status: 404,
            body: body.clone(),
            broken: false,
        })
        .await;
        let response = fixture.request().await;
        assert_eq!(response.status(), 404);
        assert_eq!(response.headers()["x-request-id"], "synthetic-id");
        assert_eq!(response.bytes().await.unwrap(), body);
        assert_eq!(fixture.seen.lock().unwrap().len(), 1);
        assert_eq!(
            fixture
                .state
                .routing
                .lock()
                .unwrap()
                .cooldowns
                .until("a", "claude-opus-test"),
            0
        );
    }
}

#[tokio::test]
async fn native_accepted_broken_stream_is_never_replayed() {
    let fixture = Fixture::new(Reply {
        status: 200,
        body: SUCCESS.to_vec(),
        broken: true,
    })
    .await;
    let response = fixture.request().await;
    assert_eq!(response.status(), 200);
    let mut stream = response.bytes_stream();
    let mut received = Vec::new();
    let mut failed = false;
    while let Some(item) = stream.next().await {
        match item {
            Ok(bytes) => received.extend_from_slice(&bytes),
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    assert!(failed);
    assert_eq!(received, SUCCESS);
    assert_eq!(fixture.seen.lock().unwrap().len(), 1);
}

/// A panic while the routing and session locks were held does not take the router
/// down: later requests still route and status still answers.
#[tokio::test]
async fn native_routing_survives_a_panic_under_its_locks() {
    let fixture = Fixture::new(Reply {
        status: 200,
        body: SUCCESS.to_vec(),
        broken: false,
    })
    .await;
    let state = fixture.state.clone();
    std::thread::spawn(move || {
        let _routing = state.routing.lock().unwrap();
        let _sessions = state.sessions.lock().unwrap();
        panic!("synthetic panic under the routing locks");
    })
    .join()
    .unwrap_err();
    assert!(fixture.state.routing.is_poisoned() && fixture.state.sessions.is_poisoned());
    let response = fixture.request().await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap(), SUCCESS);
    assert_eq!(
        fixture.state.status().await.unwrap()["accounts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn a_response_in_flight_at_runtime_shutdown_is_recorded_once_as_cancelled() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let fixture = runtime.block_on(async {
        let fixture = Fixture::new(Reply {
            status: 200,
            body: SUCCESS.to_vec(),
            broken: false,
        })
        .await;
        let account = fixture.state.accounts().await.unwrap().remove(0);
        let facts = Exchange {
            request_id: "in-flight".into(),
            sse: true,
            timing: json!({}),
            headers_at: std::time::Instant::now(),
            attempts: 1,
            a: account,
            model: "claude-opus-test".into(),
            sid: "synthetic-session".into(),
            status: 200,
            digest: String::new(),
            body_bytes: 0,
            started: std::time::Instant::now(),
        };
        let observation = Observation::new(fixture.state.clone(), facts, "identity", None);
        tokio::spawn(async move {
            let _streaming = observation;
            std::future::pending::<()>().await;
        });
        fixture
    });
    drop(runtime);
    let events = fixture.state.rt.native_events.snapshot()["events"].clone();
    let recorded: Vec<_> = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["requestId"] == "in-flight")
        .collect();
    assert_eq!(recorded.len(), 1, "{events}");
    assert_eq!(recorded[0]["outcome"], "client_cancelled");
    assert_eq!(recorded[0]["status"], 499);
}

fn history_rows(dir: &Path) -> Vec<crate::request_logging::history::row::Row> {
    let mut rows = Vec::new();
    let Ok(partitions) = std::fs::read_dir(dir) else {
        return rows;
    };
    for partition in partitions.flatten() {
        if partition.path().is_dir() && partition.file_name() != ".deletes" {
            for file in std::fs::read_dir(partition.path()).unwrap().flatten() {
                if file.path().extension().is_some_and(|e| e == "parquet") {
                    rows.extend(crate::request_logging::history::file::read_rows(&file.path()).unwrap());
                }
            }
        }
    }
    rows.sort_by_key(|r| r.send);
    rows
}

/// Flushes the history writer until `count` rows are on disk.
async fn flushed_rows(fixture: &Fixture, dir: &Path, count: usize) -> Vec<crate::request_logging::history::row::Row> {
    for _ in 0..200 {
        let rt = fixture.state.rt.clone();
        tokio::task::spawn_blocking(move || rt.history().flush()).await.unwrap();
        let rows = history_rows(dir);
        if rows.len() >= count {
            assert_eq!(rows.len(), count, "{rows:#?}");
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected {count} history row(s), found {:#?}", history_rows(dir));
}

#[tokio::test]
async fn a_native_exchange_is_recorded_decoded() {
    use crate::request_logging::history::row::{Outcome, Source};
    let dir = crate::request_logging::history::file::tests::TempDir::new();
    let plain = br#"{"content":[],"usage":{"input_tokens":4,"output_tokens":2,"cache_read_input_tokens":3}}"#;
    let fixture = Fixture::with_history(
        Reply {
            status: 200,
            body: zstd::bulk::compress(plain, 3).unwrap(),
            broken: false,
        },
        &dir.0,
    )
    .await;
    let response = fixture.request().await;
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    let rows = flushed_rows(&fixture, &dir.0, 1).await;
    let row = &rows[0];
    assert_eq!(row.source, Source::Native);
    assert_eq!(row.request_body.as_deref().map(str::as_bytes), Some(REQUEST));
    assert_eq!(row.response_body.as_deref().map(str::as_bytes), Some(&plain[..]));
    assert!(!row.response_truncated);
    assert_eq!(row.outcome, Outcome::Complete);
    assert_eq!(row.status, Some(200));
    assert_eq!(row.endpoint.as_deref(), Some("POST /v1/messages"));
    assert_eq!(row.model.as_deref(), Some("claude-opus-test"));
    // The client's own session ID, so `history delete --session` finds native rows.
    assert_eq!(row.session_id.as_deref(), Some("synthetic-session"));
    assert_eq!(row.account.as_deref(), Some("a"));
    assert_eq!(row.upstream_request_id.as_deref(), Some("synthetic-id"));
    assert_eq!(row.input_tokens, Some(4));
    assert_eq!(row.output_tokens, Some(2));
    assert_eq!(row.cache_read_tokens, Some(3));
}

#[tokio::test]
async fn a_native_failover_records_both_sends() {
    use crate::request_logging::history::row::Outcome;
    let dir = crate::request_logging::history::file::tests::TempDir::new();
    let fixture = Fixture::with_history(
        Reply {
            status: 403,
            body: br#"{"error":{"code":"model_access_denied"}}"#.to_vec(),
            broken: false,
        },
        &dir.0,
    )
    .await;
    let response = fixture.request().await;
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    let rows = flushed_rows(&fixture, &dir.0, 2).await;
    assert_eq!(rows[0].exchange_id, rows[1].exchange_id);
    assert_eq!((rows[0].send, rows[1].send), (0, 1));
    assert_eq!((rows[0].status, rows[0].outcome), (Some(403), Outcome::Failover));
    assert_eq!(rows[0].account.as_deref(), Some("a"));
    assert_eq!(rows[0].request_body.as_deref().map(str::as_bytes), Some(REQUEST));
    assert_eq!((rows[1].status, rows[1].outcome), (Some(200), Outcome::Complete));
    assert_eq!(rows[1].account.as_deref(), Some("b"));
    assert_eq!(rows[1].response_body.as_deref().map(str::as_bytes), Some(SUCCESS));
}

#[tokio::test]
async fn a_broken_native_stream_is_a_stream_error_row() {
    use crate::request_logging::history::row::Outcome;
    let dir = crate::request_logging::history::file::tests::TempDir::new();
    let fixture = Fixture::with_history(
        Reply {
            status: 200,
            body: br#"{"content":["#.to_vec(),
            broken: true,
        },
        &dir.0,
    )
    .await;
    let response = fixture.request().await;
    let _ = response.bytes().await;
    let rows = flushed_rows(&fixture, &dir.0, 1).await;
    assert_eq!(rows[0].outcome, Outcome::StreamError);
    assert_eq!(rows[0].response_body.as_deref(), Some(r#"{"content":["#));
}

#[tokio::test]
async fn a_native_error_response_is_a_failed_row() {
    use crate::request_logging::history::row::Outcome;
    let dir = crate::request_logging::history::file::tests::TempDir::new();
    let fixture = Fixture::with_history(
        Reply {
            status: 400,
            body: br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#.to_vec(),
            broken: false,
        },
        &dir.0,
    )
    .await;
    let response = fixture.request().await;
    assert_eq!(response.status(), 400);
    response.bytes().await.unwrap();
    let rows = flushed_rows(&fixture, &dir.0, 1).await;
    assert_eq!((rows[0].status, rows[0].outcome), (Some(400), Outcome::Failed));
}

#[tokio::test]
async fn a_client_that_leaves_before_the_headers_still_leaves_a_row() {
    use crate::request_logging::history::row::Outcome;
    let dir = crate::request_logging::history::file::tests::TempDir::new();
    let fixture = Fixture::with_history(
        Reply {
            status: 200,
            body: SUCCESS.to_vec(),
            broken: false,
        },
        &dir.0,
    )
    .await;
    let stalled = br#"{"model":"claude-opus-test","metadata":{"session_id":"synthetic-session"},"messages":[{"role":"user","content":"MODE_STALL"}]}"#;
    let request = wreq::Client::builder()
        .no_proxy()
        .retry(wreq::retry::Policy::never())
        .build()
        .unwrap()
        .post(format!("{}/v1/messages", fixture.url))
        .body(&stalled[..])
        .send();
    assert!(tokio::time::timeout(Duration::from_millis(500), request).await.is_err());
    let rows = flushed_rows(&fixture, &dir.0, 1).await;
    assert_eq!(rows[0].outcome, Outcome::Cancelled);
    assert_eq!(rows[0].status, None);
    assert!(
        rows[0]
            .request_body
            .as_deref()
            .is_some_and(|b| b.contains("MODE_STALL"))
    );
}
