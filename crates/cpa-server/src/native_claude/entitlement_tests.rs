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
use cpa_core::credential::Credential;

pub(super) const REQUEST: &[u8] =
    br#"{ "model" : "claude-opus-test", "metadata":{"session_id":"synthetic-session"}, "messages": [] }"#;
pub(super) const SUCCESS: &[u8] = br#"{ "content":[], "usage":{"input_tokens":1} }"#;

#[derive(Clone)]
pub(super) struct Reply {
    pub(super) status: u16,
    pub(super) body: Vec<u8>,
    pub(super) broken: bool,
}
type Seen = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

pub(super) struct Fixture {
    pub(super) state: Arc<State>,
    pub(super) url: String,
    pub(super) seen: Seen,
    tasks: Vec<JoinHandle<()>>,
    pub(super) dir: PathBuf,
}
/// What a test changes about the default fixture: two enabled accounts with quota.
#[derive(Default)]
pub(super) struct Setup {
    /// `fixtureAccounts`, when not the default pair.
    pub(super) accounts: Option<Value>,
    /// Native configuration keys merged over the fixture's.
    pub(super) native: Value,
    /// Runtime YAML appended to the fixture's; its `api-keys` become credentials.
    pub(super) yaml: String,
    /// A Claude token endpoint. Account `a` is then the signed-in file `a.json` in the
    /// auth-dir, holding `token-a` and a refresh token.
    pub(super) token_url: Option<String>,
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
        Self::build(reply, None, None, None).await
    }
    /// No native accounts at all.
    async fn without_accounts(reply: Reply) -> Self {
        Self::build_with(reply, None, None, None, false).await
    }
    async fn with_history(reply: Reply, history: &Path) -> Self {
        Self::build(reply, Some(history), None, None).await
    }
    /// With history, and account `a` backed by `credential` in the runtime's store.
    async fn with_credential(reply: Reply, history: &Path, credential: Credential) -> Self {
        Self::build(reply, Some(history), Some(credential), None).await
    }
    /// Account `a` uses the outbound proxy at `proxy`.
    async fn with_proxy(reply: Reply, proxy: &str) -> Self {
        Self::build(reply, None, None, Some(proxy)).await
    }
    async fn build(reply: Reply, history: Option<&Path>, credential: Option<Credential>, proxy: Option<&str>) -> Self {
        Self::build_with(reply, history, credential, proxy, true).await
    }
    async fn build_with(
        reply: Reply,
        history: Option<&Path>,
        credential: Option<Credential>,
        proxy: Option<&str>,
        accounts: bool,
    ) -> Self {
        let setup = Setup {
            accounts: (!accounts).then(|| json!([])),
            ..Setup::default()
        };
        Self::build_from(reply, history, credential, proxy, setup).await
    }
    pub(super) async fn custom(reply: Reply, setup: Setup) -> Self {
        Self::build_from(reply, None, None, None, setup).await
    }
    async fn build_from(
        reply: Reply,
        history: Option<&Path>,
        credential: Option<Credential>,
        proxy: Option<&str>,
        setup: Setup,
    ) -> Self {
        let dir = std::env::temp_dir().join(format!("native-entitlement-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let credential = credential.or_else(|| setup.token_url.as_ref().map(|_| signed_in(&dir)));
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
                            if cpa_common::gostr::contains(&body, b"MODE_SLOW") {
                                tokio::time::sleep(Duration::from_millis(150)).await;
                            }
                            let response = if cpa_common::gostr::contains(&body, b"MODE_OPEN_STREAM") {
                                let stream = futures_util::stream::once(async {
                                    Ok::<_, std::io::Error>(Bytes::from_static(b"event: ping\n\n"))
                                })
                                .chain(futures_util::stream::pending());
                                Response::builder()
                                    .header("content-type", "text/event-stream")
                                    .body(Body::from_stream(stream))
                                    .unwrap()
                            } else if token == "Bearer token-a"
                                && cpa_common::gostr::contains(&body, b"claude-opus-test")
                            {
                                let zstd = reply.body.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]);
                                let exhausted = cpa_common::gostr::contains(&body, b"MODE_EXHAUSTED");
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
                                // The rate-limit headers of an account whose five-hour
                                // window is used up.
                                if exhausted {
                                    response = response
                                        .header("anthropic-ratelimit-unified-5h-status", "rejected")
                                        .header(
                                            "anthropic-ratelimit-unified-5h-reset",
                                            (now() / 1000 + 4 * 3600).to_string(),
                                        );
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
        let mut native = json!({
            "port":0,"httpProxyPort":0,"database":dir.join("routing.json"),"events":dir.join("events.jsonl"),
            "proxySecret":dir.join("secret"),"key":dir.join("key"),"cert":dir.join("cert"),"authDir":dir,
            "fixtureAccounts":setup.accounts.unwrap_or_else(|| json!([
                {"id":"a","token":"token-a","quota":quota,"fileId":credential.as_ref().map(|c| c.id.clone()).unwrap_or_default(),"proxyUrl":proxy},
                {"id":"b","token":"token-b","quota":quota}
            ])),
            "upstream":{"hostname":"127.0.0.1","port":port}
        });
        for (key, value) in setup.native.as_object().into_iter().flatten() {
            native[key] = value.clone();
        }
        let native: Config = serde_json::from_value(native).unwrap();
        let mut rt_yaml = format!("auth-dir: {}\n{}", dir.display(), setup.yaml);
        if let Some(history) = history {
            rt_yaml.push_str(&format!(
                "observability: {{history: {{enable: true, dir: '{}'}}}}\n",
                history.display()
            ));
        }
        let rt_config = cpa_core::config::Config::parse(&rt_yaml).unwrap();
        let credentials = credential
            .into_iter()
            .chain(cpa_core::config::credentials::from_config(&rt_config))
            .collect();
        let rt = Arc::new(crate::testing::runtime(
            rt_config,
            credentials,
            cpa_exec::Executors {
                claude: {
                    let claude = cpa_exec::claude::ClaudeExecutor::new(format!("https://127.0.0.1:{port}")).unwrap();
                    match setup.token_url.as_deref() {
                        Some(url) => claude.with_oauth(cpa_exec::oauth::OAuth::with_endpoints(
                            wreq::Client::builder().no_proxy().build().unwrap(),
                            url,
                            url,
                            url,
                        )),
                        None => claude,
                    }
                },
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        rt.start_activity(native.events.with_extension("sqlite")).unwrap();
        let state = Arc::new(State {
            events: diagnostics::Writer::start(native.events.clone(), rt.native_events.lost.clone()).unwrap(),
            config: native,
            rt,
            clients: Clients::new(|| native_client().tls_cert_verification(false)).unwrap(),
            tls: std::sync::RwLock::new(tls),
            secret: "synthetic-secret".into(),
            routing: Default::default(),
            sessions: Default::default(),
            skipped: Default::default(),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router_state = state.clone();
        let router = tokio::spawn(async move {
            while let Ok((io, _)) = listener.accept().await {
                tokio::spawn(connection(io, router_state.clone(), Serve::Tunnel));
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
    pub(super) async fn request(&self) -> wreq::Response {
        self.request_body(REQUEST).await
    }
    pub(super) async fn request_body(&self, body: &'static [u8]) -> wreq::Response {
        wreq::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .retry(wreq::retry::Policy::never())
            .build()
            .unwrap()
            .post(format!("{}/v1/messages", self.url))
            .header("user-agent", "synthetic-agent/1")
            .body(body)
            .send()
            .await
            .unwrap()
    }
}

/// A private Claude sign-in file `a.json` in `dir` with an access token that has not
/// expired, and its credential.
fn signed_in(dir: &Path) -> Credential {
    let file = json!({"type":"claude","access_token":"token-a","refresh_token":"fake-rt-a",
        "expired":"2099-01-01T00:00:00Z","account_uuid":"acct-a","claude_device_ids":["b".repeat(64)]});
    let path = dir.join("a.json");
    std::fs::write(&path, file.to_string()).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    crate::testing::local(Credential::from_file(dir, &path, file.as_object().unwrap().clone()).unwrap())
}

/// An upstream 401 on native routing refreshes that account once, off the request. When
/// the token endpoint refuses the refresh token, native routing stops offering the
/// account, as it does an expired one.
#[cfg(unix)]
#[tokio::test]
async fn a_native_401_takes_an_account_with_a_refused_refresh_token_out_of_rotation() {
    let refreshes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let token = axum::Router::new().route(
        "/token",
        axum::routing::post({
            let refreshes = refreshes.clone();
            move || async move {
                refreshes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                (StatusCode::BAD_REQUEST, r#"{"error":"invalid_grant"}"#)
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let token_url = format!("http://{}/token", listener.local_addr().unwrap());
    let token = tokio::spawn(async move { axum::serve(listener, token).await.unwrap() });
    let mut fixture = Fixture::custom(
        Reply {
            status: 401,
            body: br#"{"error":{"type":"authentication_error","message":"Invalid bearer token"}}"#.to_vec(),
            broken: false,
        },
        Setup {
            token_url: Some(token_url),
            ..Setup::default()
        },
    )
    .await;
    fixture.tasks.push(token);
    let offered = |fixture: &Fixture| {
        let accounts = fixture.state.read_accounts().unwrap();
        accounts.iter().find(|a| a.file_id == "a.json").unwrap().enabled
    };
    assert!(offered(&fixture));

    assert_eq!(fixture.request().await.bytes().await.unwrap(), SUCCESS);
    let tokens: Vec<_> = fixture.seen.lock().unwrap().iter().map(|(t, _)| t.clone()).collect();
    assert_eq!(tokens, ["Bearer token-a", "Bearer token-b"]);
    let store = fixture.state.rt.store();
    let a = store.get("a.json").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !store.out_of_rotation(&a) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the 401 recorded the refused refresh token");
    assert_eq!(refreshes.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(!offered(&fixture), "native routing still offers the refused account");
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

/// The router stops polling a body once it has sent the declared content-length, so the
/// exchange must finish when that many bytes arrived, including none at all.
#[tokio::test]
async fn a_reply_read_to_its_content_length_is_recorded_as_complete() {
    for (status, body, outcome, input_tokens) in [
        (200, SUCCESS.to_vec(), "success", json!(1)),
        (404, vec![b'x'; 70000], "not_found", Value::Null),
        (500, Vec::new(), "provider_error", Value::Null),
    ] {
        let fixture = Fixture::new(Reply {
            status,
            body: body.clone(),
            broken: false,
        })
        .await;
        enable_usage(&fixture);
        let response = fixture.request().await;
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()["content-length"], body.len().to_string().as_str());
        assert_eq!(response.bytes().await.unwrap(), body);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let records = loop {
            let records = fixture.state.rt.usage_queue().pop_oldest(10);
            if !records.is_empty() {
                break records;
            }
            assert!(tokio::time::Instant::now() < deadline, "no usage record for {status}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(records.len(), 1);
        let record: Value = serde_json::from_slice(&records[0]).unwrap();
        assert_eq!(record["failed"], status != 200, "{record}");
        let events = fixture.state.rt.native_events.snapshot()["events"].clone();
        let inference: Vec<_> = events
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == "inference")
            .collect();
        assert_eq!(inference.len(), 1, "{events}");
        let event = inference[0];
        assert_eq!(event["outcome"], outcome, "{event}");
        assert_eq!(event["complete"], true, "{event}");
        assert_eq!(event["status"], status, "{event}");
        assert_eq!(event["usage"]["input_tokens"], input_tokens, "{event}");
    }
}

fn enable_usage(fixture: &Fixture) {
    let usage = crate::testing::config("observability:\n  usage:\n    usage-statistics-enabled: true\n");
    fixture.state.rt.usage_queue().configure(true, &usage);
}

/// Exactly `count` usage records, waiting up to five seconds for them and briefly for any
/// extra one.
async fn usage_records(fixture: &Fixture, count: usize) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut records: Vec<Value> = Vec::new();
    while records.len() < count {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{} of {count} usage records: {records:?}",
            records.len()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
        for record in fixture.state.rt.usage_queue().pop_oldest(10) {
            records.push(serde_json::from_slice(&record).unwrap());
        }
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    for record in fixture.state.rt.usage_queue().pop_oldest(10) {
        records.push(serde_json::from_slice(&record).unwrap());
    }
    assert_eq!(records.len(), count, "{records:?}");
    records
}

/// Each native send leaves its own usage record under the request's id. One that failed
/// over is failed, with its status and the reason, never provider text, no tokens, and
/// the client's stream choice.
#[tokio::test]
async fn a_native_failover_leaves_a_failed_usage_record_before_the_answer() {
    const STREAMING: &[u8] =
        br#"{"model":"claude-opus-test","stream":true,"metadata":{"session_id":"synthetic-session"},"messages":[]}"#;
    for (status, body, reason) in [
        (
            403,
            br#"{"error":{"code":"model_access_denied"}}"#.to_vec(),
            "model_entitlement",
        ),
        (
            429,
            br#"{"error":{"type":"rate_limit_error","message":"synthetic"}}"#.to_vec(),
            "retryable_rejection",
        ),
        (503, Vec::new(), "retryable_rejection"),
    ] {
        let fixture = Fixture::new(Reply {
            status,
            body,
            broken: false,
        })
        .await;
        enable_usage(&fixture);
        let response = fixture.request_body(STREAMING).await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.bytes().await.unwrap(), SUCCESS);
        let records = usage_records(&fixture, 2).await;
        let failover = &records[0];
        assert_eq!(failover["failed"], true, "{failover}");
        assert_eq!(failover["fail"]["status_code"], status, "{failover}");
        assert_eq!(failover["fail"]["body"], reason, "{failover}");
        assert_eq!(failover["model"], "claude-opus-test", "{failover}");
        assert_eq!(failover["session_source"], "native", "{failover}");
        assert_eq!(failover["endpoint"], "POST /v1/messages", "{failover}");
        assert_eq!(failover["tokens"]["input_tokens"], 0, "{failover}");
        assert_eq!(failover["stream"], true, "{failover}");
        let answer = &records[1];
        assert!(
            failover["request_id"].as_str().is_some_and(|id| !id.is_empty()),
            "{failover}"
        );
        assert_eq!(failover["request_id"], answer["request_id"], "{answer}");
        assert_eq!(answer["failed"], false, "{answer}");
        assert_eq!(answer["tokens"]["input_tokens"], 1, "{answer}");
    }
}

/// A send whose connection was lost counts as a failed request for its credential and
/// leaves a failed usage record, statusless like a transport fault on the translating
/// path, and the request is still not replayed.
#[tokio::test]
async fn a_lost_native_send_leaves_a_failed_usage_record() {
    let mut metadata = serde_json::Map::new();
    metadata.insert("type".into(), "claude".into());
    let auth = Path::new("/synthetic-auth");
    let credential = Credential::from_file(auth, &auth.join("a.json"), metadata).unwrap();
    let id = credential.id.clone();
    let fixture = Fixture::build(
        Reply {
            status: 200,
            body: SUCCESS.to_vec(),
            broken: false,
        },
        None,
        Some(credential),
        Some("http://127.0.0.1:9"),
    )
    .await;
    enable_usage(&fixture);
    assert_eq!(fixture.request().await.status(), 422);
    let records = usage_records(&fixture, 1).await;
    assert_eq!(records[0]["failed"], true, "{}", records[0]);
    assert_eq!(records[0]["fail"]["status_code"], 500, "{}", records[0]);
    assert_eq!(records[0]["fail"]["body"], "transport_lost", "{}", records[0]);
    assert!(fixture.seen.lock().unwrap().is_empty());
    let activity = fixture.state.rt.store().activity(&id);
    assert_eq!((activity.success, activity.failed), (0, 1), "{activity:?}");
}

/// A send that failed over counts as a failed request for its credential.
#[tokio::test]
async fn a_native_failover_counts_as_a_failed_request_for_its_credential() {
    let dir = crate::request_logging::history::test_support::TempDir::new();
    let mut metadata = serde_json::Map::new();
    metadata.insert("type".into(), "claude".into());
    let auth = Path::new("/synthetic-auth");
    let credential = Credential::from_file(auth, &auth.join("a.json"), metadata).unwrap();
    let id = credential.id.clone();
    let fixture = Fixture::with_credential(
        Reply {
            status: 403,
            body: br#"{"error":{"code":"model_access_denied"}}"#.to_vec(),
            broken: false,
        },
        &dir.0,
        credential,
    )
    .await;
    assert_eq!(fixture.request().await.bytes().await.unwrap(), SUCCESS);
    let activity = fixture.state.rt.store().activity(&id);
    assert_eq!((activity.success, activity.failed), (0, 1), "{activity:?}");
}

/// A client that leaves before the response head leaves no usage record: the account
/// did not fail.
#[tokio::test]
async fn a_client_that_leaves_before_the_headers_leaves_no_usage_record() {
    use crate::request_logging::history::row::Outcome;
    let dir = crate::request_logging::history::test_support::TempDir::new();
    let fixture = Fixture::with_history(
        Reply {
            status: 200,
            body: SUCCESS.to_vec(),
            broken: false,
        },
        &dir.0,
    )
    .await;
    enable_usage(&fixture);
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
    assert!(fixture.state.rt.usage_queue().pop_oldest(10).is_empty());
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
            sent_at: std::time::Instant::now(),
            send: 0,
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
    let mut rows = crate::request_logging::history::test_support::rows(dir);
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
async fn a_native_row_names_the_store_credential_and_the_client() {
    let dir = crate::request_logging::history::test_support::TempDir::new();
    let mut metadata = serde_json::Map::new();
    metadata.insert("type".into(), "claude".into());
    metadata.insert("email".into(), "user-a@example.com".into());
    let auth = Path::new("/synthetic-auth");
    let credential = Credential::from_file(auth, &auth.join("a.json"), metadata).unwrap();
    let auth_index = cpa_core::config::credentials::auth_index(&credential);
    let fixture = Fixture::with_credential(
        Reply {
            status: 200,
            body: br#"{"content":[]}"#.to_vec(),
            broken: false,
        },
        &dir.0,
        credential,
    )
    .await;
    fixture.request().await.bytes().await.unwrap();
    let rows = flushed_rows(&fixture, &dir.0, 1).await;
    assert_eq!(rows[0].account.as_deref(), Some("user-a@example.com"));
    assert_eq!(rows[0].auth_index.as_deref(), Some(auth_index.as_str()));
    assert_eq!(rows[0].user_agent.as_deref(), Some("synthetic-agent/1"));
}

#[tokio::test]
async fn a_native_exchange_is_recorded_decoded() {
    use crate::request_logging::history::row::{Outcome, Source};
    let dir = crate::request_logging::history::test_support::TempDir::new();
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
    assert_eq!(row.transport, crate::request_logging::history::row::Transport::Http);
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
    let dir = crate::request_logging::history::test_support::TempDir::new();
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
    let dir = crate::request_logging::history::test_support::TempDir::new();
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
    let dir = crate::request_logging::history::test_support::TempDir::new();
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
    let dir = crate::request_logging::history::test_support::TempDir::new();
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

/// A skipped account file is reported once per reason, not on every pool read.
#[tokio::test]
async fn a_skipped_account_file_is_reported_once_per_reason() {
    let fixture = Fixture::new(Reply {
        status: 200,
        body: SUCCESS.to_vec(),
        broken: false,
    })
    .await;
    let reported = || -> Vec<Value> {
        fixture.state.rt.native_events.snapshot()["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == "account-skipped")
            .map(|e| json!([e["fileId"], e["reason"]]))
            .collect()
    };
    let skip = |reason| vec![("synthetic.json".to_owned(), reason)];
    fixture.state.report_skipped(skip(Skip::NotPrivate));
    fixture.state.report_skipped(skip(Skip::NotPrivate));
    assert_eq!(reported(), [json!(["synthetic.json", "not_private"])]);
    fixture.state.report_skipped(skip(Skip::Unreadable));
    assert_eq!(reported().len(), 2);
    assert!(reported().contains(&json!(["synthetic.json", "unreadable"])));
    fixture.state.report_skipped(Vec::new());
    fixture.state.report_skipped(skip(Skip::Unreadable));
    assert_eq!(reported().len(), 3);
}

/// A loopback HTTP CONNECT proxy that records each request line and tunnels to the
/// requested loopback target.
async fn connect_proxy() -> (String, Arc<Mutex<Vec<String>>>, JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = requests.clone();
    let task = tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if client.read_exact(&mut byte).await.is_err() {
                        return;
                    }
                    head.push(byte[0]);
                }
                let line = String::from_utf8_lossy(&head)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                log.lock().unwrap().push(line.clone());
                let Some(target) = line.strip_prefix("CONNECT ").and_then(|rest| rest.split(' ').next()) else {
                    return;
                };
                let Ok(mut upstream) = TcpStream::connect(target).await else {
                    return;
                };
                client
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await
                    .unwrap();
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    (url, requests, task)
}

/// An account with its own proxy sends inference, and its token, only through it.
#[tokio::test]
async fn an_account_proxy_carries_its_inference() {
    let (proxy, requests, task) = connect_proxy().await;
    let fixture = Fixture::with_proxy(
        Reply {
            status: 200,
            body: SUCCESS.to_vec(),
            broken: false,
        },
        &proxy,
    )
    .await;
    let response = fixture.request().await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap(), SUCCESS);
    assert_eq!(fixture.seen.lock().unwrap()[0].0, "Bearer token-a");
    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(requests[0].starts_with("CONNECT 127.0.0.1:"), "{requests:?}");
    task.abort();
}

/// An account whose proxy cannot be used fails before the send; its token never goes
/// out directly.
#[tokio::test]
async fn an_unusable_account_proxy_fails_before_the_send() {
    let fixture = Fixture::with_proxy(
        Reply {
            status: 200,
            body: SUCCESS.to_vec(),
            broken: false,
        },
        "ftp://unusable-proxy.invalid",
    )
    .await;
    let response = fixture.request().await;
    assert_eq!(response.status(), 502);
    assert!(
        fixture.seen.lock().unwrap().is_empty(),
        "the token was sent without its proxy"
    );
    let events = fixture.state.rt.native_events.snapshot()["events"].clone();
    assert!(
        events
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "rejected" && e["outcome"] == "routing_error" && e["account"] == "a"),
        "{events}"
    );
}

/// A request waits for another request of its session only while that one chooses an
/// account, not while its response streams; the streaming session stays active.
#[tokio::test]
async fn a_streaming_turn_does_not_hold_back_its_session() {
    const OPEN: &[u8] =
        br#"{"model":"claude-opus-test","metadata":{"session_id":"synthetic-session"},"messages":[],"mode":"MODE_OPEN_STREAM"}"#;
    let fixture = Fixture::new(Reply {
        status: 200,
        body: SUCCESS.to_vec(),
        broken: false,
    })
    .await;
    let open = fixture.request_body(OPEN).await;
    assert_eq!(open.status(), 200);
    let mut stream = open.bytes_stream();
    assert_eq!(stream.next().await.unwrap().unwrap(), &b"event: ping\n\n"[..]);
    let next = tokio::time::timeout(Duration::from_secs(5), fixture.request())
        .await
        .expect("a same-session request waited for an open stream");
    assert_eq!(next.status(), 200);
    assert_eq!(next.bytes().await.unwrap(), SUCCESS);
    let sid = hash("synthetic-session");
    let active = fixture.state.sessions.lock().unwrap().get(&sid).map(Arc::strong_count);
    assert_eq!(active, Some(2), "the open stream no longer marks its session active");
    drop(stream);
}

/// Requests of one session choose their account one at a time, so a new session binds
/// one account.
#[tokio::test]
async fn same_session_requests_choose_their_account_one_at_a_time() {
    let fixture = Fixture::new(Reply {
        status: 200,
        body: SUCCESS.to_vec(),
        broken: false,
    })
    .await;
    let session = fixture
        .state
        .sessions
        .lock()
        .unwrap()
        .entry(hash("synthetic-session"))
        .or_default()
        .clone();
    let choosing = session.lock().await;
    let request = fixture.request();
    tokio::pin!(request);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut request)
            .await
            .is_err(),
        "a request chose its account while another request of its session was choosing"
    );
    assert!(fixture.seen.lock().unwrap().is_empty());
    let events = fixture.state.rt.native_events.snapshot()["events"].clone();
    assert!(
        !events.as_array().unwrap().iter().any(|e| e["type"] == "selection"),
        "an account was chosen while another request of its session was choosing: {events}"
    );
    drop(choosing);
    let response = request.await;
    assert_eq!(response.status(), 200);
    assert_eq!(fixture.seen.lock().unwrap().len(), 1);
}

/// When every account file was skipped, a request fails as a routing error, not as
/// exhausted quota, and nothing is sent.
#[tokio::test]
async fn a_pool_of_only_skipped_files_is_a_routing_error() {
    let fixture = Fixture::without_accounts(Reply {
        status: 200,
        body: SUCCESS.to_vec(),
        broken: false,
    })
    .await;
    fixture
        .state
        .report_skipped(vec![("synthetic.json".to_owned(), Skip::InvalidProxy)]);
    let response = fixture.request().await;
    assert_eq!(response.status(), 502);
    assert!(fixture.seen.lock().unwrap().is_empty());
}

/// The fixture's `sends` rows once every queued record is written.
pub(super) async fn activity_sends(fixture: &Fixture) -> Vec<Value> {
    let rt = fixture.state.rt.clone();
    tokio::task::spawn_blocking(move || crate::testing::activity_sends(&rt))
        .await
        .unwrap()
}

fn send_column(sends: &[Value], name: &str) -> Vec<Value> {
    sends.iter().map(|send| send[name].clone()).collect()
}

#[tokio::test]
async fn a_failover_then_a_success_is_two_native_sends_each_timed_from_its_own_start() {
    let fixture = Fixture::new(Reply {
        status: 403,
        body: br#"{"error":{"code":"model_access_denied"}}"#.to_vec(),
        broken: false,
    })
    .await;
    let began = std::time::Instant::now();
    let response = fixture.request().await;
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    let elapsed = i64::try_from(began.elapsed().as_millis()).unwrap();
    let sends = activity_sends(&fixture).await;
    assert_eq!(sends.len(), 2, "{sends:?}");
    assert_eq!(sends[0]["request_id"], sends[1]["request_id"]);
    assert_eq!(send_column(&sends, "send"), [0, 1]);
    assert_eq!(send_column(&sends, "outcome"), ["failover", "complete"]);
    assert_eq!(sends[0]["status"], 403);
    assert_eq!(send_column(&sends, "source"), ["native", "native"]);
    assert_eq!(send_column(&sends, "provider"), ["claude", "claude"]);
    assert_eq!(send_column(&sends, "turn"), [0, 0]);
    let start = |i: usize| sends[i]["start_ms"].as_i64().unwrap();
    let latency = sends[0]["latency_ms"].as_i64().unwrap();
    assert!(start(1) >= start(0) + latency, "{sends:?}");
    // A send's latency counts from its own start, so it ends inside the client's wait.
    for send in &sends {
        let end = send["start_ms"].as_i64().unwrap() + send["latency_ms"].as_i64().unwrap();
        assert!(end <= elapsed, "{send} ended after the {elapsed} ms the client waited");
    }
}

#[tokio::test]
async fn an_account_without_a_credential_file_is_recorded_under_its_account_id() {
    let fixture = Fixture::new(Reply {
        status: 200,
        body: SUCCESS.to_vec(),
        broken: false,
    })
    .await;
    let mut headers = HeaderMap::new();
    headers.insert("anthropic-ratelimit-unified-5h-utilization", "0.3".parse().unwrap());
    fixture
        .state
        .rt
        .executors
        .claude
        .quota()
        .observe_model("a", "claude-opus-test", &headers);
    assert_eq!(fixture.request().await.bytes().await.unwrap(), SUCCESS);
    let sends = activity_sends(&fixture).await;
    assert_eq!(sends.len(), 1, "{sends:?}");
    assert_eq!(sends[0]["credential"], "a");
    let db = rusqlite::Connection::open(fixture.dir.join("events.sqlite")).unwrap();
    let readings: Vec<(String, f64)> = db
        .prepare("SELECT credential, used FROM quota_readings")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(readings, [("a".to_owned(), 30.0)]);
    assert_eq!(sends[0]["ttft_kind"], "byte");
    assert_eq!(sends[0]["outcome"], "complete");
    assert_eq!(sends[0]["model"], "claude-opus-test");
}

#[tokio::test]
async fn a_native_start_that_fails_leaves_no_activity_store() {
    let dir = std::env::temp_dir().join(format!("native-start-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
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
    let private = |file: &str, bytes: &[u8]| {
        std::fs::write(dir.join(file), bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join(file), std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    };
    private("key", &key.private_key_to_pem_pkcs8().unwrap());
    private("cert", &cert.build().to_pem().unwrap());
    private("secret", b"synthetic-secret-synthetic-secret-0123");
    // The diagnostics port is taken, so the start fails at its last listener.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let config = json!({
        "port":taken.local_addr().unwrap().port(),"httpProxyPort":0,"database":dir.join("routing.json"),
        "events":dir.join("events.jsonl"),"proxySecret":dir.join("secret"),"key":dir.join("key"),
        "cert":dir.join("cert"),"fixtureAccounts":[],"upstream":{"hostname":"127.0.0.1","port":1}
    });
    std::fs::write(dir.join("native.json"), config.to_string()).unwrap();
    let rt = Arc::new(crate::testing::runtime(
        cpa_core::config::Config::parse(&format!("auth-dir: {}\n", dir.join("auth").display())).unwrap(),
        Vec::new(),
        cpa_exec::Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("https://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        },
    ));
    let Err(error) = start_config(rt.clone(), &dir.join("native.json")).await else {
        panic!("the native start succeeded on a taken port")
    };
    assert!(
        format!("{error:#}").contains("native diagnostics listener"),
        "{error:#}"
    );
    assert!(rt.activity().is_none());
    assert!(!dir.join("events.sqlite").exists());
    drop(taken);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The CONNECT listener on loopback answers `/health`; on any other address it serves
/// CONNECT alone, so `/health` (no secret, a quota read per account) and every other
/// plain request are refused, and a CONNECT still needs the proxy secret.
#[tokio::test]
async fn connect_listener_beyond_loopback_serves_only_authenticated_connect() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let answer = |url: String, request: &'static str| async move {
        let mut io = TcpStream::connect(url).await.unwrap();
        io.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        io.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8_lossy(&response).into_owned();
        let status = response[..12].to_owned();
        let body = response.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned();
        (status, body)
    };
    const HEALTH: &str = "GET /health HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n";
    const CONNECT: &str = "CONNECT api.anthropic.com:443 HTTP/1.1\r\nhost: api.anthropic.com:443\r\n\r\n";
    for exposed in [false, true] {
        let native = if exposed {
            json!({"httpProxyHost": "0.0.0.0"})
        } else {
            json!({})
        };
        let fixture = Fixture::custom(
            Reply {
                status: 200,
                body: SUCCESS.to_vec(),
                broken: false,
            },
            Setup {
                native,
                ..Setup::default()
            },
        )
        .await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = listener.local_addr().unwrap().to_string();
        let state = fixture.state.clone();
        let server = tokio::spawn(async move {
            while let Ok((io, _)) = listener.accept().await {
                tokio::spawn(connection(io, state.clone(), state.config.connect_serves()));
            }
        });
        let (status, body) = answer(url.clone(), HEALTH).await;
        if exposed {
            assert_eq!(status, "HTTP/1.1 400");
            assert!(body.contains("Use CONNECT proxy protocol"), "{body}");
        } else {
            assert_eq!(status, "HTTP/1.1 200");
            assert!(body.contains("\"ready\":true"), "{body}");
        }
        assert_eq!(answer(url, CONNECT).await.0, "HTTP/1.1 407", "exposed: {exposed}");
        server.abort();
    }
}
