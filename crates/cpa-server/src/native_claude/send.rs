//! A native send is one upstream request of a native Claude exchange, made with one admitted
//! account. It starts the send's history capture, sends, reads the response head, and ends one
//! of three ways: the send gets no response head, the account refuses, or the account answers.
//! A send that gets no head or is refused writes its records and cools the account here. An
//! answer streams to the client, and `Observation` writes its records when the stream ends.
use super::observation::{self, Exchange, Observation, UsageEntry};
use super::{Account, State, error, hash, headers, now, policy, quota};
use crate::request_logging::history::{
    native::{NativeCapture, NativeEnd, NativeSend},
    row::Outcome,
};
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

/// Milliseconds the request has spent so far, reported with its answer. The waits add up
/// across the request's attempts.
#[derive(Default)]
pub(super) struct Timing {
    pub(super) preparation_ms: u128,
    pub(super) session_wait_ms: u128,
    pub(super) admission_ms: u128,
    /// Waits for response heads.
    pub(super) upstream_headers_ms: u128,
}

pub(super) enum Ending {
    /// The send got no response head. The request ends with this response and is never
    /// resent: a send error rarely shows whether the upstream received the request, and a
    /// connection that could not be made is treated the same way.
    Lost(Response),
    /// The account refused, and the request may try another. Set for a 429, with when the
    /// account may be tried again.
    Rejected(Option<quota::Rejection>),
    /// The account answered, and the response streams its body to the client.
    Answered(Response),
}

/// One send's place in its request, which its failure records carry.
struct Attempt<'r> {
    s: &'r Arc<State>,
    req: &'r Request<'r>,
    a: &'r Account,
    /// This send's index within the request, counting from 0.
    index: i32,
    started: Instant,
}
impl Attempt<'_> {
    /// Ends a send that got no response head (`failure` status 0) or was refused with the
    /// `failure` status: it finishes the history capture with `error`, writes the activity
    /// row, counts a failed request for the account, enqueues a failed usage record and
    /// cools the account.
    async fn fail(
        &self,
        capture: Option<NativeCapture>,
        outcome: Outcome,
        error: &str,
        failure: (u16, &'static str),
        delay: Option<u64>,
        entitlement: bool,
    ) -> anyhow::Result<()> {
        let (s, req, a) = (self.s, self.req, self.a);
        if let Some(capture) = capture {
            capture.finish(NativeEnd {
                stream: false,
                outcome,
                detail: None,
                error: Some(error.to_owned()),
            });
        }
        s.rt.record_send(crate::activity::sends::SendRow {
            status: (failure.0 != 0).then_some(i64::from(failure.0)),
            ..observation::send_row(
                req.request_id,
                self.index,
                a,
                req.model,
                req.started,
                self.started,
                outcome,
            )
        });
        s.rt.store().note_activity(&a.file_id, false);
        UsageEntry {
            request_id: req.request_id,
            a,
            model: req.model,
            sid: req.sid,
            request_started: req.started,
            stream: req.requested_stream,
            detail: Default::default(),
            failure: Some(failure),
        }
        .enqueue(s);
        s.cool(a, req.model, failure.0, delay, entitlement).await
    }
}

/// Sends `req` with account `a` through `client`, as attempt number `attempts` (counting
/// from 1) of the request. A send that gets a response head adds its wait for it to
/// `timing`.
pub(super) async fn run(
    s: &Arc<State>,
    req: &Request<'_>,
    a: Account,
    client: wreq::Client,
    attempts: usize,
    timing: &mut Timing,
) -> anyhow::Result<Ending> {
    let started = Instant::now();
    let index = attempts.saturating_sub(1);
    let send = Attempt {
        s,
        req,
        a: &a,
        index: i32::try_from(index).unwrap_or(i32::MAX),
        started,
    };
    let mut capture = NativeCapture::start(
        &s.rt,
        NativeSend {
            exchange_id: req.request_id,
            send: index,
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
            send.fail(
                capture.take(),
                Outcome::TransportLost,
                failure_kind,
                (0, "transport_lost"),
                None,
                false,
            )
            .await?;
            s.event(json!({"type":"transport-failure","requestId":req.request_id,"model":req.model,"outcome":"transport_lost","session":req.sid,"account":a.id,"replayed":false,"failureKind":failure_kind,"elapsedMs":started.elapsed().as_millis()}));
            return Ok(Ending::Lost(error(
                422,
                "Upstream response was lost; request was not replayed to avoid duplicate inference. Check the session before explicitly retrying.",
            )));
        }
    };
    timing.upstream_headers_ms += started.elapsed().as_millis();
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
        send.fail(
            capture.take(),
            Outcome::Failover,
            outcome,
            (status.as_u16(), outcome),
            delay,
            entitlement,
        )
        .await?;
        if !entitlement {
            s.rt.claude_usage.invalidate(&a.token).await;
        }
        if status == StatusCode::UNAUTHORIZED {
            s.refresh_unauthorized(&a);
        }
        s.event(json!({"type":"failover","requestId":req.request_id,"model":req.model,"session":req.sid,"account":a.id,"status":status.as_u16(),"outcome":outcome,"attempt":attempts,"retryAfterMs":delay}));
        return Ok(Ending::Rejected(rejection));
    }
    let send = send.index;
    let facts = Exchange {
        request_id: req.request_id.to_owned(),
        requested_stream: req.requested_stream,
        sse: h.get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|v| {
            v.split(';')
                .next()
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
        }),
        timing: json!({"preparationMs":timing.preparation_ms,"sessionWaitMs":timing.session_wait_ms,"admissionMs":timing.admission_ms,"upstreamHeadersMs":timing.upstream_headers_ms}),
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
