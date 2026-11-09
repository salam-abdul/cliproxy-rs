//! Capture of the translating proxy's upstream sends. A tee in front of the request's
//! capture sink hands every executor event to the existing sink unchanged and to a
//! per-attempt [`Sends`] recorder; the usage tracker adds the attempt's metadata when
//! it publishes, and each send becomes one row.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use chrono::Utc;
use cpa_core::exec::{CaptureEvent, CaptureObserver, CaptureSink, UpstreamRequest};

use super::file::{Outcome, Row, Source};
use super::writer::{Body, Budget, PendingRow};
use crate::usage_record::Detail;

/// Response headers that carry the provider's request ID, in order of preference.
const REQUEST_ID_HEADERS: [&str; 3] = ["request-id", "x-request-id", "openai-request-id"];

struct SendCapture {
    index: i32,
    started_us: i64,
    websocket: bool,
    request: Body,
    response: Body,
    request_frames: Vec<Body>,
    response_frames: Vec<Body>,
    status: Option<i32>,
    upstream_request_id: Option<String>,
    error: Option<String>,
}

/// The sends of one dispatch attempt, in order.
pub(crate) struct Sends {
    exchange_id: String,
    turn: i32,
    counter: Arc<AtomicI32>,
    budget: Budget,
    sends: Mutex<Vec<SendCapture>>,
}

impl Sends {
    /// `counter` numbers sends across every attempt and retry of the exchange.
    pub(crate) fn new(exchange_id: String, turn: i32, counter: Arc<AtomicI32>, budget: &Budget) -> Arc<Self> {
        Arc::new(Self {
            exchange_id,
            turn,
            counter,
            budget: budget.clone(),
            sends: Mutex::default(),
        })
    }

    fn open(&self, websocket: bool) -> SendCapture {
        SendCapture {
            index: self.counter.fetch_add(1, Ordering::Relaxed),
            started_us: Utc::now().timestamp_micros(),
            websocket,
            request: Body::new(&self.budget),
            response: Body::new(&self.budget),
            request_frames: Vec::new(),
            response_frames: Vec::new(),
            status: None,
            upstream_request_id: None,
            error: None,
        }
    }

    /// The send events after a request belong to.
    fn current<R>(&self, f: impl FnOnce(&mut SendCapture) -> R) -> R {
        let mut sends = self.sends.lock().unwrap_or_else(PoisonError::into_inner);
        if sends.is_empty() {
            let send = self.open(false);
            sends.push(send);
        }
        let last = sends.len() - 1;
        f(&mut sends[last])
    }

    fn request(&self, websocket: bool, info: &UpstreamRequest<'_>, body: Option<&Bytes>) {
        // A websocket dial without a payload opens no send.
        if websocket && info.body.is_empty() {
            return;
        }
        let mut send = self.open(websocket);
        if websocket {
            let mut frame = Body::new(&self.budget);
            frame.push_slice(info.body);
            send.request_frames.push(frame);
        } else {
            match body {
                Some(body) => send.request.push_bytes(body),
                None => send.request.push_slice(info.body),
            }
        }
        self.sends.lock().unwrap_or_else(PoisonError::into_inner).push(send);
    }

    fn observe(&self, event: &CaptureEvent<'_>) {
        match event {
            CaptureEvent::Request(info) => self.request(false, info, None),
            CaptureEvent::WebsocketRequest(info) => self.request(true, info, None),
            CaptureEvent::ResponseMetadata(status, headers) | CaptureEvent::WebsocketHandshake(status, headers) => {
                self.current(|send| {
                    send.status = Some(i32::from(*status));
                    send.upstream_request_id = REQUEST_ID_HEADERS.iter().find_map(|name| {
                        headers
                            .iter()
                            .find(|(key, _)| key.eq_ignore_ascii_case(name))
                            .map(|(_, value)| value.clone())
                    });
                });
            }
            CaptureEvent::ResponseChunk(line) => self.current(|send| send.response.push_line(line)),
            CaptureEvent::ResponseError(error) => self.current(|send| send.error = Some((*error).to_owned())),
            CaptureEvent::WebsocketResponse(payload) => self.current(|send| {
                let mut frame = Body::new(&self.budget);
                frame.push_slice(payload);
                send.response_frames.push(frame);
            }),
            CaptureEvent::WebsocketError { stage, error } => {
                self.current(|send| send.error = Some(format!("{stage}: {error}")))
            }
        }
    }

    /// Queues one row per send. The last send carries the attempt's outcome and usage;
    /// earlier ones were answered and replaced, so they count as failed.
    pub(crate) fn emit(&self, rt: &crate::Runtime, facts: AttemptFacts) {
        let sends = std::mem::take(&mut *self.sends.lock().unwrap_or_else(PoisonError::into_inner));
        let Some(last) = sends.len().checked_sub(1) else { return };
        let ended_us = Utc::now().timestamp_micros();
        let cfg = rt.config();
        let mut next_started = None;
        let mut rows = Vec::with_capacity(sends.len());
        for (i, send) in sends.into_iter().enumerate().rev() {
            let is_last = i == last;
            let mut row = Row {
                exchange_id: self.exchange_id.clone(),
                turn: self.turn,
                send: send.index,
                started_at_us: send.started_us,
                ended_at_us: Some(next_started.unwrap_or(ended_us)),
                source: Source::Proxy,
                endpoint: text(&facts.endpoint),
                transport: Some(if send.websocket { "websocket" } else { "http" }.into()),
                provider: text(&facts.provider),
                model_requested: text(&facts.model_requested),
                model: text(&facts.model),
                response_model: text(&facts.response_model),
                account: text(&facts.account),
                auth_index: text(&facts.auth_index),
                session_id: text(&facts.session_id),
                user_agent: text(&facts.user_agent),
                stream: facts.stream,
                status: send.status,
                outcome: Outcome::Failed,
                error: send.error,
                upstream_request_id: send.upstream_request_id,
                ..Row::default()
            };
            next_started = Some(send.started_us);
            if is_last {
                row.outcome = facts.outcome;
                row.status = row.status.or(facts.status);
                row.error = row.error.or_else(|| facts.error.clone());
                row.latency_ms = Some(facts.latency_ms);
                row.ttft_ms = (facts.ttft_ms > 0).then_some(facts.ttft_ms);
                tokens(&mut row, &facts.detail);
            }
            let mut pending = PendingRow::new(row, &self.budget);
            pending.request = send.request;
            pending.response = send.response;
            if send.websocket {
                pending.request_frames = Some(send.request_frames);
                pending.response_frames = Some(send.response_frames);
            }
            rows.push(pending);
        }
        for pending in rows.into_iter().rev() {
            rt.history().submit(&cfg, pending);
        }
    }
}

fn text(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

/// Token columns from a usage detail; all null when the attempt reported none.
fn tokens(row: &mut Row, detail: &Detail) {
    if *detail == Detail::default() {
        return;
    }
    row.input_tokens = Some(detail.input);
    row.output_tokens = Some(detail.output);
    row.reasoning_tokens = Some(detail.reasoning);
    row.cache_read_tokens = Some(if detail.cache_read > 0 {
        detail.cache_read
    } else {
        detail.cached
    });
    row.cache_write_tokens = Some(detail.cache_creation);
    row.cache_write_5m_tokens = detail.cache_creation_5m;
    row.cache_write_1h_tokens = detail.cache_creation_1h;
    row.total_tokens = Some(detail.total);
}

/// What the usage tracker knows about an attempt when it ends. Every field is chosen;
/// the tracker's record and client facts also hold keys and are never copied whole.
pub(crate) struct AttemptFacts {
    pub(crate) endpoint: String,
    pub(crate) provider: String,
    pub(crate) model_requested: String,
    pub(crate) model: String,
    pub(crate) response_model: String,
    pub(crate) account: String,
    pub(crate) auth_index: String,
    pub(crate) session_id: String,
    pub(crate) user_agent: String,
    pub(crate) stream: bool,
    pub(crate) detail: Detail,
    pub(crate) latency_ms: i64,
    pub(crate) ttft_ms: i64,
    pub(crate) outcome: Outcome,
    pub(crate) status: Option<i32>,
    pub(crate) error: Option<String>,
}

/// The account a row names: the credential's email when it has one, else its ID.
/// Neither holds a key: config credentials have hashed IDs and file credentials are
/// named by their file.
pub(crate) fn account_label(credential: &cpa_core::credential::Credential) -> String {
    credential
        .str("email")
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .map_or_else(|| credential.id.clone(), str::to_owned)
}

struct Tee {
    log: CaptureSink,
    sends: Arc<Sends>,
}

impl CaptureObserver for Tee {
    fn record(&self, event: CaptureEvent<'_>) {
        self.sends.observe(&event);
        self.log.record(event);
    }

    fn record_request(&self, info: UpstreamRequest<'_>, body: &Bytes) {
        self.sends.request(false, &info, Some(body));
        self.log.record_request(info, body);
    }

    fn logs_responses(&self) -> bool {
        true
    }
}

/// `log` with every event also recorded in `sends`. `log` sees exactly what it saw
/// before.
pub(crate) fn tee(log: CaptureSink, sends: Arc<Sends>) -> CaptureSink {
    CaptureSink::new(Arc::new(Tee { log, sends }))
}

#[cfg(test)]
#[path = "capture_tests.rs"]
mod tests;
