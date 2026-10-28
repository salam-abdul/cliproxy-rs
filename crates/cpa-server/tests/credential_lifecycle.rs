use std::sync::Arc;
use std::time::Duration;

use axum::{Json, Router, extract::State, routing::post};
use cpa_core::{
    config::Config,
    credential::{Credential, MetadataPatch},
};
use cpa_exec::{Executors, claude::ClaudeExecutor, oauth::OAuth};
use cpa_server::runtime::Runtime;
use serde_json::json;
use tokio::sync::Semaphore;

struct Fixture {
    dir: std::path::PathBuf,
    rt: Arc<Runtime>,
    entered: Arc<Semaphore>,
    release: Arc<Semaphore>,
    server: tokio::task::JoinHandle<()>,
}

async fn token(State((entered, release)): State<(Arc<Semaphore>, Arc<Semaphore>)>) -> Json<serde_json::Value> {
    entered.add_permits(1);
    release.acquire().await.unwrap().forget();
    Json(json!({"access_token":"sk-ant-oat-rotated-fake", "refresh_token":"rotated-fake", "expires_in":3600}))
}

impl Fixture {
    async fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("cpa-lifecycle-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let entered = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/token", post(token))
            .with_state((entered.clone(), release.clone()));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let metadata = json!({"type":"claude", "access_token":"sk-ant-oat-old-fake", "refresh_token":"old-fake",
            "expired":"2000-01-01T00:00:00Z", "account_uuid":"account-fake", "claude_device_ids":["b".repeat(64)]});
        let path = dir.join("account.json");
        std::fs::write(&path, metadata.to_string()).unwrap();
        let hash = bcrypt::hash("fake-management-only", 4).unwrap();
        let cfg_text = format!("auth-dir: {}\nmanagement:\n  secret-key: '{hash}'\n", dir.display());
        std::fs::write(dir.join("config.yaml"), &cfg_text).unwrap();
        let cfg = Config::parse(&cfg_text).unwrap();
        let credential = Credential::from_file(&dir, &path, metadata.as_object().unwrap().clone()).unwrap();
        let client = wreq::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let oauth = OAuth::with_endpoints(
            client,
            &format!("{url}/token"),
            &format!("{url}/profile"),
            &format!("{url}/roles"),
        );
        let rt = Arc::new(cpa_server::testing::runtime(
            cfg,
            vec![credential],
            Executors {
                claude: ClaudeExecutor::new(&url).unwrap().with_oauth(oauth),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        Self {
            dir,
            rt,
            entered,
            release,
            server,
        }
    }

    async fn start_refresh(&self) -> tokio::task::JoinHandle<Result<Arc<Credential>, cpa_core::exec::ExecError>> {
        let rt = self.rt.clone();
        let task = tokio::spawn(async move { rt.refresh_credential("account.json").await });
        tokio::time::timeout(Duration::from_secs(5), self.entered.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        task
    }

    fn patch(&self, value: serde_json::Value) {
        let current = self.rt.store().get("account.json").unwrap();
        self.rt
            .store()
            .apply_patch(
                &current.id,
                current.revision,
                &MetadataPatch {
                    set: value.as_object().unwrap().clone(),
                    remove: vec![],
                },
            )
            .unwrap();
    }

    async fn management(&self) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = cpa_server::management::router(cpa_server::management::Management::new(
            self.rt.clone(),
            self.dir.join("config.yaml"),
        ));
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap();
        });
        (url, server)
    }

    fn disk(&self) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(self.dir.join("account.json")).unwrap()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn refresh_keeps_rotated_tokens_and_concurrent_metadata_edits() {
    let f = Fixture::new().await;
    let refresh = f.start_refresh().await;
    f.patch(json!({"note":"edited", "disabled":true, "email":"edited@example.invalid"}));
    f.release.add_permits(1);
    let result = tokio::time::timeout(Duration::from_secs(5), refresh)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.str("refresh_token"), Some("rotated-fake"));
    assert_eq!(result.str("note"), Some("edited"));
    assert_eq!(result.str("email"), Some("edited@example.invalid"));
    assert!(result.disabled);
    assert_eq!(f.disk()["refresh_token"], "rotated-fake");
}

#[tokio::test]
async fn refresh_keeps_replacement_tokens_even_when_restored_to_the_original_values() {
    for restore in [false, true] {
        let f = Fixture::new().await;
        let refresh = f.start_refresh().await;
        f.patch(json!({"access_token":"replacement-fake", "refresh_token":"replacement-refresh-fake"}));
        if restore {
            f.patch(json!({"access_token":"sk-ant-oat-old-fake", "refresh_token":"old-fake"}));
        }
        let before = f.disk();
        let revision = f.rt.store().get("account.json").unwrap().revision;
        f.release.add_permits(1);
        let result = tokio::time::timeout(Duration::from_secs(5), refresh)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result.revision, revision);
        assert_eq!(f.disk(), before);
    }
}

#[tokio::test]
async fn deleted_and_recreated_credentials_reject_an_older_refresh() {
    let f = Fixture::new().await;
    let refresh = f.start_refresh().await;
    let before = f.rt.store().get("account.json").unwrap();
    f.rt.store().reconcile(vec![]);
    f.rt.store().reconcile(vec![Credential::clone(&before)]);
    f.release.add_permits(1);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), refresh)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(
        f.rt.store().get("account.json").unwrap().str("refresh_token"),
        Some("old-fake")
    );
    assert_eq!(f.disk()["refresh_token"], "old-fake");
}

#[tokio::test]
async fn management_uploads_and_deletes_win_over_in_flight_refreshes() {
    for delete in [false, true] {
        let f = Fixture::new().await;
        let refresh = f.start_refresh().await;
        let (url, server) = f.management().await;
        let client = wreq::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("{url}/v8/management/credentials?name=account.json");
        let request = if delete {
            client.delete(&url)
        } else {
            client
                .post(&url)
                .header("content-type", "application/json")
                .body(r#"{"type":"claude","access_token":"uploaded-fake","refresh_token":"uploaded-refresh-fake"}"#)
        };
        let response = request.bearer_auth("fake-management-only").send().await.unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(status, 200, "{body}");
        f.release.add_permits(1);
        let result = tokio::time::timeout(Duration::from_secs(5), refresh)
            .await
            .unwrap()
            .unwrap();
        server.abort();
        if delete {
            assert!(result.is_err());
            assert!(f.rt.store().get("account.json").is_none());
            assert!(!f.dir.join("account.json").exists());
        } else {
            assert_eq!(result.unwrap().str("refresh_token"), Some("uploaded-refresh-fake"));
            assert_eq!(f.disk()["refresh_token"], "uploaded-refresh-fake");
        }
    }
}

#[tokio::test]
async fn refresh_and_reload_preserve_active_quota_cooldowns() {
    use cpa_core::exec::{ExecError, FailureScope};
    use cpa_server::runtime::{Outcome, Selection};
    let f = Fixture::new().await;
    let lease =
        f.rt.store()
            .select(Selection {
                provider: "claude".into(),
                ..Default::default()
            })
            .unwrap();
    let mut quota = ExecError::local(429, FailureScope::Credential, "usage_limit_reached");
    quota.retry_after = Some(Duration::from_secs(600));
    lease.complete(Outcome::Failure(quota));
    let before = f.rt.store().cooldowns("account.json");
    assert_eq!(before.len(), 1);
    assert!(before[0].quota);
    let refresh = f.start_refresh().await;
    f.patch(json!({"note":"edited"}));
    f.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), refresh)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let state = cpa_server::management::Management::new(f.rt.clone(), f.dir.join("config.yaml"));
    cpa_server::watching::reload(&state).unwrap();
    let after = f.rt.store().cooldowns("account.json");
    assert_eq!(after.len(), 1);
    assert!(after[0].quota && after[0].remaining > Duration::ZERO);
    assert_eq!(
        f.rt.store().get("account.json").unwrap().str("refresh_token"),
        Some("rotated-fake")
    );
}

#[tokio::test]
async fn management_retyping_invalidates_an_in_flight_refresh() {
    let f = Fixture::new().await;
    let refresh = f.start_refresh().await;
    let (url, server) = f.management().await;
    let response = wreq::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
        .patch(format!("{url}/v8/management/credentials/fields"))
        .bearer_auth("fake-management-only")
        .json(&json!({"name":"account.json", "type":"codex"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    f.release.add_permits(1);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), refresh)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    server.abort();
    assert_eq!(f.rt.store().get("account.json").unwrap().provider, "codex");
    assert_eq!(f.disk()["refresh_token"], "old-fake");
}
