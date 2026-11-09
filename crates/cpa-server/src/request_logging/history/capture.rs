//! Capture of the translating proxy's upstream sends. A tee in front of the request's
//! capture sink hands every executor event to the existing sink unchanged and to a
//! per-attempt [`Sends`] recorder. Each send becomes one row: a WebSocket turn as soon
//! as its response ends, the rest when the usage tracker publishes the attempt.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use chrono::Utc;
use cpa_core::exec::{CaptureEvent, CaptureObserver, CaptureSink, UpstreamRequest};

use super::body::{Bodies, Body, Budget, HttpBodies, PendingRow};
use super::row::{Outcome, Row, Source};
use crate::usage_record::Detail;

/// Response headers that carry the provider's request ID, in order of preference.
pub(crate) const REQUEST_ID_HEADERS: [&str; 3] = ["request-id", "x-request-id", "openai-request-id"];

/// Response-frame types that end a Responses WebSocket turn, and whether it succeeded.
/// An incomplete response was answered, as over HTTP.
const TERMINALS: [(&str, bool); 5] = [
    ("response.completed", true),
    ("response.done", true),
    ("response.incomplete", true),
    ("response.failed", false),
    ("error", false),
];

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
    /// The response carried an in-band error event.
    saw_error: bool,
    /// The send failed before any response: a resend replaces it.
    dead: bool,
    /// When the first response bytes arrived, in microseconds since the epoch.
    first_byte_us: Option<i64>,
}

/// What an attempt is for, known when it starts.
#[derive(Clone)]
pub(crate) struct AttemptBase {
    pub(crate) endpoint: String,
    pub(crate) provider: String,
    pub(crate) model_requested: String,
    pub(crate) model: String,
    pub(crate) account: String,
    pub(crate) auth_index: String,
    pub(crate) session_id: String,
    pub(crate) user_agent: String,
    pub(crate) stream: bool,
}

/// How an attempt ended, from the usage tracker.
pub(crate) struct AttemptEnd {
    pub(crate) response_model: String,
    pub(crate) detail: Detail,
    pub(crate) latency_ms: i64,
    pub(crate) ttft_ms: Option<i64>,
    pub(crate) outcome: Outcome,
    pub(crate) status: Option<i32>,
    pub(crate) error: Option<String>,
}

/// Which exchange and turn a request's sends belong to. Every attempt and bootstrap
/// retry of the request shares one.
#[derive(Debug, Clone, Default)]
pub(crate) struct HistoryExchange {
    /// Numbers the exchange's sends.
    pub(crate) sends: Arc<AtomicI32>,
    /// A WebSocket turn's connection ID and turn index.
    pub(crate) turn: Option<(String, i32)>,
}

/// The sends of one dispatch attempt, in order.
pub(crate) struct Sends {
    exchange_id: String,
    turn: i32,
    counter: Arc<AtomicI32>,
    budget: Budget,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    base: Option<(Arc<crate::Runtime>, AttemptBase)>,
    sends: Vec<SendCapture>,
    /// The attempt was published: later events belong to no send.
    finished: bool,
    /// A WebSocket turn was written with its own usage.
    wrote_turn: bool,
}

impl Sends {
    /// An HTTP exchange is named by its request ID; WebSocket turns by their connection.
    pub(crate) fn new(exchange: &HistoryExchange, request_id: &str, budget: &Budget) -> Arc<Self> {
        let (exchange_id, turn) = exchange.turn.clone().unwrap_or_else(|| (request_id.to_owned(), 0));
        Arc::new(Self {
            exchange_id,
            turn,
            counter: exchange.sends.clone(),
            budget: budget.clone(),
            state: Mutex::default(),
        })
    }

    /// What the attempt is for. A WebSocket turn is written once its response ends,
    /// which needs this; it is set before the executor runs.
    pub(crate) fn set_base(&self, rt: Arc<crate::Runtime>, base: AttemptBase) {
        self.lock().base = Some((rt, base));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
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
            saw_error: false,
            dead: false,
            first_byte_us: None,
        }
    }

    fn request(&self, websocket: bool, info: &UpstreamRequest<'_>, body: Option<&Bytes>) {
        // A WebSocket dial without a payload opens no send.
        if websocket && info.body.is_empty() {
            return;
        }
        let mut state = self.lock();
        if state.finished {
            return;
        }
        if websocket {
            let mut frame = Body::new(&self.budget);
            frame.push_slice(info.body);
            // On a steered connection a steer or append frame belongs to the turn in
            // progress, the oldest still waiting, as responses are matched; only a
            // create starts one.
            let joins = (frame_type(info.body) != "response.create")
                .then(|| {
                    let sends = &state.sends;
                    sends
                        .iter()
                        .position(|send| send.websocket && !send.dead)
                        .or_else(|| sends.len().checked_sub(1).filter(|&last| sends[last].websocket))
                })
                .flatten();
            match joins {
                Some(index) => state.sends[index].request_frames.push(frame),
                None => {
                    let mut send = self.open(true);
                    send.request_frames.push(frame);
                    state.sends.push(send);
                }
            }
        } else {
            let mut send = self.open(false);
            match body {
                Some(body) => send.request.push_bytes(body),
                None => send.request.push_slice(info.body),
            }
            state.sends.push(send);
        }
    }

    fn observe(&self, event: &CaptureEvent<'_>) {
        match event {
            CaptureEvent::Request(info) => return self.request(false, info, None),
            CaptureEvent::WebsocketRequest(info) => return self.request(true, info, None),
            CaptureEvent::WebsocketResponse(payload) => return self.websocket_response(payload),
            _ => {}
        }
        let mut state = self.lock();
        let Some(send) = state.sends.last_mut() else {
            // Nothing was sent, or the attempt is over: no send to attach to.
            return;
        };
        match event {
            CaptureEvent::ResponseMetadata(status, headers) | CaptureEvent::WebsocketHandshake(status, headers) => {
                send.status = Some(i32::from(*status));
                send.upstream_request_id = REQUEST_ID_HEADERS.iter().find_map(|name| {
                    headers
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case(name))
                        .map(|(_, value)| value.clone())
                });
            }
            CaptureEvent::ResponseChunk(line) => {
                send.first_byte_us.get_or_insert_with(|| Utc::now().timestamp_micros());
                send.saw_error |= is_error_event(line);
                send.response.push_line(line);
            }
            CaptureEvent::ResponseError(error) => send.error = Some((*error).to_owned()),
            CaptureEvent::WebsocketError { stage, error } => {
                send.error = Some(format!("{stage}: {error}"));
                send.dead |= send.websocket && send.response_frames.is_empty();
            }
            CaptureEvent::Request(_) | CaptureEvent::WebsocketRequest(_) | CaptureEvent::WebsocketResponse(_) => {}
        }
    }

    /// A WebSocket response frame belongs to the oldest turn still waiting: a socket
    /// answers its creates in order. A `response.created` with no turn waiting is a
    /// successor the upstream started on its own after a steer.
    fn websocket_response(&self, payload: &[u8]) {
        let mut state = self.lock();
        if state.finished {
            return;
        }
        let kind = frame_type(payload);
        let waiting = state.sends.iter().position(|send| send.websocket && !send.dead);
        let index = match waiting {
            Some(index) => index,
            None if kind == "response.created" => {
                state.sends.push(self.open(true));
                state.sends.len() - 1
            }
            None => return,
        };
        let send = &mut state.sends[index];
        send.first_byte_us.get_or_insert_with(|| Utc::now().timestamp_micros());
        let mut frame = Body::new(&self.budget);
        frame.push_slice(payload);
        send.response_frames.push(frame);
        if let Some((_, ok)) = TERMINALS.iter().find(|(terminal, _)| *terminal == kind) {
            let send = state.sends.remove(index);
            state.wrote_turn = true;
            self.write_turn(&state, send, *ok, payload);
        }
    }

    /// Writes the WebSocket turn whose response just ended with `terminal`, so a long
    /// steered connection neither holds its turns nor loses them in a crash.
    fn write_turn(&self, state: &State, send: SendCapture, ok: bool, terminal: &[u8]) {
        let Some((rt, base)) = state.base.clone() else { return };
        let message = ["response.error.message", "error.message"]
            .iter()
            .map(|path| cpa_common::json::get(terminal, path).str().into_owned())
            .find(|message| !message.is_empty())
            .unwrap_or_else(|| frame_type(terminal));
        let end = AttemptEnd {
            response_model: cpa_common::json::get(terminal, "response.model").str().into_owned(),
            detail: crate::usage_record::parse_codex(terminal).unwrap_or_default(),
            latency_ms: (Utc::now().timestamp_micros() - send.started_us) / 1000,
            ttft_ms: send.first_byte_us.map(|at| (at - send.started_us) / 1000),
            outcome: if ok { Outcome::Complete } else { Outcome::Failed },
            status: None,
            error: (!ok).then_some(message),
        };
        let row = self.row(send, &base, Some(&end), None);
        rt.history().submit(&rt.config(), row);
    }

    /// Queues one row per send still open when the attempt ends. The last carries the
    /// attempt's outcome and usage; earlier ones were answered and resent, so they count
    /// as failed.
    pub(crate) fn finish(&self, rt: &crate::Runtime, mut end: AttemptEnd) {
        let (sends, base, wrote_turn) = {
            let mut state = self.lock();
            state.finished = true;
            (
                std::mem::take(&mut state.sends),
                state.base.as_ref().map(|(_, base)| base.clone()),
                state.wrote_turn,
            )
        };
        if wrote_turn {
            // The tracker's usage and model are a written turn's; a turn still open
            // has none of its own.
            end.detail = Detail::default();
            end.response_model.clear();
        }
        let Some(base) = base else { return };
        // The attempt's ending belongs to the last send that was not replaced.
        let last = sends.iter().rposition(|send| !send.dead);
        let ended_us = Utc::now().timestamp_micros();
        let mut rows = Vec::with_capacity(sends.len());
        let mut next_started = None;
        for (i, send) in sends.into_iter().enumerate().rev() {
            let started = send.started_us;
            let ended = next_started.unwrap_or(ended_us);
            rows.push(self.row(send, &base, (Some(i) == last).then_some(&end), Some(ended)));
            next_started = Some(started);
        }
        let cfg = rt.config();
        for row in rows.into_iter().rev() {
            rt.history().submit(&cfg, row);
        }
    }

    /// One send's row. Without `end` the send was answered and resent.
    fn row(
        &self,
        send: SendCapture,
        base: &AttemptBase,
        end: Option<&AttemptEnd>,
        ended_us: Option<i64>,
    ) -> PendingRow {
        let text = super::text;
        let mut row = Row {
            exchange_id: self.exchange_id.clone(),
            turn: self.turn,
            send: send.index,
            started_at_us: send.started_us,
            ended_at_us: Some(ended_us.unwrap_or_else(|| Utc::now().timestamp_micros())),
            source: Source::Proxy,
            endpoint: text(&base.endpoint),
            provider: text(&base.provider),
            model_requested: text(&base.model_requested),
            model: text(&base.model),
            account: text(&base.account),
            auth_index: text(&base.auth_index),
            session_id: text(&base.session_id),
            user_agent: text(&base.user_agent),
            stream: base.stream,
            status: send.status,
            outcome: Outcome::Failed,
            error: send.error,
            upstream_request_id: send.upstream_request_id,
            ..Row::default()
        };
        if let Some(end) = end {
            row.outcome = match end.outcome {
                // The server stops reading a stream at its in-band error, so the
                // attempt reads as dropped.
                Outcome::Cancelled if send.saw_error => Outcome::Failed,
                outcome => outcome,
            };
            row.status = row.status.or(end.status);
            row.error = row.error.or_else(|| end.error.clone());
            row.response_model = text(&end.response_model);
            row.latency_ms = Some(end.latency_ms);
            row.ttft_ms = end.ttft_ms;
            row.set_tokens(&end.detail);
        }
        let bodies = if send.websocket {
            Bodies::Websocket {
                request: send.request_frames,
                response: send.response_frames,
            }
        } else {
            Bodies::Http(HttpBodies {
                request: send.request,
                response: send.response,
                response_encoding: None,
            })
        };
        PendingRow { row, bodies }
    }
}

fn frame_type(payload: &[u8]) -> String {
    cpa_common::json::get(payload, "type").str().into_owned()
}

/// An SSE line that opens or carries an error event.
fn is_error_event(line: &[u8]) -> bool {
    line.trim_ascii() == b"event: error"
        || line
            .strip_prefix(b"data:")
            .is_some_and(|data| frame_type(data.trim_ascii()) == "error")
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
