//! Capture of native Claude sends. The native router buffers the request and streams
//! the response; a capture keeps a handle to the request and to each response chunk as
//! it passes, and the writer thread decodes the response, so the client stream does no
//! extra work.

use std::sync::Arc;

use axum::http::HeaderMap;
use bytes::Bytes;
use chrono::Utc;

use super::file::{Outcome, Row, Source};
use super::writer::PendingRow;
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
}

/// How a native send ended.
pub(crate) struct NativeEnd<'a> {
    pub(crate) stream: bool,
    pub(crate) outcome: Outcome,
    pub(crate) latency_ms: i64,
    pub(crate) ttft_ms: Option<i64>,
    pub(crate) detail: Option<&'a Detail>,
    pub(crate) error: Option<String>,
}

impl NativeCapture {
    /// A capture for `send`, while history is on.
    pub(crate) fn start(rt: &Arc<crate::Runtime>, send: NativeSend<'_>, request: &Bytes) -> Option<Self> {
        if !super::on(&rt.config()) {
            return None;
        }
        let text = |s: &str| (!s.is_empty()).then(|| s.to_owned());
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
        pending.row.upstream_request_id = ["request-id", "x-request-id"]
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
        row.ended_at_us = Some(Utc::now().timestamp_micros());
        row.stream = end.stream;
        row.outcome = match end.outcome {
            Outcome::Cancelled if delivered && row.status.is_some_and(|s| s < 400) => Outcome::Complete,
            Outcome::Cancelled if delivered => Outcome::Failed,
            outcome => outcome,
        };
        row.latency_ms = Some(end.latency_ms);
        row.ttft_ms = end.ttft_ms;
        row.error = end.error.filter(|_| row.outcome != Outcome::Complete);
        if let Some(detail) = end.detail.filter(|d| **d != Detail::default()) {
            row.input_tokens = Some(detail.input);
            row.output_tokens = Some(detail.output);
            row.cache_read_tokens = Some(detail.cache_read);
            row.cache_write_tokens = Some(detail.cache_creation);
            row.cache_write_5m_tokens = detail.cache_creation_5m;
            row.cache_write_1h_tokens = detail.cache_creation_1h;
            row.total_tokens = Some(detail.total);
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
