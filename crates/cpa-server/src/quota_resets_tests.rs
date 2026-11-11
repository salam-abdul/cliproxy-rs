use super::*;
use std::future::IntoFuture;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{HeaderMap, Request, Uri},
    routing::{get, post},
};
use cpa_core::{
    config::{Config, credentials::auth_index},
    credential::Credential,
};
use tower::ServiceExt;

const ORG: &str = "0f8b6c1e-2d3a-4b5c-9d7e-112233445566";

fn at(rfc: &str) -> SystemTime {
    crate::quota_time::rfc3339(rfc).unwrap()
}

#[test]
fn claude_grants_list_one_row_per_reset_and_mark_what_cannot_be_used() {
    let block = json!({
        "eligible": true,
        "next_grant_id": "g-weekly",
        "grants": [
            {"id": "g-weekly", "resets_left": 2, "ends_at": "2033-01-10T00:00:00Z", "clears": ["seven_day"],
             "usable_now": true},
            {"id": "g-session", "resets_left": 1, "ends_at": "2033-01-09T00:00:00Z", "clears": ["five_hour"],
             "usable_now": true},
            {"id": "g-paused", "resets_left": 1, "ends_at": 1988236800, "type": "full", "usable_now": true, "paused": true},
            {"id": "g-expired", "resets_left": 1, "ends_at": "2032-12-31T00:00:00Z", "usable_now": true},
            {"id": "g-spent", "resets_left": 0, "ends_at": "2033-01-10T00:00:00Z", "usable_now": true},
            {"id": "g-fable", "resets_left": 1, "clears": ["five_hour"], "gives_back_weekly": true}
        ]
    });
    let status = claude_status(&block, at("2033-01-01T00:00:00Z"));
    assert!(status.eligible);
    let rows: Vec<_> = status
        .resets
        .iter()
        .map(|r| (r.key.as_str(), r.id.as_str(), r.kind, r.usable, r.blocked))
        .collect();
    assert_eq!(
        rows,
        [
            ("g-weekly:0", "g-weekly", "weekly", true, None),
            ("g-weekly:1", "g-weekly", "weekly", true, None),
            (
                "g-session:0",
                "g-session",
                "session",
                false,
                Some("Anthropic wants another reset used first")
            ),
            ("g-paused:0", "g-paused", "full", false, Some("Paused by Anthropic")),
            (
                "g-fable:0",
                "g-fable",
                "fable",
                false,
                Some("Anthropic wants another reset used first")
            ),
        ]
    );
    assert_eq!(status.resets[0].expires_at.as_deref(), Some("2033-01-10T00:00:00Z"));
    assert_eq!(status.resets[3].expires_at.as_deref(), Some("2033-01-02T00:00:00Z"));
    assert_eq!(
        status.resets[4].expires_at, None,
        "a grant without an end date is listed"
    );
    assert_eq!(
        status.resets[0].detail,
        "Refills your weekly limit now. This cannot be undone."
    );
    assert_eq!(
        status.resets[4].detail,
        "Refills your 5-hour limit now and gives back what Fable used of your weekly limit. This cannot be undone."
    );
}

#[test]
fn claude_ineligible_accounts_list_nothing_and_say_why() {
    let now = at("2033-01-01T00:00:00Z");
    for (block, reason) in [
        (
            json!({"eligible": false, "ineligible_reason": "cli_version", "grants": [{"id": "g", "resets_left": 1}]}),
            "Anthropic requires a newer Claude Code identity for resets.",
        ),
        (Value::Null, "Anthropic reported no reset information for this account."),
    ] {
        let status = claude_status(&block, now);
        assert!(!status.eligible);
        assert!(status.resets.is_empty());
        assert_eq!(status.reason.as_deref(), Some(reason));
    }
}

#[test]
fn codex_lists_only_available_unexpired_credits() {
    let body = json!({"available_count": 2, "credits": [
        {"id": "cr_1", "status": "available", "expires_at": "2033-02-01T00:00:00Z", "reset_type": "codex_rate_limits"},
        {"id": "cr_2", "expires_at": 1991001600},
        {"id": "cr_3", "status": "redeemed", "expires_at": "2033-02-01T00:00:00Z"},
        {"id": "cr_4", "status": "available", "expires_at": "2032-01-01T00:00:00Z"},
        {"id": "bad id", "status": "available"}
    ]});
    let status = codex_status(&body, at("2033-01-01T00:00:00Z"));
    let ids: Vec<_> = status
        .resets
        .iter()
        .map(|r| (r.id.as_str(), r.kind, r.usable))
        .collect();
    assert_eq!(ids, [("cr_1", "full", true), ("cr_2", "full", true)]);
    assert_eq!(status.resets[1].expires_at.as_deref(), Some("2033-02-03T00:00:00Z"));
}

#[test]
fn claim_answers_that_do_not_prove_the_outcome_are_unconfirmed() {
    let claude = |status, body: &str| claude_claim(status, Some(body.as_bytes())).outcome;
    assert_eq!(claude(200, r#"{"result":"reset","resets_left":0}"#), Outcome::Applied);
    assert_eq!(claude(200, r#"{"result":"already_used"}"#), Outcome::Applied);
    assert_eq!(
        claude(200, r#"{"result":"unavailable","reason":"not_limited"}"#),
        Outcome::NothingToReset
    );
    assert_eq!(
        claude(200, r#"{"result":"unavailable","reason":"no_grant"}"#),
        Outcome::Gone
    );
    assert_eq!(claude(200, r#"{"result":"not_next_grant"}"#), Outcome::NotNext);
    assert_eq!(claude(200, r#"{"result":"reset_unconfirmed"}"#), Outcome::Unconfirmed);
    assert_eq!(claude(200, r#"{"result":"something_new"}"#), Outcome::Unconfirmed);
    assert_eq!(claude(200, "not json"), Outcome::Unconfirmed);
    assert_eq!(claude(502, "{}"), Outcome::Unconfirmed);
    assert_eq!(claude(408, "{}"), Outcome::Unconfirmed);
    assert_eq!(
        claude_claim(200, None).outcome,
        Outcome::Unconfirmed,
        "a cut-off body proves nothing"
    );
    assert_eq!(claude(401, "{}"), Outcome::Failed);
    assert_eq!(claude(400, "{}"), Outcome::Failed);
    let scope = claude_claim(403, Some(br#"{"error":"OAuth token does not meet scope requirement"}"#));
    assert_eq!(
        scope.message,
        "This sign-in cannot use resets. Sign in again through the dashboard."
    );
    let cooldown = claude_claim(
        200,
        Some(br#"{"result":"unavailable","reason":"cooldown","cooldown_until":"2033-01-01T05:00:00Z"}"#),
    );
    assert_eq!(
        (cooldown.outcome, cooldown.until.as_deref()),
        (Outcome::Unavailable, Some("2033-01-01T05:00:00Z"))
    );

    let codex = |status, body: &str| codex_claim(status, Some(body.as_bytes())).outcome;
    assert_eq!(codex(200, r#"{"code":"reset"}"#), Outcome::Applied);
    assert_eq!(codex(200, r#"{"code":"already_redeemed"}"#), Outcome::Applied);
    assert_eq!(codex(200, r#"{"code":"nothing_to_reset"}"#), Outcome::NothingToReset);
    assert_eq!(codex(200, r#"{"code":"no_credit"}"#), Outcome::Gone);
    assert_eq!(codex(200, r#"{"code":"later"}"#), Outcome::Unconfirmed);
    assert_eq!(codex(503, ""), Outcome::Unconfirmed);
    assert_eq!(codex(403, ""), Outcome::Failed);
}

#[derive(Default)]
struct Upstream {
    reads: AtomicUsize,
    claims: Mutex<Vec<(String, String, Value)>>,
    claim_status: Mutex<u16>,
}

/// A loopback upstream reached through each credential's own proxy setting, so the
/// request line carries the absolute URL the server built.
async fn upstream(state: Arc<Upstream>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let read = {
        let state = state.clone();
        move |uri: Uri, headers: HeaderMap| {
            let state = state.clone();
            async move {
                state.reads.fetch_add(1, Ordering::SeqCst);
                assert_eq!(headers["authorization"], "Bearer synthetic-claude");
                assert_eq!(uri.query(), Some("cedar_ember=1&skip_spend=1"));
                assert!(headers["user-agent"].to_str().unwrap().starts_with("claude-cli/"));
                axum::Json(json!({"five_hour": {"utilization": 100}, "cedar_ember": {
                    "eligible": true, "next_grant_id": "g-1",
                    "grants": [{"id": "g-1", "resets_left": 1, "ends_at": "2099-01-01T00:00:00Z",
                                "clears": ["five_hour"], "usable_now": true}]}}))
            }
        }
    };
    let claim = |provider: &'static str| {
        let state = state.clone();
        move |uri: Uri, headers: HeaderMap, body: axum::body::Bytes| {
            let state = state.clone();
            async move {
                let auth = headers["authorization"].to_str().unwrap().to_owned();
                let body: Value = serde_json::from_slice(&body).unwrap();
                state.claims.lock().unwrap().push((uri.path().to_owned(), auth, body));
                let status = *state.claim_status.lock().unwrap();
                let reply = if provider == "claude" {
                    json!({"result": "reset", "resets_left": 0})
                } else {
                    json!({"code": "reset"})
                };
                (axum::http::StatusCode::from_u16(status).unwrap(), axum::Json(reply))
            }
        }
    };
    let codex_read = |headers: HeaderMap| async move {
        assert_eq!(headers["chatgpt-account-id"], "acct-synthetic");
        assert_eq!(headers["openai-beta"], "codex-1");
        axum::Json(json!({"available_count": 1, "credits": [
            {"id": "cr_1", "status": "available", "expires_at": "2099-01-01T00:00:00Z"}]}))
    };
    let router = Router::new()
        .route("/api/oauth/usage", get(read))
        .route(
            &format!("/api/organizations/{ORG}/reset_rate_limits"),
            post(claim("claude")),
        )
        .route("/backend-api/wham/rate-limit-reset-credits", get(codex_read))
        .route(
            "/backend-api/wham/rate-limit-reset-credits/consume",
            post(claim("codex")),
        );
    tokio::spawn(axum::serve(listener, router).into_future());
    address
}

struct Harness {
    router: Router,
    rt: Arc<crate::runtime::Runtime>,
    indices: Vec<String>,
    upstream: Arc<Upstream>,
}

async fn harness() -> Harness {
    let dir = std::env::temp_dir().join(format!("resets-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let upstream_state = Arc::new(Upstream::default());
    *upstream_state.claim_status.lock().unwrap() = 200;
    let proxy = upstream(upstream_state.clone()).await;
    let hash = bcrypt::hash("synthetic-management", 4).unwrap();
    let cfg = Config::parse(&format!(
        "oauth:\n  auth-dir: {}\nmanagement:\n  secret-key: '{hash}'\n",
        dir.display()
    ))
    .unwrap();
    let files = [
        json!({"type": "claude", "access_token": "synthetic-claude", "organization_uuid": ORG}),
        json!({"type": "claude", "access_token": "synthetic-claude", "organization_uuid": "../../admin"}),
        json!({"type": "claude", "access_token": "synthetic-claude", "organization_uuid": ORG, "is_setup_token": true}),
        json!({"type": "codex", "access_token": "synthetic-codex", "account_id": "acct-synthetic"}),
        json!({"type": "claude", "access_token": "synthetic-claude", "organization_uuid": ORG,
               "proxy_url": "ftp://unusable"}),
    ];
    let credentials: Vec<_> = files
        .into_iter()
        .enumerate()
        .map(|(i, mut metadata)| {
            if metadata.get("proxy_url").is_none() {
                metadata["proxy_url"] = json!(proxy);
            }
            Credential::from_file(
                &dir,
                &dir.join(format!("account-{i}.json")),
                metadata.as_object().unwrap().clone(),
            )
            .unwrap()
        })
        .collect();
    let indices = credentials.iter().map(auth_index).collect();
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
    rt.quota_resets = QuotaResets::with_base("http://127.0.0.1:1");
    let rt = Arc::new(rt);
    let router = crate::management::router(crate::management::Management::new(rt.clone(), dir.join("config.yaml")));
    Harness {
        router,
        rt,
        indices,
        upstream: upstream_state,
    }
}

impl Harness {
    async fn call(&self, method: &str, uri: String, body: Option<Value>) -> (u16, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", "Bearer synthetic-management")
            .header("content-type", "application/json")
            .extension(ConnectInfo("127.0.0.1:40000".parse::<std::net::SocketAddr>().unwrap()))
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn read(&self, account: usize) -> (u16, Value) {
        let index = &self.indices[account];
        self.call("GET", format!("/v8/management/quota/resets?auth_index={index}"), None)
            .await
    }

    async fn claim(&self, account: usize, reset: &str, request: &str) -> (u16, Value) {
        let body = json!({"auth_index": self.indices[account], "reset_id": reset, "request_id": request});
        self.call("POST", "/v8/management/quota/resets".into(), Some(body))
            .await
    }
}

#[tokio::test]
async fn reads_are_cached_per_account_and_a_claim_spends_the_chosen_grant_once() {
    let h = harness().await;
    let (status, body) = h.read(0).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["resets"][0]["id"], "g-1");
    assert_eq!(body["resets"][0]["kind"], "session");
    assert_eq!(body["resets"][0]["usable"], true);
    h.read(0).await;
    assert_eq!(
        h.upstream.reads.load(Ordering::SeqCst),
        1,
        "the second read is served from the cache"
    );

    let (status, body) = h.claim(0, "g-1", "req-1").await;
    assert_eq!((status, body["outcome"].as_str()), (200, Some("applied")), "{body}");
    let claims = h.upstream.claims.lock().unwrap().clone();
    assert_eq!(
        claims,
        [(
            format!("/api/organizations/{ORG}/reset_rate_limits"),
            "Bearer synthetic-claude".to_owned(),
            json!({"program": "cedar_ember", "grant_id": "g-1", "request_id": "req-1"})
        )]
    );
    h.read(0).await;
    assert_eq!(
        h.upstream.reads.load(Ordering::SeqCst),
        2,
        "a claim drops the cached list"
    );
}

#[tokio::test]
async fn a_server_error_on_a_claim_is_unconfirmed_and_never_resent() {
    let h = harness().await;
    *h.upstream.claim_status.lock().unwrap() = 503;
    let (status, body) = h.claim(0, "g-1", "req-1").await;
    assert_eq!((status, body["outcome"].as_str()), (200, Some("unconfirmed")), "{body}");
    assert_eq!(h.upstream.claims.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn codex_credits_are_read_and_consumed_with_the_account_header() {
    let h = harness().await;
    let (status, body) = h.read(3).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["resets"][0]["id"], "cr_1");
    let (status, body) = h.claim(3, "cr_1", "req-2").await;
    assert_eq!((status, body["outcome"].as_str()), (200, Some("applied")), "{body}");
    let claims = h.upstream.claims.lock().unwrap().clone();
    assert_eq!(claims[0].0, "/backend-api/wham/rate-limit-reset-credits/consume");
    assert_eq!(claims[0].2, json!({"credit_id": "cr_1", "redeem_request_id": "req-2"}));
}

#[tokio::test]
async fn claims_refuse_before_sending_when_the_account_or_request_is_unsafe() {
    let h = harness().await;
    for (account, reset, request, status, message) in [
        (
            1,
            "g-1",
            "req-1",
            400,
            "This Claude account has no organization UUID. Sign in again through the dashboard.",
        ),
        (
            2,
            "g-1",
            "req-1",
            400,
            "Setup tokens cannot use resets. Connect Claude through the dashboard sign-in.",
        ),
        (4, "g-1", "req-1", 502, "The account's proxy cannot be used"),
        (0, "G/../x", "req-1", 400, "reset_id is not a Claude grant id"),
        (
            0,
            "g-1",
            "req 1",
            400,
            "request_id must be 1 to 64 letters, digits, '-' or '_'",
        ),
    ] {
        let (got, body) = h.claim(account, reset, request).await;
        assert_eq!(
            (got, body["error"].as_str()),
            (status, Some(message)),
            "account {account}"
        );
    }
    assert!(
        h.upstream.claims.lock().unwrap().is_empty(),
        "no refused claim reached the provider"
    );
}

#[tokio::test]
async fn one_claim_runs_per_account_at_a_time() {
    let h = harness().await;
    let resets = &h.rt.quota_resets;
    resets.claiming.lock().unwrap().insert(
        h.rt.store()
            .snapshot()
            .into_iter()
            .find(|c| auth_index(c) == h.indices[0])
            .unwrap()
            .id
            .clone(),
    );
    let (status, body) = h.claim(0, "g-1", "req-1").await;
    assert_eq!(
        (status, body["error"].as_str()),
        (409, Some("A reset is already being applied to this account"))
    );
    assert!(h.upstream.claims.lock().unwrap().is_empty());
}
