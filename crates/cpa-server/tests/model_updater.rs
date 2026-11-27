//! Remote model catalog refresh against local mock sources. Its own test binary: the
//! refresh swaps the process-wide static catalog.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use cpa_core::registry::{catalog_generation, pinned};

const EMBEDDED: &str = include_str!("../../cpa-core/src/registry/models.json");

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

/// The embedded catalog with one extra Claude model.
fn with_extra_claude_model() -> String {
    let mut root: serde_json::Map<String, serde_json::Value> = serde_json::from_str(EMBEDDED).unwrap();
    let mut extra = root["claude"][0].as_object().unwrap().clone();
    extra.insert("id".into(), "claude-refreshed-1".into());
    root["claude"].as_array_mut().unwrap().push(extra.into());
    serde_json::to_string(&root).unwrap()
}

#[tokio::test]
async fn refresh_skips_failing_sources_and_swaps_a_changed_catalog() {
    let hits = Arc::new(AtomicUsize::new(0));
    let modified = with_extra_claude_model();
    let invalid = r#"{"claude":[{"id":"a"},{"id":"a"}]}"#.to_owned();
    let without_meta = {
        let mut root: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&modified).unwrap();
        root.insert("meta".into(), serde_json::Value::Array(Vec::new()));
        serde_json::to_string(&root).unwrap()
    };
    let app = axum::Router::new()
        .route(
            "/{name}",
            axum::routing::get(move |State(hits): State<Arc<AtomicUsize>>, Path(name): Path<String>| {
                let (modified, invalid, without_meta) = (modified.clone(), invalid.clone(), without_meta.clone());
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    match name.as_str() {
                        "down" => (StatusCode::INTERNAL_SERVER_ERROR, String::new()).into_response(),
                        "invalid" => invalid.into_response(),
                        "modified" => modified.into_response(),
                        _ => without_meta.into_response(),
                    }
                }
            }),
        )
        .with_state(hits.clone());
    let base = serve(app).await;
    let urls = |names: &[&str]| names.iter().map(|n| format!("{base}/{n}")).collect::<Vec<_>>();

    assert!(pinned().lookup("claude-refreshed-1").is_none());
    let mut metadata = serde_json::Map::new();
    metadata.insert("type".into(), "claude".into());
    metadata.insert("access_token".into(), "fake-token".into());
    let credential = cpa_core::credential::Credential::from_file(
        std::path::Path::new("/auth"),
        std::path::Path::new("/auth/c.json"),
        metadata,
    )
    .unwrap();
    let executors = cpa_exec::Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
        codex: Default::default(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let dir = std::env::temp_dir().join(format!("cpa-model-refresh-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    let hash = bcrypt::hash("fake-management-only", 4).unwrap();
    std::fs::write(
        &path,
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\naccess:\n  api-keys: [fake-client]\n",
            dir.join("auth").display()
        ),
    )
    .unwrap();
    let cfg = cpa_core::config::Config::load(&path).unwrap();
    let rt = Arc::new(cpa_server::testing::runtime(cfg, vec![credential], executors));
    let registered = |rt: &cpa_server::Runtime| rt.registry().ids().any(|id| id == "claude-refreshed-1");
    assert!(!registered(&rt));
    // What clients and the dashboard see: `/v1/models` and the Models page's catalog.
    let app = cpa_server::app(
        rt.clone(),
        cpa_server::management::router(cpa_server::management::Management::new(rt.clone(), path)),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let listed = |route: &'static str, key: &'static str, list: &'static str| {
        let proxy = proxy.clone();
        async move {
            let body: serde_json::Value = wreq::Client::new()
                .get(format!("{proxy}{route}"))
                .bearer_auth(key)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            body[list]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["id"] == "claude-refreshed-1")
        }
    };
    let client_models = || listed("/v1/models", "fake-client", "data");
    let dashboard_models = || {
        listed(
            "/v8/management/routing/model-definitions/claude",
            "fake-management-only",
            "models",
        )
    };
    assert!(!client_models().await);
    assert!(!dashboard_models().await);
    let meta_before = pinned().channel("meta").len();
    let generation = catalog_generation();

    // Every source fails: the current catalog stays.
    cpa_server::model_updater::refresh(&urls(&["down", "invalid"]), "test").await;
    assert_eq!(catalog_generation(), generation);

    // A 500 and an invalid catalog are skipped; the third source is applied.
    cpa_server::model_updater::refresh(&urls(&["down", "invalid", "modified"]), "test").await;
    assert_eq!(hits.load(Ordering::SeqCst), 5);
    assert_eq!(catalog_generation(), generation + 1);
    assert!(pinned().lookup("claude-refreshed-1").is_some());
    // Go's refresh callback re-registers the credentials of changed providers.
    assert!(registered(&rt));
    assert!(client_models().await);
    assert!(dashboard_models().await);

    // The same content again changes nothing.
    cpa_server::model_updater::refresh(&urls(&["modified"]), "test").await;
    assert_eq!(catalog_generation(), generation + 1);

    // An empty remote `meta` section keeps the current one (Go tryRefreshModels).
    cpa_server::model_updater::refresh(&urls(&["no-meta"]), "test").await;
    assert_eq!(catalog_generation(), generation + 1, "identical once meta is kept");
    assert_eq!(pinned().channel("meta").len(), meta_before);
}
