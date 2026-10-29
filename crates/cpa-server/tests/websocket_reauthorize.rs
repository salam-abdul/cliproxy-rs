//! Client-key changes on established Responses sockets, with loopback upstreams only.
use axum::{
    Router,
    body::Bytes,
    extract::{
        State,
        ws::{Message as AxMessage, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::IntoResponse,
};
use cpa_core::{config::Config, credential::Credential};
use cpa_exec::Executors;
use cpa_server::router;
use futures_util::StreamExt;
use serde_json::Value;
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-5.5","output":[]}}"#;
const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"resp_1","model":"gpt-5.5","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#;

async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
    )
}

async fn mock(
    State(count): State<Arc<AtomicUsize>>,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
    headers: HeaderMap,
    _body: Bytes,
) -> axum::response::Response {
    assert_eq!(headers["authorization"], "Bearer upstream-key");
    assert!(headers.get("x-api-key").is_none());
    if let Ok(ws) = ws {
        return ws.on_upgrade(move |mut socket| async move {
            while let Some(Ok(AxMessage::Text(_))) = socket.recv().await {
                count.fetch_add(1, Ordering::SeqCst);
                for event in [CREATED, COMPLETED] {
                    if socket.send(AxMessage::Text(event.into())).await.is_err() {
                        return;
                    }
                }
            }
        });
    }
    assert!(headers.get("upgrade").is_none());
    assert!(headers.get("sec-websocket-key").is_none());
    count.fetch_add(1, Ordering::SeqCst);
    (
        [("content-type", "text/event-stream")],
        format!("data: {CREATED}\n\ndata: {COMPLETED}\n\n"),
    )
        .into_response()
}

async fn next(socket: &mut wreq::ws::WebSocket) -> Value {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("downstream frame")
        {
            Some(Ok(wreq::ws::message::Message::Text(text))) => return serde_json::from_str(text.as_str()).unwrap(),
            Some(Ok(wreq::ws::message::Message::Ping(_) | wreq::ws::message::Message::Pong(_))) => continue,
            other => panic!("unexpected downstream frame: {other:?}"),
        }
    }
}

async fn completed(socket: &mut wreq::ws::WebSocket) {
    loop {
        let event = next(socket).await;
        assert_ne!(event["type"], "error", "{event}");
        if event["type"] == "response.completed" {
            break;
        }
    }
}

#[tokio::test]
async fn established_sockets_reauthorize_rotated_header_and_query_keys() {
    for duplex in [false, true] {
        for (query, fallback) in [(false, false), (true, false), (false, true)] {
            let count = Arc::new(AtomicUsize::new(0));
            let (upstream, up) = serve(Router::new().fallback(mock).with_state(count.clone())).await;
            let dir = std::env::temp_dir().join(format!("cpa-test-{}", uuid::Uuid::new_v4()));
            let mut config=Config::parse(&format!("api-keys: [client-old]\ncodex:\n  response-steering: {duplex}\nrouting:\n  retry:\n    request-retry: 0\n")).unwrap();
            config.auth_dir = dir.clone();
            let mut credential = Credential::from_file(
                Path::new("/mock"),
                Path::new("/mock/codex.json"),
                serde_json::json!({"type":"codex"}).as_object().unwrap().clone(),
            )
            .unwrap();
            for (k, v) in [
                ("api_key", "upstream-key"),
                ("base_url", upstream.as_str()),
                ("websockets", if duplex { "true" } else { "false" }),
            ] {
                credential.attributes.insert(k.into(), v.into());
            }
            let rt = Arc::new(cpa_server::testing::runtime(
                config.clone(),
                vec![cpa_server::testing::local(credential)],
                Executors {
                    claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                    codex: Default::default(),
                    devices: Default::default(),
                    openai: Default::default(),
                    google: Default::default(),
                },
            ));
            let (proxy, task) = serve(router(rt.clone())).await;
            let suffix = if query { "?key=client-old" } else { "" };
            let mut request =
                wreq::Client::new().websocket(format!("{}/v1/responses{suffix}", proxy.replace("http://", "ws://")));
            if !query {
                request = request.header("authorization", "Bearer client-old");
            }
            if fallback {
                request = request.header("x-api-key", "client-new");
            }
            let mut socket = request.send().await.unwrap().into_websocket().await.unwrap();
            let create = r#"{"type":"response.create","model":"gpt-5.5","input":[]}"#;
            socket
                .send(wreq::ws::message::Message::Text(create.into()))
                .await
                .unwrap();
            completed(&mut socket).await;
            assert_eq!(count.load(Ordering::SeqCst), 1);
            config.api_keys = vec!["client-new".into()];
            rt.publish_config(config.clone());
            socket
                .send(wreq::ws::message::Message::Text(create.into()))
                .await
                .unwrap();
            let expected = if fallback && !duplex {
                completed(&mut socket).await;
                2
            } else {
                let denied = next(&mut socket).await;
                assert_eq!(denied["type"], "error", "duplex={duplex},query={query}: {denied}");
                assert_eq!(denied["status"], 401, "{denied}");
                if fallback {
                    assert!(denied.to_string().contains("client credential changed"));
                }
                1
            };
            assert_eq!(
                count.load(Ordering::SeqCst),
                expected,
                "rejected turn never reaches upstream"
            );
            config.api_keys = vec!["client-old".into()];
            rt.publish_config(config.clone());
            socket
                .send(wreq::ws::message::Message::Text(create.into()))
                .await
                .unwrap();
            completed(&mut socket).await;
            assert_eq!(count.load(Ordering::SeqCst), expected + 1);
            config.api_keys.clear();
            rt.publish_config(config);
            socket
                .send(wreq::ws::message::Message::Text(create.into()))
                .await
                .unwrap();
            completed(&mut socket).await;
            assert_eq!(count.load(Ordering::SeqCst), expected + 2);
            task.abort();
            up.abort();
        }
    }
}
