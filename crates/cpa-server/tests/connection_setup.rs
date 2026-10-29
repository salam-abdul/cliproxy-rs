//! Synthetic loopback inference: retry only before any request can have been sent.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::{Runtime, router};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn credential(name: &str, base: &str, proxy: Option<&str>) -> Credential {
    let mut metadata = serde_json::json!({
        "type": "claude", "access_token": format!("fake-{name}"),
        "expired": "2099-01-01T00:00:00Z",
        "request_scoped_errors": [{"status":502,"match":["upstream request failed"],"action":"continue"}]
    });
    if let Some(proxy) = proxy {
        metadata["proxy_url"] = proxy.into();
    }
    let mut credential = Credential::from_file(
        Path::new("/synthetic"),
        &Path::new("/synthetic").join(name),
        metadata.as_object().unwrap().clone(),
    )
    .unwrap();
    credential.attributes.insert("base_url".into(), base.into());
    cpa_server::testing::local(credential)
}

async fn fixture(credentials: Vec<Credential>) -> (Arc<Runtime>, String, tokio::task::JoinHandle<()>) {
    let mut config = Config::parse("requests: {tls-handshake-timeout-seconds: 1}\n").unwrap();
    config.auth_dir = std::env::temp_dir().join(format!("cpa-connection-setup-{}", uuid::Uuid::new_v4()));
    let runtime = Arc::new(cpa_server::testing::runtime(
        config,
        credentials,
        Executors {
            claude: ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = router(runtime.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (runtime, url, task)
}

async fn read_request(socket: &mut tokio::net::TcpStream) {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(socket.read_u8().await.unwrap());
    }
    let head = String::from_utf8(head).unwrap().to_ascii_lowercase();
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await.unwrap();
    assert!(serde_json::from_slice::<serde_json::Value>(&body).is_ok());
}

async fn successful_upstream() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            read_request(&mut socket).await;
            count.fetch_add(1, Ordering::SeqCst);
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}").await.unwrap();
        }
    });
    (url, hits, task)
}

async fn request(url: &str) -> wreq::Response {
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        wreq::Client::new()
            .post(format!("{url}/v1/messages"))
            .body(r#"{"model":"claude-sonnet-5","messages":[{"role":"user","content":"synthetic"}]}"#)
            .send(),
    )
    .await
    .unwrap()
    .unwrap()
}

#[tokio::test]
async fn connection_setup_timeout_fails_over_without_cooldown_or_duplicate_inference() {
    let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    let setup_count = Arc::new(AtomicUsize::new(0));
    let count = setup_count.clone();
    let silent = tokio::spawn(async move {
        while let Ok((mut socket, _)) = proxy.accept().await {
            count.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                socket.read_to_end(&mut bytes).await.unwrap();
                assert!(bytes.starts_with(b"CONNECT api.anthropic.com:443 "));
                assert!(bytes.ends_with(b"\r\n\r\n"), "nothing beyond CONNECT was sent");
            });
        }
    });
    let (base, inference_count, upstream) = successful_upstream().await;
    let (runtime, url, server) = fixture(vec![
        credential("a", "https://api.anthropic.com", Some(&proxy_url)),
        credential("b", &base, None),
    ])
    .await;
    let response = request(&url).await;
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.text().await.unwrap(), r#"{"ok":true}"#);
    assert_eq!(setup_count.load(Ordering::SeqCst), 1);
    assert_eq!(inference_count.load(Ordering::SeqCst), 1);
    assert!(runtime.store().cooldowns("a").is_empty());
    assert!(runtime.store().cooldowns("b").is_empty());
    server.abort();
    upstream.abort();
    silent.abort();
}

#[tokio::test]
async fn ambiguous_send_read_and_redirect_failures_never_fail_over_with_continue_rule() {
    for mode in 0..3 {
        let broken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", broken.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirect = format!("http://{}/target", dead.local_addr().unwrap());
        drop(dead);
        let broken_task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = broken.accept().await {
                read_request(&mut socket).await;
                seen.fetch_add(1, Ordering::SeqCst);
                if mode == 1 {
                    socket
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\npartial")
                        .await
                        .unwrap();
                } else if mode == 2 {
                    socket.write_all(format!("HTTP/1.1 307 Redirect\r\nLocation: {redirect}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                }
            }
        });
        let (fallback, fallback_count, upstream) = successful_upstream().await;
        let (runtime, url, server) =
            fixture(vec![credential("a", &base, None), credential("b", &fallback, None)]).await;
        let response = request(&url).await;
        assert_eq!(response.status().as_u16(), 502, "mode={mode}");
        let _ = response.bytes().await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1, "mode={mode}");
        assert_eq!(fallback_count.load(Ordering::SeqCst), 0, "mode={mode}");
        assert!(runtime.store().cooldowns("a").is_empty());
        server.abort();
        upstream.abort();
        broken_task.abort();
    }
}
