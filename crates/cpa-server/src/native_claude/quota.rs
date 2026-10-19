use serde_json::Value;
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::Mutex as AsyncMutex, time::Instant};

const TTL: Duration = Duration::from_secs(30);
const CAPACITY: usize = 64;
type Snapshot = Option<(Instant, Value)>;

struct Entry {
    value: Arc<AsyncMutex<Snapshot>>,
    used: Instant,
}

#[derive(Default)]
pub(super) struct Cache(Mutex<HashMap<String, Entry>>);

impl Cache {
    pub(super) async fn invalidate(&self, key: &str) {
        let value = self.0.lock().unwrap().get(key).map(|entry| entry.value.clone());
        if let Some(value) = value {
            *value.lock().await = None;
        }
    }

    pub(super) async fn get<F>(&self, key: String, fetch: F) -> anyhow::Result<Value>
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
        if let Some((_, value)) = snapshot.as_ref().filter(|(at, _)| at.elapsed() < TTL) {
            return Ok(value.clone());
        }
        let fetched = fetch.await?;
        *snapshot = Some((Instant::now(), fetched.clone()));
        Ok(fetched)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
