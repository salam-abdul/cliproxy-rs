use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;

use crate::request_logging::history::file::read_rows;
use crate::request_logging::history::file::tests::TempDir;
use crate::request_logging::history::row::{Outcome, Row};

const JSON_REPLY: &str = r#"{"id":"msg_1","type":"message","model":"claude-sonnet-4-6","content":[],"usage":{"input_tokens":11,"output_tokens":7,"cache_read_input_tokens":5,"cache_creation_input_tokens":3}}"#;
const SSE: &str = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";

#[derive(Default)]
struct Upstream {
    bodies: Mutex<Vec<Bytes>>,
}

async fn upstream(State(seen): State<Arc<Upstream>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default()
    };
    let key = [
        header("x-api-key"),
        header("authorization").trim_start_matches("Bearer ").to_owned(),
    ]
    .into_iter()
    .find(|k| !k.is_empty())
    .unwrap_or_default();
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    seen.bodies.lock().unwrap().push(body.clone());
    let has = |marker: &[u8]| cpa_common::gostr::contains(&body, marker);
    if key == "sk-synthetic-limited" {
        let err = r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#;
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("content-type", "application/json")],
            err,
        )
            .into_response();
    }
    if has(b"MODE_STALL") {
        use futures_util::StreamExt;
        let chunks = futures_util::stream::once(async {
            Ok::<_, std::io::Error>(Bytes::from_static(b"event: ping\ndata: {\"type\":\"ping\"}\n\n"))
        })
        .chain(futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(Bytes::from_static(b""))
        }));
        return ([("content-type", "text/event-stream")], Body::from_stream(chunks)).into_response();
    }
    if has(br#""stream":true"#) {
        (
            [("content-type", "text/event-stream"), ("request-id", "req_stream")],
            SSE,
        )
            .into_response()
    } else {
        (
            [("content-type", "application/json"), ("request-id", "req_json")],
            JSON_REPLY,
        )
            .into_response()
    }
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

struct Proxy {
    rt: Arc<crate::Runtime>,
    url: String,
    seen: Arc<Upstream>,
    _auth: TempDir,
}

/// A proxy in front of Claude API keys on a mock upstream. `history` is the
/// `observability.history.dir`, or `None` for history off.
async fn proxy(history: Option<&Path>, keys: &[&str], retry_credentials: usize) -> Proxy {
    let seen = Arc::new(Upstream::default());
    let upstream_url = serve(axum::Router::new().fallback(upstream).with_state(seen.clone())).await;
    let auth = TempDir::new();
    let mut yaml = String::from("claude-api-key:\n");
    for key in keys {
        yaml.push_str(&format!("  - api-key: {key}\n    base-url: {upstream_url}\n"));
    }
    if let Some(dir) = history {
        yaml.push_str(&format!(
            "observability:\n  history:\n    enable: true\n    dir: '{}'\n",
            dir.display()
        ));
    }
    let mut config = cpa_core::config::Config::parse(&yaml).unwrap();
    config.api_keys = vec!["client-key-1".into()];
    config.auth_dir = auth.0.clone();
    let creds = cpa_core::config::credentials::from_config(&config)
        .into_iter()
        .map(crate::testing::local)
        .collect();
    let executors = Executors {
        claude: ClaudeExecutor::new(&upstream_url).unwrap(),
        codex: Default::default(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(crate::testing::runtime(config, creds, executors));
    rt.publish_policy(crate::scheduler::Policy {
        max_retry_credentials: retry_credentials,
        ..Default::default()
    });
    crate::install_registry(&rt);
    let url = serve(crate::router(rt.clone())).await;
    Proxy {
        rt,
        url: format!("{url}/v1/messages"),
        seen,
        _auth: auth,
    }
}

/// One test at a time owns the process-wide registry overlay.
static REGISTRY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn rows_in(dir: &Path) -> Vec<Row> {
    let mut rows = Vec::new();
    let Ok(partitions) = std::fs::read_dir(dir) else {
        return rows;
    };
    for partition in partitions.flatten() {
        if !partition.path().is_dir() || partition.file_name() == ".deletes" {
            continue;
        }
        for file in std::fs::read_dir(partition.path()).unwrap().flatten() {
            if file.path().extension().is_some_and(|e| e == "parquet") {
                rows.extend(read_rows(&file.path()).unwrap());
            }
        }
    }
    rows.sort_by_key(|r| (r.started_at_us, r.send));
    rows
}

/// Flushes until `count` rows are on disk; a stream's row is published as it ends.
async fn rows(proxy: &Proxy, dir: &Path, count: usize) -> Vec<Row> {
    for _ in 0..200 {
        let history = proxy.rt.clone();
        tokio::task::spawn_blocking(move || history.history().flush())
            .await
            .unwrap();
        let rows = rows_in(dir);
        if rows.len() >= count {
            assert_eq!(rows.len(), count, "{rows:#?}");
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected {count} row(s), found {:#?}", rows_in(dir));
}

async fn post(url: &str, body: &'static str) -> (u16, String) {
    let res = wreq::Client::new()
        .post(url)
        .header("x-api-key", "client-key-1")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = res.status().as_u16();
    (status, res.text().await.unwrap())
}

const BUFFERED: &str = r#"{"model":"claude-sonnet-4-6","max_tokens":5,"messages":[{"role":"user","content":"hi"}]}"#;
const STREAMED: &str =
    r#"{"model":"claude-sonnet-4-6","max_tokens":5,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#;

#[tokio::test]
async fn a_buffered_claude_call_is_recorded() {
    let _registry = REGISTRY.lock().await;
    let dir = TempDir::new();
    let proxy = proxy(Some(&dir.0), &["sk-synthetic-123"], 1).await;
    let (status, _) = post(&proxy.url, BUFFERED).await;
    assert_eq!(status, 200);
    let rows = rows(&proxy, &dir.0, 1).await;
    let row = &rows[0];
    let sent = proxy.seen.bodies.lock().unwrap()[0].clone();
    assert_eq!(row.request_body.as_deref().map(str::as_bytes), Some(&sent[..]));
    assert_eq!(row.response_body.as_deref(), Some(JSON_REPLY));
    assert!(!row.exchange_id.is_empty());
    assert_eq!(row.send, 0);
    assert_eq!(row.outcome, Outcome::Complete);
    assert_eq!(row.status, Some(200));
    assert_eq!(row.provider.as_deref(), Some("claude"));
    assert_eq!(row.model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(row.endpoint.as_deref(), Some("POST /v1/messages"));
    assert_eq!(row.upstream_request_id.as_deref(), Some("req_json"));
    assert!(!row.stream);
    assert_eq!(row.input_tokens, Some(11));
    assert_eq!(row.output_tokens, Some(7));
    assert_eq!(row.cache_read_tokens, Some(5));
    assert_eq!(row.cache_write_tokens, Some(3));
    assert!(row.account.as_deref().is_some_and(|a| !a.is_empty()));
}

#[tokio::test]
async fn a_streamed_claude_call_is_recorded_as_lines() {
    let _registry = REGISTRY.lock().await;
    let dir = TempDir::new();
    let proxy = proxy(Some(&dir.0), &["sk-synthetic-123"], 1).await;
    let (status, _) = post(&proxy.url, STREAMED).await;
    assert_eq!(status, 200);
    let rows = rows(&proxy, &dir.0, 1).await;
    let row = &rows[0];
    assert!(row.stream);
    let lines = |s: &str| {
        s.lines()
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(lines(row.response_body.as_deref().unwrap()), lines(SSE));
    assert_eq!(row.input_tokens, Some(11));
    assert_eq!(row.output_tokens, Some(7));
    assert_eq!(row.upstream_request_id.as_deref(), Some("req_stream"));
}

#[tokio::test]
async fn history_does_not_change_client_bytes() {
    let _registry = REGISTRY.lock().await;
    let dir = TempDir::new();
    let on = proxy(Some(&dir.0), &["sk-synthetic-123"], 1).await;
    let off = proxy(None, &["sk-synthetic-123"], 1).await;
    for body in [BUFFERED, STREAMED] {
        assert_eq!(post(&on.url, body).await, post(&off.url, body).await);
    }
    assert_eq!(
        *on.seen.bodies.lock().unwrap(),
        *off.seen.bodies.lock().unwrap(),
        "upstream bytes differ with history on"
    );
}

#[tokio::test]
async fn a_failover_records_each_send() {
    let _registry = REGISTRY.lock().await;
    let dir = TempDir::new();
    let proxy = proxy(Some(&dir.0), &["sk-synthetic-limited", "sk-synthetic-123"], 2).await;
    // Round-robin: one of two requests starts on the limited key and fails over.
    for _ in 0..2 {
        assert_eq!(post(&proxy.url, BUFFERED).await.0, 200);
    }
    assert_eq!(proxy.seen.bodies.lock().unwrap().len(), 3);
    let rows = rows(&proxy, &dir.0, 3).await;
    let failed_over: Vec<&Row> = rows
        .iter()
        .filter(|r| rows.iter().filter(|o| o.exchange_id == r.exchange_id).count() == 2)
        .collect();
    assert_eq!(failed_over.len(), 2, "{rows:#?}");
    let (first, second) = (failed_over[0], failed_over[1]);
    assert_eq!((first.send, second.send), (0, 1));
    assert_eq!(first.status, Some(429));
    assert_eq!(first.outcome, Outcome::Failed);
    assert!(
        first
            .response_body
            .as_deref()
            .is_some_and(|b| b.contains("rate_limit_error"))
    );
    assert_eq!(second.status, Some(200));
    assert_eq!(second.outcome, Outcome::Complete);
    assert_ne!(first.account, second.account);
}

#[tokio::test]
async fn a_dropped_stream_still_records_a_cancelled_row() {
    let _registry = REGISTRY.lock().await;
    let dir = TempDir::new();
    let proxy = proxy(Some(&dir.0), &["sk-synthetic-123"], 1).await;
    let body = r#"{"model":"claude-sonnet-4-6","max_tokens":5,"stream":true,"messages":[{"role":"user","content":"MODE_STALL"}]}"#;
    let client = wreq::Client::new();
    let request = client
        .post(&proxy.url)
        .header("x-api-key", "client-key-1")
        .body(body)
        .send();
    // Whether the proxy has answered yet or not, the client goes away mid-stream.
    if let Ok(Ok(res)) = tokio::time::timeout(Duration::from_millis(500), request).await {
        use futures_util::StreamExt;
        let mut stream = res.bytes_stream();
        let _ = tokio::time::timeout(Duration::from_millis(500), stream.next()).await;
    }
    let rows = rows(&proxy, &dir.0, 1).await;
    assert_eq!(rows[0].outcome, Outcome::Cancelled);
    assert!(
        rows[0]
            .request_body
            .as_deref()
            .is_some_and(|b| b.contains("MODE_STALL"))
    );
}

#[tokio::test]
async fn no_key_reaches_the_file() {
    let _registry = REGISTRY.lock().await;
    let dir = TempDir::new();
    let proxy = proxy(Some(&dir.0), &["sk-synthetic-123"], 1).await;
    post(&proxy.url, BUFFERED).await;
    post(&proxy.url, STREAMED).await;
    let rows = rows(&proxy, &dir.0, 2).await;
    let text = format!("{rows:?}");
    assert!(!text.contains("sk-synthetic-123"), "upstream key in a row");
    assert!(!text.contains("client-key-1"), "client key in a row");
}

#[tokio::test]
async fn history_off_writes_nothing() {
    let _registry = REGISTRY.lock().await;
    let dir = TempDir::new();
    let proxy = proxy(None, &["sk-synthetic-123"], 1).await;
    post(&proxy.url, BUFFERED).await;
    post(&proxy.url, STREAMED).await;
    assert!(!proxy.rt.history().is_running());
    assert!(!dir.0.exists());
}

#[test]
fn facts_rebuilt_for_a_bootstrap_retry_keep_counting_sends() {
    use cpa_core::format::Format;
    let trace = crate::dispatch::Trace::with_request_id(Some("ex".into()));
    let facts = || {
        crate::usage_record::Facts::new(
            crate::usage_record::Client::default(),
            Format::Claude,
            Format::Claude,
            "m",
            b"{}",
            false,
            &trace,
        )
    };
    let (first, retry) = (facts(), facts());
    first.history.sends.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(retry.history.sends.load(std::sync::atomic::Ordering::Relaxed), 1);
}

fn exchange(connection: &str, turn: i32) -> super::HistoryExchange {
    super::HistoryExchange {
        sends: Default::default(),
        turn: Some((connection.into(), turn)),
    }
}

fn base(stream: bool) -> super::AttemptBase {
    super::AttemptBase {
        endpoint: "GET /v1/responses".into(),
        provider: "codex".into(),
        model_requested: "gpt-fixture".into(),
        model: "gpt-fixture".into(),
        account: "acct".into(),
        auth_index: String::new(),
        session_id: String::new(),
        user_agent: String::new(),
        stream,
    }
}

fn dropped() -> super::AttemptEnd {
    super::AttemptEnd {
        response_model: String::new(),
        detail: Default::default(),
        latency_ms: 5,
        ttft_ms: None,
        outcome: Outcome::Cancelled,
        status: None,
        error: None,
    }
}

fn frame(body: &'static [u8]) -> cpa_core::exec::UpstreamRequest<'static> {
    cpa_core::exec::UpstreamRequest {
        url: "wss://upstream.invalid/responses",
        method: "WEBSOCKET",
        body,
        ..cpa_core::exec::UpstreamRequest::default()
    }
}

#[tokio::test]
async fn websocket_frames_are_kept_per_send() {
    use cpa_core::exec::CaptureEvent;
    let _registry = REGISTRY.lock().await;
    let dir = TempDir::new();
    let proxy = proxy(Some(&dir.0), &[], 1).await;
    let sends = super::Sends::new(&exchange("conn", 3), "", proxy.rt.history().budget());
    sends.set_base(proxy.rt.clone(), base(true));
    let sink = super::tee(cpa_core::exec::CaptureSink::default(), sends.clone());
    // A dial without a payload opens no send.
    sink.record(CaptureEvent::WebsocketRequest(frame(b"")));
    sink.record(CaptureEvent::WebsocketRequest(frame(
        b"{\"type\":\"response.create\",\"input\":\"a\nb\"}",
    )));
    sink.record(CaptureEvent::WebsocketHandshake(
        101,
        &[("X-Request-Id".into(), "ws_1".into())],
    ));
    sink.record(CaptureEvent::WebsocketResponse(b"{\"type\":\"response.created\"}"));
    sink.record(CaptureEvent::WebsocketResponse(b"{\"type\":\"response.in_progress\"}"));
    sends.finish(&proxy.rt, dropped());
    let rows = rows(&proxy, &dir.0, 1).await;
    let row = &rows[0];
    assert_eq!((row.exchange_id.as_str(), row.turn, row.send), ("conn", 3, 0));
    assert_eq!(row.transport.as_deref(), Some("websocket"));
    assert_eq!(
        row.request_frames.as_deref(),
        Some(&["{\"type\":\"response.create\",\"input\":\"a\nb\"}".to_owned()][..])
    );
    assert_eq!(
        row.response_frames.as_deref(),
        Some(
            &[
                "{\"type\":\"response.created\"}".to_owned(),
                "{\"type\":\"response.in_progress\"}".to_owned()
            ][..]
        )
    );
    assert_eq!(row.status, Some(101));
    assert_eq!(row.outcome, Outcome::Cancelled);
    assert_eq!(row.upstream_request_id.as_deref(), Some("ws_1"));
    assert_eq!(row.request_body, None);
}

#[tokio::test]
async fn duplex_turns_become_rows_as_their_responses_complete() {
    use cpa_core::exec::CaptureEvent;
    const DONE_1: &[u8] = br#"{"type":"response.completed","response":{"id":"r1","model":"gpt-fixture-1","usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}"#;
    const DONE_2: &[u8] = br#"{"type":"response.failed","response":{"id":"r2"}}"#;
    let _registry = REGISTRY.lock().await;
    let dir = TempDir::new();
    let proxy = proxy(Some(&dir.0), &[], 1).await;
    let sends = super::Sends::new(&exchange("conn", 0), "", proxy.rt.history().budget());
    sends.set_base(proxy.rt.clone(), base(true));
    let sink = super::tee(cpa_core::exec::CaptureSink::default(), sends.clone());
    sink.record(CaptureEvent::WebsocketRequest(frame(
        br#"{"type":"response.create","input":"one"}"#,
    )));
    sink.record(CaptureEvent::WebsocketResponse(br#"{"type":"response.created"}"#));
    sink.record(CaptureEvent::WebsocketResponse(DONE_1));
    sink.record(CaptureEvent::WebsocketRequest(frame(
        br#"{"type":"response.create","input":"two"}"#,
    )));
    sink.record(CaptureEvent::WebsocketResponse(br#"{"type":"response.created"}"#));
    sink.record(CaptureEvent::WebsocketRequest(frame(
        br#"{"type":"response.steer","input":"faster"}"#,
    )));
    sink.record(CaptureEvent::WebsocketResponse(DONE_2));
    // Both turns are written while the connection is still open.
    let rows = rows(&proxy, &dir.0, 2).await;
    assert_eq!((rows[0].send, rows[1].send), (0, 1));
    assert_eq!(rows[0].outcome, Outcome::Complete);
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (Some(3), Some(2)));
    assert_eq!(rows[0].response_model.as_deref(), Some("gpt-fixture-1"));
    assert_eq!(rows[1].outcome, Outcome::Failed);
    assert_eq!(
        rows[1].request_frames.as_ref().map(Vec::len),
        Some(2),
        "the steer joins its turn"
    );
    assert_eq!(rows[1].response_frames.as_ref().map(Vec::len), Some(2));
    // The attempt ends with no turn open: nothing more is written.
    sends.finish(&proxy.rt, dropped());
    let history = proxy.rt.clone();
    tokio::task::spawn_blocking(move || history.history().flush())
        .await
        .unwrap();
    assert_eq!(rows_in(&dir.0).len(), 2);
}

#[tokio::test]
async fn events_without_an_open_send_are_ignored() {
    use cpa_core::exec::CaptureEvent;
    let _registry = REGISTRY.lock().await;
    let dir = TempDir::new();
    let proxy = proxy(Some(&dir.0), &[], 1).await;
    let sends = super::Sends::new(&exchange("conn", 0), "", proxy.rt.history().budget());
    sends.set_base(proxy.rt.clone(), base(false));
    let sink = super::tee(cpa_core::exec::CaptureSink::default(), sends.clone());
    sink.record(CaptureEvent::ResponseError("dial failed"));
    sink.record(CaptureEvent::ResponseChunk(b"late"));
    sends.finish(&proxy.rt, dropped());
    let history = proxy.rt.clone();
    tokio::task::spawn_blocking(move || history.history().flush())
        .await
        .unwrap();
    assert!(rows_in(&dir.0).is_empty());
}

#[tokio::test]
async fn a_stream_stopped_at_its_error_event_is_a_failed_row() {
    use cpa_core::exec::CaptureEvent;
    let _registry = REGISTRY.lock().await;
    let dir = TempDir::new();
    let proxy = proxy(Some(&dir.0), &[], 1).await;
    let sends = super::Sends::new(&exchange("ex", 0), "", proxy.rt.history().budget());
    sends.set_base(proxy.rt.clone(), base(true));
    let sink = super::tee(cpa_core::exec::CaptureSink::default(), sends.clone());
    sink.record(CaptureEvent::Request(cpa_core::exec::UpstreamRequest {
        body: b"{}",
        ..cpa_core::exec::UpstreamRequest::default()
    }));
    sink.record(CaptureEvent::ResponseMetadata(200, &[]));
    sink.record(CaptureEvent::ResponseChunk(b"event: error"));
    sink.record(CaptureEvent::ResponseChunk(
        br#"data: {"type":"error","error":{"message":"overloaded"}}"#,
    ));
    // The tracker sees the stream dropped, not failed.
    sends.finish(&proxy.rt, dropped());
    let rows = rows(&proxy, &dir.0, 1).await;
    assert_eq!(rows[0].outcome, Outcome::Failed);
}

fn create(input: &str) -> Vec<u8> {
    format!(r#"{{"type":"response.create","input":"{input}"}}"#).into_bytes()
}

fn terminal(id: &str, tokens: i64) -> Vec<u8> {
    format!(
        r#"{{"type":"response.completed","response":{{"id":"{id}","usage":{{"input_tokens":{tokens},"output_tokens":1,"total_tokens":{}}}}}}}"#,
        tokens + 1
    )
    .into_bytes()
}

async fn steered(events: impl FnOnce(&CaptureSink)) -> (Proxy, TempDir, std::sync::Arc<super::Sends>) {
    let dir = TempDir::new();
    let proxy = proxy(Some(&dir.0), &[], 1).await;
    let sends = super::Sends::new(&exchange("conn", 0), "", proxy.rt.history().budget());
    sends.set_base(proxy.rt.clone(), base(true));
    events(&super::tee(CaptureSink::default(), sends.clone()));
    (proxy, dir, sends)
}

use cpa_core::exec::{CaptureEvent as E, CaptureSink};

#[tokio::test]
async fn pipelined_turns_keep_their_own_requests_and_responses() {
    let _registry = REGISTRY.lock().await;
    let (a, b, done_a, done_b) = (create("a"), create("b"), terminal("ra", 10), terminal("rb", 20));
    let (proxy, dir, _sends) = steered(|sink| {
        sink.record(E::WebsocketRequest(frame(Box::leak(a.into_boxed_slice()))));
        sink.record(E::WebsocketRequest(frame(Box::leak(b.into_boxed_slice()))));
        sink.record(E::WebsocketResponse(&done_a));
        sink.record(E::WebsocketResponse(&done_b));
    })
    .await;
    let rows = rows(&proxy, &dir.0, 2).await;
    assert!(rows[0].request_frames.as_ref().unwrap()[0].contains("\"a\""));
    assert_eq!(rows[0].input_tokens, Some(10));
    assert!(rows[1].request_frames.as_ref().unwrap()[0].contains("\"b\""));
    assert_eq!(rows[1].input_tokens, Some(20));
}

#[tokio::test]
async fn a_steer_behind_a_pipelined_turn_joins_the_turn_in_progress() {
    let _registry = REGISTRY.lock().await;
    let (a, b, done_a, done_b) = (create("a"), create("b"), terminal("ra", 10), terminal("rb", 20));
    let (proxy, dir, _sends) = steered(|sink| {
        sink.record(E::WebsocketRequest(frame(Box::leak(a.into_boxed_slice()))));
        sink.record(E::WebsocketRequest(frame(Box::leak(b.into_boxed_slice()))));
        sink.record(E::WebsocketRequest(frame(
            br#"{"type":"response.steer","input":"faster"}"#,
        )));
        sink.record(E::WebsocketResponse(&done_a));
        sink.record(E::WebsocketResponse(&done_b));
    })
    .await;
    let rows = rows(&proxy, &dir.0, 2).await;
    assert_eq!(rows[0].request_frames.as_ref().map(Vec::len), Some(2));
    assert_eq!(rows[1].request_frames.as_ref().map(Vec::len), Some(1));
}

#[tokio::test]
async fn an_automatic_successor_response_gets_its_own_row() {
    let _registry = REGISTRY.lock().await;
    let a = create("a");
    let (done_a, done_next) = (terminal("ra", 10), terminal("rnext", 30));
    let (proxy, dir, _sends) = steered(|sink| {
        sink.record(E::WebsocketRequest(frame(Box::leak(a.into_boxed_slice()))));
        sink.record(E::WebsocketRequest(frame(
            br#"{"type":"response.steer","input":"more"}"#,
        )));
        sink.record(E::WebsocketResponse(&done_a));
        sink.record(E::WebsocketResponse(
            br#"{"type":"response.created","response":{"id":"rnext"}}"#,
        ));
        sink.record(E::WebsocketResponse(&done_next));
    })
    .await;
    let rows = rows(&proxy, &dir.0, 2).await;
    assert_eq!(rows[1].input_tokens, Some(30));
    assert_eq!(rows[1].request_frames.as_deref(), Some(&[][..]));
}

#[tokio::test]
async fn a_resent_turn_takes_the_response_not_the_failed_send() {
    let _registry = REGISTRY.lock().await;
    let (a, again, done) = (create("a"), create("a"), terminal("ra", 10));
    let (proxy, dir, sends) = steered(|sink| {
        sink.record(E::WebsocketRequest(frame(Box::leak(a.into_boxed_slice()))));
        sink.record(E::WebsocketError {
            stage: "send",
            error: "broken pipe",
        });
        sink.record(E::WebsocketRequest(frame(Box::leak(again.into_boxed_slice()))));
        sink.record(E::WebsocketResponse(&done));
    })
    .await;
    sends.finish(&proxy.rt, dropped());
    let rows = rows(&proxy, &dir.0, 2).await;
    let by_send = |n: i32| rows.iter().find(|r| r.send == n).unwrap();
    assert_eq!(by_send(0).outcome, Outcome::Failed);
    assert_eq!(by_send(0).response_frames.as_deref(), Some(&[][..]));
    assert_eq!(by_send(1).outcome, Outcome::Complete);
    assert_eq!(by_send(1).input_tokens, Some(10));
}

#[tokio::test]
async fn a_turn_left_open_does_not_repeat_an_earlier_turns_usage() {
    let _registry = REGISTRY.lock().await;
    let (a, b, done_a) = (create("a"), create("b"), terminal("ra", 10));
    let (proxy, dir, sends) = steered(|sink| {
        sink.record(E::WebsocketRequest(frame(Box::leak(a.into_boxed_slice()))));
        sink.record(E::WebsocketResponse(&done_a));
        sink.record(E::WebsocketRequest(frame(Box::leak(b.into_boxed_slice()))));
    })
    .await;
    // The tracker reports the first terminal's usage for the whole attempt.
    let mut end = dropped();
    end.detail = crate::usage_record::parse_codex(&terminal("ra", 10)).unwrap();
    sends.finish(&proxy.rt, end);
    let rows = rows(&proxy, &dir.0, 2).await;
    let open = rows.iter().find(|r| r.send == 1).unwrap();
    assert_eq!(open.outcome, Outcome::Cancelled);
    assert_eq!(open.input_tokens, None);
}

#[tokio::test]
async fn a_websocket_turn_records_time_to_first_frame() {
    let _registry = REGISTRY.lock().await;
    let (a, done) = (create("a"), terminal("ra", 10));
    let (proxy, dir, _sends) = steered(|sink| {
        sink.record(E::WebsocketRequest(frame(Box::leak(a.into_boxed_slice()))));
        sink.record(E::WebsocketResponse(br#"{"type":"response.created"}"#));
        sink.record(E::WebsocketResponse(&done));
    })
    .await;
    let rows = rows(&proxy, &dir.0, 1).await;
    let (ttft, latency) = (rows[0].ttft_ms, rows[0].latency_ms);
    assert!(ttft.is_some() && ttft <= latency, "ttft {ttft:?}, latency {latency:?}");
}

#[test]
fn a_usage_detail_with_only_a_service_tier_writes_no_tokens() {
    let mut row = Row::default();
    row.set_tokens(&crate::usage_record::Detail {
        response_service_tier: "flex".into(),
        ..Default::default()
    });
    assert_eq!((row.input_tokens, row.total_tokens), (None, None));
}
