//! A native send is one upstream request of a native Claude exchange, made with one admitted
//! account. It starts the send's history capture, sends, reads the response head, and ends one
//! of three ways: the connection is lost, the account refuses, or the account answers. Each
//! ending applies its own effects on that account (history, activity row, failed counter,
//! usage record, cooldown and event), so the attempt loop only decides whether to try another.
use super::{Account, Exchange, Observation, State, error, hash, headers, now, observation, policy, quota};
use axum::{
    body::Body,
    http::{HeaderMap, Method, StatusCode},
    response::Response,
};
use futures_util::StreamExt;
use serde_json::json;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

/// What every send of one native request shares.
pub(super) struct Request<'a> {
    pub(super) request_id: &'a str,
    /// The hashed session id, used in events and usage records.
    pub(super) sid: &'a str,
    /// The session id the client sent, used in history.
    pub(super) client_sid: &'a str,
    pub(super) model: &'a str,
    /// The client asked for a stream.
    pub(super) requested_stream: bool,
    pub(super) method: &'a Method,
    pub(super) url: &'a str,
    pub(super) headers: &'a HeaderMap,
    pub(super) raw: &'a axum::body::Bytes,
    /// When the request arrived.
    pub(super) started: Instant,
    /// The answered stream holds a clone, which keeps the session active until it ends.
    pub(super) session: &'a Arc<tokio::sync::Mutex<()>>,
}

/// Milliseconds the request spent before this send, reported with its answer.
pub(super) struct Timing {
    pub(super) preparation_ms: u128,
    pub(super) session_wait_ms: u128,
    pub(super) admission_ms: u128,
    /// Waits for the response heads of earlier sends.
    pub(super) upstream_headers_ms: u128,
}

pub(super) enum Ending {
    /// The connection was lost after the send. The request ends with this response and is
    /// never resent, since the upstream may have produced output for it.
    Lost(Response),
    /// The account refused, and the request may try another. `headers_ms` is the wait for the
    /// refusal's response head; `rejection` is set when the refusal was a 429.
    Rejected {
        headers_ms: u128,
        rejection: Option<quota::Rejection>,
    },
    /// The account answered, and the response streams its body to the client.
    Answered(Response),
}

/// Sends `req` with account `a` through `client`, as send number `attempts` (counting from 1)
/// of the request.
pub(super) async fn run(
    s: &Arc<State>,
    req: &Request<'_>,
    a: Account,
    client: wreq::Client,
    attempts: usize,
    timing: Timing,
) -> anyhow::Result<Ending> {
    let started = Instant::now();
    let send = i32::try_from(attempts.saturating_sub(1)).unwrap_or(i32::MAX);
    let mut capture = crate::request_logging::history::native::NativeCapture::start(
        &s.rt,
        crate::request_logging::history::native::NativeSend {
            exchange_id: req.request_id,
            send: attempts.saturating_sub(1),
            credential: &a.file_id,
            account_id: &a.id,
            model: req.model,
            session_id: req.client_sid,
            user_agent: req
                .headers
                .get(axum::http::header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default(),
        },
        req.raw,
    );
    let result = client
        .request(req.method.clone(), req.url)
        .headers(headers(req.headers, Some(&a.token)))
        .body(req.raw.clone())
        .read_timeout(Duration::from_secs(600))
        .send()
        .await;
    let r = match result {
        Ok(r) => r,
        Err(upstream_error) => {
            // Log only classifications, never error Display strings containing URLs or credentials.
            let failure_kind = if upstream_error.is_timeout() {
                "timeout"
            } else if upstream_error.is_connect() {
                "connect"
            } else if upstream_error.is_connection_reset() {
                "connection-reset"
            } else {
                "transport"
            };
            if let Some(capture) = capture.take() {
                capture.finish(crate::request_logging::history::native::NativeEnd {
                    stream: false,
                    outcome: crate::request_logging::history::row::Outcome::TransportLost,
                    detail: None,
                    error: Some(failure_kind.to_owned()),
                });
            }
            s.rt.record_send(observation::send_row(
                req.request_id,
                send,
                &a,
                req.model,
                req.started,
                started,
                crate::request_logging::history::row::Outcome::TransportLost,
            ));
            s.rt.store().note_activity(&a.file_id, false);
            observation::UsageEntry {
                request_id: req.request_id,
                a: &a,
                model: req.model,
                sid: req.sid,
                request_started: req.started,
                stream: req.requested_stream,
                detail: Default::default(),
                failure: Some((0, "transport_lost")),
            }
            .enqueue(s);
            s.cool(&a, req.model, 0, None, false).await?;
            s.event(json!({"type":"transport-failure","requestId":req.request_id,"model":req.model,"outcome":"transport_lost","session":req.sid,"account":a.id,"replayed":false,"failureKind":failure_kind,"elapsedMs":started.elapsed().as_millis()}));
            return Ok(Ending::Lost(error(
                422,
                "Upstream response was lost; request was not replayed to avoid duplicate inference. Check the session before explicitly retrying.",
            )));
        }
    };
    let headers_ms = started.elapsed().as_millis();
    let status = r.status();
    if let Some(capture) = capture.as_mut() {
        capture.response_head(status.as_u16(), r.headers());
    }
    s.rt.executors
        .claude
        .quota()
        .observe_model(a.quota_id(), req.model, r.headers());
    let h = headers(r.headers(), None);
    let encoding = h
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let stream: observation::ResponseStream = Box::pin(r.bytes_stream());
    let (stream, entitlement) = if matches!(status.as_u16(), 400 | 403 | 404 | 422) {
        observation::inspect_rejection(stream, status.as_u16(), &encoding).await
    } else {
        (stream, false)
    };
    if entitlement || [401, 403, 429, 503, 529].contains(&status.as_u16()) {
        let delay = policy::retry_delay(h.get("retry-after").and_then(|v| v.to_str().ok()), now());
        // Read before the cooldown below changes the account's recovery.
        let rejection = (status == StatusCode::TOO_MANY_REQUESTS)
            .then(|| quota::Rejection::new(s.reset(&a, req.model), Some(delay.unwrap_or(60000)), now()));
        let outcome = if entitlement {
            "model_entitlement"
        } else {
            "retryable_rejection"
        };
        if let Some(capture) = capture.take() {
            capture.finish(crate::request_logging::history::native::NativeEnd {
                stream: false,
                outcome: crate::request_logging::history::row::Outcome::Failover,
                detail: None,
                error: Some(outcome.to_owned()),
            });
        }
        s.rt.record_send(crate::activity::sends::SendRow {
            status: Some(i64::from(status.as_u16())),
            ..observation::send_row(
                req.request_id,
                send,
                &a,
                req.model,
                req.started,
                started,
                crate::request_logging::history::row::Outcome::Failover,
            )
        });
        s.rt.store().note_activity(&a.file_id, false);
        observation::UsageEntry {
            request_id: req.request_id,
            a: &a,
            model: req.model,
            sid: req.sid,
            request_started: req.started,
            stream: req.requested_stream,
            detail: Default::default(),
            failure: Some((status.as_u16(), outcome)),
        }
        .enqueue(s);
        s.cool(&a, req.model, status.as_u16(), delay, entitlement).await?;
        if !entitlement {
            s.rt.claude_usage.invalidate(&a.token).await;
        }
        if status == StatusCode::UNAUTHORIZED {
            s.refresh_unauthorized(&a);
        }
        s.event(json!({"type":"failover","requestId":req.request_id,"model":req.model,"session":req.sid,"account":a.id,"status":status.as_u16(),"outcome":outcome,"attempt":attempts,"retryAfterMs":delay}));
        return Ok(Ending::Rejected { headers_ms, rejection });
    }
    let facts = Exchange {
        request_id: req.request_id.to_owned(),
        requested_stream: req.requested_stream,
        sse: h.get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|v| {
            v.split(';')
                .next()
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
        }),
        timing: json!({"preparationMs":timing.preparation_ms,"sessionWaitMs":timing.session_wait_ms,"admissionMs":timing.admission_ms,"upstreamHeadersMs":timing.upstream_headers_ms + headers_ms}),
        headers_at: Instant::now(),
        attempts,
        a,
        model: req.model.to_owned(),
        sid: req.sid.to_owned(),
        status: status.as_u16(),
        digest: hash(req.raw),
        body_bytes: req.raw.len(),
        started: req.started,
        sent_at: started,
        send,
    };
    let mut record = Some(Observation::new(s.clone(), facts, &encoding, capture));
    // The server stops polling a body once it has sent the declared content-length, so the
    // stream never reaches its end; the exchange completes when that many bytes arrived.
    let declared = h
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared == Some(0)
        && let Some(o) = record.take()
    {
        o.finish("complete").await;
    }
    let stream = futures_util::stream::unfold(
        (stream, record, req.session.clone(), declared),
        |(mut stream, mut record, session, mut remaining)| async move {
            match stream.next().await {
                Some(item) => {
                    if let Some(o) = record.as_mut()
                        && let Ok(b) = &item
                    {
                        o.observe(b).await;
                        remaining = remaining.map(|n| n.saturating_sub(b.len() as u64));
                    }
                    if item.is_err()
                        && let Some(o) = record.take()
                    {
                        o.finish("upstream_stream_error").await;
                    } else if remaining == Some(0)
                        && let Some(o) = record.take()
                    {
                        o.finish("complete").await;
                    }
                    Some((item, (stream, record, session, remaining)))
                }
                None => {
                    if let Some(o) = record.take() {
                        o.finish("complete").await;
                    }
                    None
                }
            }
        },
    );
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = h;
    Ok(Ending::Answered(response))
}
