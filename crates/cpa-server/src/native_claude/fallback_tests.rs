use super::entitlement_tests::{Fixture, REQUEST, Reply, SUCCESS, Setup};
use super::*;

const MODEL: &str = "claude-opus-test";

fn ok() -> Reply {
    Reply {
        status: 200,
        body: SUCCESS.to_vec(),
        broken: false,
    }
}

/// A usage reading; a used-up window names a reset four hours ahead, as Anthropic's does.
fn quota(utilization: u64) -> Value {
    let resets_at = (chrono::Utc::now() + chrono::Duration::hours(4)).to_rfc3339();
    let window = json!({"utilization":utilization,"resets_at":resets_at});
    json!({"five_hour":window,"seven_day":window})
}

fn accounts(quotas: [Value; 2]) -> Option<Value> {
    let [a, b] = quotas;
    Some(json!([
        {"id":"a","token":"token-a","quota":a},
        {"id":"b","token":"token-b","quota":b}
    ]))
}

async fn blocked(accounts: Option<Value>) -> bool {
    let fixture = Fixture::custom(
        ok(),
        Setup {
            accounts,
            ..Setup::default()
        },
    )
    .await;
    fixture.state.out_of_quota(MODEL).await
}

#[tokio::test]
async fn out_of_quota_requires_every_enabled_account_exhausted() {
    assert!(blocked(accounts([quota(100), quota(100)])).await);
    assert!(!blocked(accounts([quota(100), quota(1)])).await);
}

/// A usage read fails for a revoked sign-in as it does for an outage, so an unreadable
/// reading must not send the conversation elsewhere.
#[tokio::test]
async fn unknown_quota_is_not_out_of_quota() {
    assert!(!blocked(accounts([Value::Null, Value::Null])).await);
    assert!(!blocked(accounts([quota(100), Value::Null])).await);
    let no_reset = json!({"five_hour":{"utilization":100},"seven_day":{"utilization":100}});
    assert!(!blocked(accounts([no_reset.clone(), no_reset])).await);
}

#[tokio::test]
async fn out_of_quota_is_false_without_an_enabled_account() {
    assert!(!blocked(Some(json!([]))).await);
    assert!(
        !blocked(Some(json!([
            {"id":"a","token":"token-a","enabled":false,"quota":quota(100)},
            {"id":"b","token":"token-b","enabled":false,"quota":quota(100)}
        ])))
        .await
    );
    // A disabled account with quota does not hide that the enabled one is out.
    assert!(
        blocked(Some(json!([
            {"id":"a","token":"token-a","quota":quota(100)},
            {"id":"b","token":"token-b","enabled":false,"quota":quota(1)}
        ])))
        .await
    );
}

#[tokio::test]
async fn out_of_quota_is_false_while_allowed_paid_overage_is_available() {
    let mut extra = quota(100);
    extra["extra_usage"] = json!({"is_enabled":true,"monthly_limit":100,"used_credits":1});
    let fixture = Fixture::custom(
        ok(),
        Setup {
            accounts: accounts([extra, quota(100)]),
            ..Setup::default()
        },
    )
    .await;
    assert!(fixture.state.out_of_quota(MODEL).await, "overage is off by default");
    fixture.state.set_overage(true).await.unwrap();
    assert!(!fixture.state.out_of_quota(MODEL).await);
}

const SSE: &str = concat!(
    r#"data: {"id":"c","model":"local-test","choices":[{"index":0,"delta":{"reasoning_content":"pondering"}}]}"#,
    "\n\n",
    r#"data: {"id":"c","model":"local-test","choices":[{"index":0,"delta":{"content":"visible"}}]}"#,
    "\n\n",
    r#"data: {"id":"c","model":"local-test","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#,
    "\n\ndata: [DONE]\n\n"
);
const JSON: &str = r#"{"id":"c","model":"local-test","choices":[{"index":0,"message":{"role":"assistant","reasoning_content":"pondering","content":"visible"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#;
const STREAMING: &[u8] = br#"{ "model" : "claude-opus-test", "stream":true, "metadata":{"session_id":"synthetic-session"}, "messages": [{"role":"user","content":"hi"}] }"#;
const EXHAUSTING: &[u8] = br#"{ "model" : "claude-opus-test", "metadata":{"session_id":"synthetic-session"}, "messages": [{"role":"user","content":"MODE_EXHAUSTED"}] }"#;

type Requests = Arc<Mutex<Vec<(HeaderMap, Vec<u8>)>>>;

/// A loopback OpenAI-compatible server that records what it receives.
struct Local {
    port: u16,
    seen: Requests,
    task: JoinHandle<()>,
}
impl Drop for Local {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Local {
    async fn start(status: u16) -> Self {
        use axum::response::IntoResponse;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Requests::default();
        let captured = seen.clone();
        let app = axum::Router::new().fallback(move |headers: HeaderMap, body: axum::body::Bytes| {
            let seen = captured.clone();
            async move {
                seen.lock().unwrap().push((headers, body.to_vec()));
                if status != 200 {
                    let code = StatusCode::from_u16(status).unwrap();
                    return (code, r#"{"error":{"message":"synthetic"}}"#).into_response();
                }
                let streaming = serde_json::from_slice::<Value>(&body).is_ok_and(|v| v["stream"] == true);
                if streaming {
                    ([("content-type", "text/event-stream")], SSE).into_response()
                } else {
                    ([("content-type", "application/json")], JSON).into_response()
                }
            }
        });
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { port, seen, task }
    }
    fn yaml(&self) -> String {
        format!(
            "api-keys:\n  openai-compatibility:\n    - name: local\n      base-url: http://127.0.0.1:{}/v1\n      headers:\n        X-Seen-Authorization: $Authorization\n        X-Seen-Key: $X-Api-Key\n        X-Seen-Cookie: $Cookie\n        X-Seen-Agent: $User-Agent\n      keys: []\n      models:\n        - name: local-test\n",
            self.port
        )
    }
    fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

fn exhausted() -> Option<Value> {
    accounts([quota(100), quota(100)])
}

/// One account with quota: the mock answers only account `a` with the scripted reply.
fn only_a() -> Option<Value> {
    Some(json!([{"id":"a","token":"token-a","quota":quota(1)}]))
}

fn mapped(to: &str) -> Value {
    json!({"quotaFallback": {"claude-opus-": to}})
}

async fn fixture(reply: Reply, accounts: Option<Value>, native: Value, yaml: String) -> Fixture {
    Fixture::custom(reply, Setup { accounts, native, yaml }).await
}

async fn post(fixture: &Fixture, body: &'static [u8]) -> wreq::Response {
    wreq::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .retry(wreq::retry::Policy::never())
        .build()
        .unwrap()
        .post(format!("{}/v1/messages", fixture.url))
        .header("user-agent", "synthetic-agent/1")
        .header("authorization", "Bearer synthetic-native-login")
        .header("x-api-key", "synthetic-client-key")
        .header("cookie", "synthetic=cookie")
        .body(body)
        .send()
        .await
        .unwrap()
}

fn events(fixture: &Fixture, kind: &str) -> Vec<Value> {
    fixture.state.rt.native_events.snapshot()["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == kind)
        .cloned()
        .collect()
}

#[tokio::test]
async fn exhausted_quota_is_served_by_the_mapped_model() {
    let local = Local::start(200).await;
    let fixture = fixture(ok(), exhausted(), mapped("local-test"), local.yaml()).await;
    let response = post(&fixture, STREAMING).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let body = String::from_utf8(response.bytes().await.unwrap().to_vec()).unwrap();
    assert!(body.contains(r#""text_delta""#) && body.contains("visible"), "{body}");
    assert!(!body.contains("thinking"), "{body}");
    assert!(body.contains("message_stop"), "{body}");

    assert!(fixture.seen.lock().unwrap().is_empty(), "Claude received nothing");
    let seen = local.seen.lock().unwrap();
    let [(headers, sent)] = seen.as_slice() else {
        panic!("one fallback request, saw {}", seen.len())
    };
    // The entry's header templates copy client headers: only the harmless one arrives.
    assert_eq!(headers["x-seen-agent"], "synthetic-agent/1");
    for name in [
        "authorization",
        "x-api-key",
        "cookie",
        "x-seen-authorization",
        "x-seen-key",
        "x-seen-cookie",
    ] {
        assert!(!headers.contains_key(name), "{name} reached the fallback provider");
    }
    let sent: Value = serde_json::from_slice(sent).unwrap();
    assert_eq!(sent["model"], "local-test");

    let served = events(&fixture, "fallback");
    assert_eq!(served.len(), 1, "{served:?}");
    assert_eq!(served[0]["outcome"], "fallback_served");
    assert_eq!(served[0]["model"], "claude-opus-test");
    assert_eq!(served[0]["fallbackModel"], "local-test");
    assert!(events(&fixture, "rejected").is_empty());
    // The fallback binds no session and cools no account.
    assert!(!fixture.dir.join("routing.json").exists());
}

#[tokio::test]
async fn a_buffered_request_falls_back_as_json() {
    let local = Local::start(200).await;
    let fixture = fixture(ok(), exhausted(), mapped("local-test"), local.yaml()).await;
    let response = post(&fixture, REQUEST).await;
    assert_eq!(response.status(), 200);
    let message: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(message["content"], json!([{"type":"text","text":"visible"}]));
    assert_eq!(local.count(), 1);
}

/// The refusal a fixture without any mapping gives, as (status, retry-after, body).
async fn refusal(fixture: &Fixture, body: &'static [u8]) -> (u16, Option<String>, Vec<u8>) {
    let response = post(fixture, body).await;
    let retry = response
        .headers()
        .get("retry-after")
        .map(|v| v.to_str().unwrap().to_owned());
    (
        response.status().as_u16(),
        retry,
        response.bytes().await.unwrap().to_vec(),
    )
}

#[tokio::test]
async fn no_mapping_and_an_unmapped_model_keep_the_429() {
    let local = Local::start(200).await;
    let baseline = fixture(ok(), exhausted(), Value::Null, String::new()).await;
    let want = refusal(&baseline, REQUEST).await;
    assert_eq!(want.0, 429);
    assert!(want.1.is_some(), "the refusal names when to retry");
    for native in [
        Value::Null,
        json!({"quotaFallback": {}}),
        json!({"quotaFallback": {"claude-haiku-": "local-test"}}),
    ] {
        let fixture = fixture(ok(), exhausted(), native.clone(), local.yaml()).await;
        assert_eq!(refusal(&fixture, REQUEST).await, want, "{native}");
        assert!(events(&fixture, "fallback").is_empty());
        assert!(events(&fixture, "rejected")[0].get("fallbackModel").is_none());
    }
    assert_eq!(local.count(), 0);
}

#[tokio::test]
async fn an_account_with_quota_resting_after_overload_blocks_the_fallback() {
    let local = Local::start(200).await;
    let overloaded = Reply {
        status: 529,
        body: br#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#.to_vec(),
        broken: false,
    };
    let fixture = fixture(overloaded, only_a(), mapped("local-test"), local.yaml()).await;
    assert_eq!(post(&fixture, REQUEST).await.status(), 503);
    // The account is resting, not out of quota: the refusal stays Claude's.
    assert_eq!(post(&fixture, REQUEST).await.status(), 429);
    assert_eq!(local.count(), 0);
    assert!(events(&fixture, "fallback").is_empty());
}

#[tokio::test]
async fn an_exhausting_429_falls_back_and_a_burst_429_does_not() {
    let limited = || Reply {
        status: 429,
        body: br#"{"type":"error","error":{"type":"rate_limit_error","message":"Limited"}}"#.to_vec(),
        broken: false,
    };
    let local = Local::start(200).await;
    let exhausting = fixture(limited(), only_a(), mapped("local-test"), local.yaml()).await;
    let response = post(&exhausting, EXHAUSTING).await;
    assert_eq!(response.status(), 200);
    assert_eq!(exhausting.seen.lock().unwrap().len(), 1, "Claude answered first");
    assert_eq!(local.count(), 1);
    assert_eq!(events(&exhausting, "fallback")[0]["attempts"], 1);

    let burst = fixture(limited(), only_a(), mapped("local-test"), local.yaml()).await;
    assert_eq!(post(&burst, REQUEST).await.status(), 429);
    assert_eq!(local.count(), 1, "a 429 that leaves quota available stays Claude's");
}

#[tokio::test]
async fn a_lost_response_is_not_sent_to_the_fallback() {
    let local = Local::start(200).await;
    // The account has quota and its proxy accepts no connection.
    let unreachable = Some(json!([{"id":"a","token":"token-a","quota":quota(1),"proxyUrl":"http://127.0.0.1:9"}]));
    let fixture = fixture(ok(), unreachable, mapped("local-test"), local.yaml()).await;
    assert_eq!(post(&fixture, REQUEST).await.status(), 422);
    assert_eq!(local.count(), 0);
}

#[tokio::test]
async fn a_failed_fallback_returns_the_original_429() {
    let baseline = fixture(ok(), exhausted(), Value::Null, String::new()).await;
    let want = refusal(&baseline, REQUEST).await;
    assert!(want.1.is_some(), "the refusal names when to retry");
    let local = Local::start(500).await;
    let fixture = fixture(ok(), exhausted(), mapped("local-test"), local.yaml()).await;
    assert_eq!(refusal(&fixture, REQUEST).await, want);
    assert_eq!(local.count(), 1, "the fallback was tried once");
    assert!(events(&fixture, "fallback").is_empty());
    let rejected = events(&fixture, "rejected");
    assert_eq!(rejected[0]["fallbackModel"], "local-test");
    assert_eq!(rejected[0]["fallbackFailure"], "no_output");
}

#[tokio::test]
async fn an_unknown_fallback_model_returns_the_original_429() {
    let local = Local::start(200).await;
    let fixture = fixture(ok(), exhausted(), mapped("no-such-model"), local.yaml()).await;
    assert_eq!(post(&fixture, REQUEST).await.status(), 429);
    assert_eq!(local.count(), 0);
    assert_eq!(events(&fixture, "rejected")[0]["fallbackFailure"], "unknown_model");
}

#[tokio::test]
async fn unknown_quota_keeps_the_429() {
    let local = Local::start(200).await;
    let unknown = accounts([Value::Null, Value::Null]);
    let fixture = fixture(ok(), unknown, mapped("local-test"), local.yaml()).await;
    assert_eq!(post(&fixture, REQUEST).await.status(), 429);
    assert_eq!(local.count(), 0);
    assert!(events(&fixture, "rejected")[0].get("fallbackModel").is_none());
}

/// The refusal at the attempt limit, after the last account answered an exhausting 429.
#[tokio::test]
async fn a_429_at_the_attempt_limit_falls_back() {
    let local = Local::start(200).await;
    let limited = Reply {
        status: 429,
        body: br#"{"type":"error","error":{"type":"rate_limit_error","message":"Limited"}}"#.to_vec(),
        broken: false,
    };
    let mut native = mapped("local-test");
    native["maxAttempts"] = json!(1);
    let fixture = fixture(limited, only_a(), native, local.yaml()).await;
    assert_eq!(post(&fixture, EXHAUSTING).await.status(), 200);
    assert_eq!(fixture.seen.lock().unwrap().len(), 1);
    assert_eq!(local.count(), 1);
}

#[tokio::test]
async fn a_gemini_key_serves_the_fallback() {
    use axum::response::IntoResponse;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Requests::default();
    let captured = seen.clone();
    let app = axum::Router::new().fallback(move |headers: HeaderMap, body: axum::body::Bytes| {
        captured.lock().unwrap().push((headers, body.to_vec()));
        async {
            (
                [("content-type", "application/json")],
                r#"{"responseId":"r","modelVersion":"gemini-test","candidates":[{"content":{"role":"model","parts":[{"text":"pondering","thought":true,"thoughtSignature":"c2ln"},{"text":"visible"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1}}"#,
            )
                .into_response()
        }
    });
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let yaml = format!(
        "api-keys:\n  gemini:\n    - base-url: http://127.0.0.1:{port}\n      keys: [{{api-key: synthetic-key}}]\n      models:\n        - name: gemini-test\n"
    );
    let fixture = fixture(ok(), exhausted(), mapped("gemini-test"), yaml).await;
    let response = post(&fixture, REQUEST).await;
    assert_eq!(response.status(), 200);
    let message: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(message["content"], json!([{"type":"text","text":"visible"}]));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0["x-goog-api-key"], "synthetic-key");
    assert!(!seen[0].0.contains_key("authorization"));
    server.abort();
}

/// A Claude API key in the translating proxy would serve the request outside native
/// admission.
#[tokio::test]
async fn a_claude_fallback_target_is_refused() {
    let local = Local::start(200).await;
    let yaml = format!(
        "api-keys:\n  claude:\n    - base-url: http://127.0.0.1:{}\n      keys: [{{api-key: synthetic-key}}]\n      models:\n        - name: claude-other-test\n",
        local.port
    );
    let fixture = fixture(ok(), exhausted(), mapped("claude-other-test"), yaml).await;
    assert_eq!(post(&fixture, REQUEST).await.status(), 429);
    assert_eq!(local.count(), 0);
    assert_eq!(
        events(&fixture, "rejected")[0]["fallbackFailure"],
        "provider_not_allowed"
    );
}

#[tokio::test]
async fn paid_overage_is_used_before_the_fallback() {
    let local = Local::start(200).await;
    let mut extra = quota(100);
    extra["extra_usage"] = json!({"is_enabled":true,"monthly_limit":100,"used_credits":1});
    let accounts = Some(json!([{"id":"a","token":"token-a","quota":extra}]));
    let fixture = fixture(ok(), accounts, mapped("local-test"), local.yaml()).await;
    fixture.state.set_overage(true).await.unwrap();
    assert_eq!(post(&fixture, REQUEST).await.bytes().await.unwrap(), SUCCESS);
    assert_eq!(fixture.seen.lock().unwrap().len(), 1);
    assert_eq!(local.count(), 0);
}

#[test]
fn the_call_body_differs_from_the_client_body_only_in_model() {
    let client = br#"{ "stream" : true,"model" :  "claude-opus-test" , "messages":[ {"role":"user","content":"claude-opus-test \\n\\\"q\\\""} ],"x":1.0 }"#;
    let call = fallback::body(client, "local-test").unwrap();
    assert_eq!(
        call.as_ref(),
        br#"{ "stream" : true,"model" :  "local-test" , "messages":[ {"role":"user","content":"claude-opus-test \\n\\\"q\\\""} ],"x":1.0 }"#
    );
}
