//! Capture of native Claude sends. The native router buffers the request and streams
//! the response; a capture keeps a handle to the request and to each response chunk as
//! it passes, and the writer thread decodes the response, so the client stream does no
//! extra work.

use std::sync::Arc;

use axum::http::HeaderMap;
use bytes::Bytes;
use chrono::Utc;

use super::body::PendingRow;
use super::row::{Outcome, Row, Source};
use crate::usage_record::Detail;

/// Who and what a native send is for, known before it goes out.
pub(crate) struct NativeSend<'a> {
    pub(crate) exchange_id: &'a str,
    /// The send's number within the exchange, from 0.
    pub(crate) send: usize,
    pub(crate) account: &'a str,
    pub(crate) model: &'a str,
    /// The client's session ID.
    pub(crate) session_id: &'a str,
}

/// One native send in progress. Dropped before [`NativeCapture::finish`], for example
/// when the client leaves while the upstream has not answered, it records the send as
/// cancelled: the upstream may already have received it.
pub(crate) struct NativeCapture {
    rt: Arc<crate::Runtime>,
    pending: Option<PendingRow>,
    /// The response's `content-length`, when it declared one.
    length: Option<u64>,
    /// When the first response byte arrived, in microseconds since the epoch.
    first_byte_us: Option<i64>,
}

/// How a native send ended.
pub(crate) struct NativeEnd<'a> {
    pub(crate) stream: bool,
    pub(crate) outcome: Outcome,
    pub(crate) detail: Option<&'a Detail>,
    pub(crate) error: Option<String>,
}

impl NativeCapture {
    /// A capture for `send`, while history is on.
    pub(crate) fn start(rt: &Arc<crate::Runtime>, send: NativeSend<'_>, request: &Bytes) -> Option<Self> {
        if !super::on(&rt.config()) {
            return None;
        }
        let text = super::text;
        let row = Row {
            exchange_id: send.exchange_id.to_owned(),
            send: i32::try_from(send.send).unwrap_or(i32::MAX),
            started_at_us: Utc::now().timestamp_micros(),
            source: Source::Native,
            endpoint: Some("POST /v1/messages".into()),
            transport: Some("http".into()),
            provider: Some("claude".into()),
            account: text(send.account),
            model: text(send.model),
            model_requested: text(send.model),
            session_id: text(send.session_id),
            outcome: Outcome::Cancelled,
            ..Row::default()
        };
        let mut pending = PendingRow::new(row, rt.history().budget());
        pending.request.push_bytes(request);
        Some(Self {
            rt: rt.clone(),
            pending: Some(pending),
            length: None,
            first_byte_us: None,
        })
    }

    /// The upstream answered with `status` and `headers`.
    pub(crate) fn response_head(&mut self, status: u16, headers: &HeaderMap) {
        self.length = headers
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok());
        let Some(pending) = self.pending.as_mut() else { return };
        pending.row.status = Some(i32::from(status));
        pending.row.upstream_request_id = super::capture::REQUEST_ID_HEADERS
            .iter()
            .find_map(|name| headers.get(*name).and_then(|v| v.to_str().ok()).map(str::to_owned));
        pending.response_encoding = headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|e| !e.is_empty() && !e.eq_ignore_ascii_case("identity"))
            .map(str::to_owned);
    }

    /// A response chunk as received, still encoded.
    pub(crate) fn push(&mut self, chunk: &Bytes) {
        if let Some(pending) = self.pending.as_mut() {
            if !chunk.is_empty() {
                self.first_byte_us.get_or_insert_with(|| Utc::now().timestamp_micros());
            }
            pending.response.push_bytes(chunk);
        }
    }

    pub(crate) fn finish(mut self, end: NativeEnd<'_>) {
        let Some(mut pending) = self.pending.take() else { return };
        // The router forwards the upstream's content-length, so a response can end
        // without its stream being polled to the end and read as cancelled when every
        // byte arrived; its status then decides.
        let delivered = self.length.is_some_and(|n| n == pending.response.total());
        let row = &mut pending.row;
        let now = Utc::now().timestamp_micros();
        row.ended_at_us = Some(now);
        // Timed from this send, not from the request's arrival: admission and earlier
        // sends belong to other rows.
        row.latency_ms = Some((now - row.started_at_us) / 1000);
        row.ttft_ms = self.first_byte_us.map(|at| (at - row.started_at_us) / 1000);
        row.stream = end.stream;
        row.outcome = match end.outcome {
            Outcome::Cancelled if delivered && row.status.is_some_and(|s| s < 400) => Outcome::Complete,
            Outcome::Cancelled if delivered => Outcome::Failed,
            outcome => outcome,
        };
        row.error = end.error.filter(|_| row.outcome != Outcome::Complete);
        if let Some(detail) = end.detail {
            row.set_tokens(detail);
        }
        self.rt.history().submit(&self.rt.config(), pending);
    }
}

impl Drop for NativeCapture {
    fn drop(&mut self) {
        if let Some(mut pending) = self.pending.take() {
            let row = &mut pending.row;
            let now = Utc::now().timestamp_micros();
            row.ended_at_us = Some(now);
            row.latency_ms = Some((now - row.started_at_us) / 1000);
            self.rt.history().submit(&self.rt.config(), pending);
        }
    }
}
