use serde_json::Value;
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::Mutex as AsyncMutex, time::Instant};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};

#[derive(Default)]
pub(crate) struct ClaudeUsage {
    cache: Cache,
    #[cfg(test)]
    endpoint: Option<String>,
}

impl ClaudeUsage {
    pub(crate) async fn get(&self, token: &str, client: &wreq::Client, deny_external: bool) -> anyhow::Result<Value> {
        anyhow::ensure!(!token.is_empty(), "Claude access token is missing");
        let endpoint = "https://api.anthropic.com/api/oauth/usage";
        #[cfg(test)]
        let endpoint = self.endpoint.as_deref().unwrap_or(endpoint);
        anyhow::ensure!(
            !deny_external || endpoint.starts_with("http://127.0.0.1:"),
            "External usage requests disabled"
        );
        self.cache
            .get(token_key(token), async {
                let response = client
                    .get(endpoint)
                    .redirect(wreq::redirect::Policy::none())
                    .bearer_auth(token)
                    .header("anthropic-beta", "oauth-2025-04-20")
                    .header("user-agent", "claude-code/2.1.296")
                    .timeout(Duration::from_secs(15))
                    .send()
                    .await
                    .map_err(|_| anyhow::anyhow!("Usage request failed"))?;
                anyhow::ensure!(
                    response.status().as_u16() == 200,
                    "Claude usage provider answered HTTP {}",
                    response.status().as_u16()
                );
                let mut bytes = Vec::new();
                let mut stream = response.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(|_| anyhow::anyhow!("Usage response interrupted"))?;
                    anyhow::ensure!(chunk.len() <= 1048576 - bytes.len(), "Usage response too large");
                    bytes.extend_from_slice(&chunk);
                }
                serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("Invalid usage response"))
            })
            .await
    }

    pub(crate) async fn invalidate(&self, token: &str) {
        self.cache.invalidate(&token_key(token)).await;
    }
}

fn token_key(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

fn now() -> u64 {
    crate::quota_time::epoch_millis(std::time::SystemTime::now()).unwrap_or_default()
}

pub(crate) fn timestamp(value: &Value) -> Option<u64> {
    crate::quota_time::rfc3339(value.as_str()?).and_then(crate::quota_time::epoch_millis)
}
fn cache_lifetime(q: &Value, at: u64) -> Duration {
    q.as_object()
        .into_iter()
        .flat_map(|q| q.values())
        .filter_map(|v| timestamp(&v["resets_at"]))
        .filter(|r| *r > at)
        .map(|r| Duration::from_millis(r - at))
        .min()
        .unwrap_or(TTL)
        .min(TTL)
}

const TTL: Duration = Duration::from_secs(30);
const CAPACITY: usize = 64;
type Snapshot = Option<(Instant, Value)>;

struct Entry {
    value: Arc<AsyncMutex<Snapshot>>,
    used: Instant,
}

#[derive(Default)]
struct Cache(Mutex<HashMap<String, Entry>>);

impl Cache {
    async fn invalidate(&self, key: &str) {
        let value = self.0.lock().unwrap().get(key).map(|entry| entry.value.clone());
        if let Some(value) = value {
            *value.lock().await = None;
        }
    }

    async fn get<F>(&self, key: String, fetch: F) -> anyhow::Result<Value>
    where
        F: Future<Output = anyhow::Result<Value>>,
    {
        let value = {
            let mut entries = self.0.lock().unwrap();
            if !entries.contains_key(&key) && entries.len() == CAPACITY {
                let oldest = entries
                    .iter()
                    .filter(|(_, entry)| Arc::strong_count(&entry.value) == 1)
                    .min_by_key(|(_, entry)| entry.used)
                    .map(|(key, _)| key.clone());
                let Some(oldest) = oldest else {
                    anyhow::bail!("Quota refresh capacity exhausted");
                };
                entries.remove(&oldest);
            }
            let entry = entries.entry(key).or_insert_with(|| Entry {
                value: Default::default(),
                used: Instant::now(),
            });
            entry.used = Instant::now();
            entry.value.clone()
        };
        let mut snapshot = value.lock().await;
        if let Some((_, value)) = snapshot.as_ref().filter(|(expires, _)| *expires > Instant::now()) {
            return Ok(value.clone());
        }
        let fetched = fetch.await?;
        *snapshot = Some((Instant::now() + cache_lifetime(&fetched, now()), fetched.clone()));
        Ok(fetched)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::future::IntoFuture;

    #[tokio::test]
    async fn management_preserves_token_forms_and_proxy_and_shares_the_native_cache() {
        use crate::management::{self, Management};
        use axum::{Router, body::Body, extract::ConnectInfo, http::Request, routing::get};
        use cpa_core::{
            config::{Config, credentials::auth_index},
            credential::Credential,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tower::ServiceExt;
        let dir = std::env::temp_dir().join(format!("usage-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(
            axum::serve(
                listener,
                Router::new().route(
                    "/usage",
                    get({
                        let calls = calls.clone();
                        move |headers: axum::http::HeaderMap| {
                            let calls = calls.clone();
                            async move {
                                assert_eq!(headers["authorization"], "Bearer synthetic-one");
                                calls.fetch_add(1, Ordering::SeqCst);
                                axum::Json(json!({"five_hour":{"utilization":25},"seven_day":{"utilization":10}}))
                            }
                        }
                    }),
                ),
            )
            .into_future(),
        );
        let hash = bcrypt::hash("synthetic-management", 4).unwrap();
        let cfg = Config::parse(&format!(
            "oauth:\n  auth-dir: {}\nmanagement:\n  secret-key: '{hash}'\n",
            dir.display()
        ))
        .unwrap();
        let credentials: Vec<_> = [
            json!({"access_token":"synthetic-one"}),
            json!({"accessToken":" synthetic-one "}),
            json!({"token":{"access_token":"synthetic-one"}}),
            json!({"token":{"accessToken":"synthetic-one"}}),
            json!({"access_token":"synthetic-one", "disabled":true}),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, mut metadata)| {
            metadata["type"] = json!("claude");
            metadata["proxy_url"] = json!(proxy);
            Credential::from_file(
                &dir,
                &dir.join(format!("account-{i}.json")),
                metadata.as_object().unwrap().clone(),
            )
            .unwrap()
        })
        .collect();
        let indices: Vec<_> = credentials.iter().map(auth_index).collect();
        let mut rt = crate::testing::runtime(
            cfg,
            credentials,
            cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        );
        rt.claude_usage.endpoint = Some("http://127.0.0.1:1/usage".into());
        let rt = Arc::new(rt);
        let router = management::router(Management::new(rt.clone(), dir.join("config.yaml")));
        let request = |index: &str, authenticated: bool| {
            let mut request =
                Request::builder().uri(format!("/v8/management/observability/claude/usage?auth_index={index}"));
            if authenticated {
                request = request.header("authorization", "Bearer synthetic-management");
            }
            request
                .extension(ConnectInfo("127.0.0.1:40000".parse::<std::net::SocketAddr>().unwrap()))
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(
            router
                .clone()
                .oneshot(request(&indices[0], false))
                .await
                .unwrap()
                .status(),
            401
        );
        for index in &indices[..4] {
            rt.claude_usage.invalidate("synthetic-one").await;
            let response = router.clone().oneshot(request(index, true)).await.unwrap();
            assert_eq!(response.status(), 200);
            let body = axum::body::to_bytes(response.into_body(), 4096).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap()["five_hour"]["utilization"],
                25
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        let direct = wreq::Client::builder().no_proxy().build().unwrap();
        assert_eq!(
            rt.claude_usage.get("synthetic-one", &direct, true).await.unwrap()["five_hour"]["utilization"],
            25
        );
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(router.oneshot(request(&indices[4], true)).await.unwrap().status(), 400);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        server.abort();
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn shared_http_reads_rotate_invalidate_and_reject_unsafe_answers() {
        use axum::{Router, body::Body, response::Response, routing::get};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let client = wreq::Client::builder().no_proxy().build().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mode = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let usage = ClaudeUsage {
            endpoint: Some(format!("http://{}/usage", listener.local_addr().unwrap())),
            ..Default::default()
        };
        let server = tokio::spawn(
            axum::serve(
                listener,
                Router::new().route(
                    "/usage",
                    get({
                        let calls = calls.clone();
                        let mode = mode.clone();
                        move |headers: axum::http::HeaderMap| {
                            let calls = calls.clone();
                            let mode = mode.clone();
                            async move {
                                assert!(matches!(
                                    headers["authorization"].to_str().unwrap(),
                                    "Bearer synthetic-one" | "Bearer synthetic-two"
                                ));
                                assert_eq!(headers["anthropic-beta"], "oauth-2025-04-20");
                                assert!(!headers.contains_key("x-cpa-local-dashboard"));
                                calls.fetch_add(1, Ordering::SeqCst);
                                tokio::task::yield_now().await;
                                match mode.load(Ordering::SeqCst) {
                                    1 => Response::builder()
                                        .status(429)
                                        .body(Body::from("private error body"))
                                        .unwrap(),
                                    2 => Response::builder()
                                        .status(302)
                                        .header("location", "https://example.invalid/private")
                                        .body(Body::empty())
                                        .unwrap(),
                                    3 => Response::new(Body::from("x".repeat(1048577))),
                                    4 => Response::new(Body::from("private invalid body")),
                                    _ => Response::new(Body::from(
                                        r#"{"five_hour":{"utilization":25},"seven_day":{"utilization":10}}"#,
                                    )),
                                }
                            }
                        }
                    }),
                ),
            )
            .into_future(),
        );
        let (native, dashboard) = tokio::join!(
            usage.get("synthetic-one", &client, true),
            usage.get("synthetic-one", &client, true)
        );
        assert_eq!(native.unwrap(), dashboard.unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        usage.get("synthetic-two", &client, true).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        usage.invalidate("synthetic-one").await;
        usage.get("synthetic-one", &client, true).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        for (value, expected) in [(1, "HTTP 429"), (2, "HTTP 302"), (3, "too large"), (4, "Invalid usage")] {
            mode.store(value, Ordering::SeqCst);
            usage.invalidate("synthetic-one").await;
            let error = usage.get("synthetic-one", &client, true).await.unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("private"));
            assert!(!error.contains("synthetic"));
        }
        mode.store(0, Ordering::SeqCst);
        assert!(usage.get("synthetic-one", &client, true).await.is_ok());
        assert!(
            ClaudeUsage::default()
                .get("synthetic-one", &client, true)
                .await
                .is_err()
        );
        server.abort();
    }
    #[tokio::test(start_paused = true)]
    async fn reset_expires_cache_before_ttl_and_requires_new_confirmation() {
        let cache = Cache::default();
        let reset = chrono::Utc::now() + chrono::Duration::seconds(2);
        let q = json!({"five_hour":{"utilization":100,"resets_at":reset.to_rfc3339()}});
        cache.get("a".into(), async { Ok(q) }).await.unwrap();
        tokio::time::advance(Duration::from_secs(3)).await;
        assert!(
            cache
                .get("a".into(), async { anyhow::bail!("usage unavailable") })
                .await
                .is_err()
        );
        assert_eq!(
            cache
                .get("a".into(), async { Ok(json!({"five_hour":{"utilization":0}})) })
                .await
                .unwrap()["five_hour"]["utilization"],
            0
        );
    }

    #[tokio::test(start_paused = true)]
    async fn independent_accounts_refresh_concurrently() {
        let cache = Cache::default();
        let started = Instant::now();
        let fetch = || async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(json!({"utilization": 20}))
        };
        let (a, b, c) = tokio::join!(
            cache.get("account-a-token-1".into(), fetch()),
            cache.get("account-b-token-1".into(), fetch()),
            cache.get("account-c-token-1".into(), fetch())
        );
        assert!(a.is_ok() && b.is_ok() && c.is_ok());
        assert_eq!(started.elapsed(), Duration::from_millis(100));
    }
    #[tokio::test(start_paused = true)]
    async fn coalesces_refreshes_and_expires_without_reusing_rotated_tokens() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cache = Cache::default();
        let calls = AtomicUsize::new(0);
        let fetch = || async {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(json!(n))
        };
        let (a, b) = tokio::join!(
            cache.get("a-token1".into(), fetch()),
            cache.get("a-token1".into(), fetch())
        );
        assert_eq!(a.unwrap(), b.unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(cache.get("a-token2".into(), fetch()).await.unwrap(), json!(1));
        tokio::time::advance(TTL).await;
        assert_eq!(cache.get("a-token1".into(), fetch()).await.unwrap(), json!(2));
        cache.invalidate("a-token1").await;
        assert_eq!(cache.get("a-token1".into(), fetch()).await.unwrap(), json!(3));
    }

    #[tokio::test(start_paused = true)]
    async fn failures_and_cancellation_cannot_supply_stale_quota_or_hold_refresh() {
        let cache = Arc::new(Cache::default());
        cache.get("a".into(), async { Ok(json!(10)) }).await.unwrap();
        tokio::time::advance(TTL).await;
        assert!(
            cache
                .get("a".into(), async { anyhow::bail!("unavailable") })
                .await
                .is_err()
        );
        let started = Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn({
            let cache = cache.clone();
            let started = started.clone();
            async move {
                cache
                    .get("a".into(), async {
                        started.notify_one();
                        std::future::pending().await
                    })
                    .await
            }
        });
        started.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let value = tokio::time::timeout(Duration::from_secs(1), cache.get("a".into(), async { Ok(json!(90)) })).await;
        assert_eq!(value.unwrap().unwrap(), json!(90));
    }

    #[tokio::test(start_paused = true)]
    async fn capacity_evicts_idle_entries_but_never_splits_active_refreshes() {
        let cache = Arc::new(Cache::default());
        for i in 0..CAPACITY * 2 {
            cache.get(i.to_string(), async { Ok(json!(i)) }).await.unwrap();
        }
        assert_eq!(cache.0.lock().unwrap().len(), CAPACITY);
        let started = Arc::new(tokio::sync::Semaphore::new(0));
        let mut tasks = Vec::new();
        for i in 0..CAPACITY {
            let cache = cache.clone();
            let started = started.clone();
            tasks.push(tokio::spawn(async move {
                cache
                    .get(format!("busy-{i}"), async {
                        started.add_permits(1);
                        std::future::pending().await
                    })
                    .await
            }));
        }
        started.acquire_many(CAPACITY as u32).await.unwrap().forget();
        assert!(
            cache
                .get("overflow".into(), async { panic!("must not fetch") })
                .await
                .is_err()
        );
        for task in tasks {
            task.abort();
            let _ = task.await;
        }
        assert_eq!(
            cache.get("recovered".into(), async { Ok(json!(0)) }).await.unwrap(),
            json!(0)
        );
        assert_eq!(cache.0.lock().unwrap().len(), CAPACITY);
    }
    #[tokio::test]
    #[ignore = "manual wall-clock benchmark"]
    async fn refresh_latency() {
        let mut serial = Vec::new();
        let mut concurrent = Vec::new();
        for _ in 0..5 {
            let fetch = || async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(json!({"utilization": 20}))
            };
            let cache = Cache::default();
            let start = Instant::now();
            for id in 0..3 {
                cache.get(id.to_string(), fetch()).await.unwrap();
            }
            serial.push(start.elapsed().as_micros());
            let cache = Cache::default();
            let start = Instant::now();
            let (a, b, c) = tokio::join!(
                cache.get("a".into(), fetch()),
                cache.get("b".into(), fetch()),
                cache.get("c".into(), fetch())
            );
            assert!(a.is_ok() && b.is_ok() && c.is_ok());
            concurrent.push(start.elapsed().as_micros());
        }
        serial.sort_unstable();
        concurrent.sort_unstable();
        println!(
            "three independent 100ms quota refreshes, five runs: serial median={}us, concurrent median={}us",
            serial[2], concurrent[2]
        );
    }
}
