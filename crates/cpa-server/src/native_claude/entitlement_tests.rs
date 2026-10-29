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
                            let response = if token == "Bearer token-a"
                                && body
                                    .windows(b"claude-opus-test".len())
                                    .any(|w| w == b"claude-opus-test")
                            {
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
                                Response::builder()
                                    .status(reply.status)
                                    .header("content-type", "application/json")
                                    .header("x-request-id", "synthetic-id")
                                    .body(body)
                                    .unwrap()
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
        let rt_config = cpa_core::config::Config::parse(&format!("auth-dir: {}\n", dir.display())).unwrap();
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
