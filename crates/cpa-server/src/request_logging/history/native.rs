//! Capture of native Claude sends. The native router buffers the request and streams
//! the response; a capture keeps a handle to the request and to each response chunk as
//! it passes, and the writer thread decodes the response, so the client stream does no
//! extra work.

use axum::http::HeaderMap;
use bytes::Bytes;
use chrono::Utc;

use super::file::{Outcome, Row, Source};
use super::writer::PendingRow;
use crate::usage_record::Detail;

/// One native send in progress.
pub(crate) struct NativeCapture {
    pending: PendingRow,
    /// The response's `content-length`, when it declared one.
    length: Option<u64>,
}

/// What the router knows about a native send when it ends.
pub(crate) struct NativeFacts<'a> {
    pub(crate) account: &'a str,
    pub(crate) model: &'a str,
    pub(crate) session_id: &'a str,
    pub(crate) stream: bool,
    pub(crate) outcome: Outcome,
    pub(crate) latency_ms: i64,
    pub(crate) ttft_ms: Option<i64>,
    pub(crate) detail: Option<&'a Detail>,
    pub(crate) error: Option<String>,
}

impl NativeCapture {
    /// A capture for send `send` of exchange `exchange_id`, while history is on.
    pub(crate) fn start(rt: &crate::Runtime, exchange_id: &str, send: usize, request: &Bytes) -> Option<Self> {
        if !super::on(&rt.config()) {
            return None;
        }
        let row = Row {
            exchange_id: exchange_id.to_owned(),
            send: i32::try_from(send).unwrap_or(i32::MAX),
            started_at_us: Utc::now().timestamp_micros(),
            source: Source::Native,
            endpoint: Some("POST /v1/messages".into()),
            transport: Some("http".into()),
            provider: Some("claude".into()),
            ..Row::default()
        };
        let mut pending = PendingRow::new(row, rt.history().budget());
        pending.request.push_bytes(request);
        Some(Self { pending, length: None })
    }

    /// The upstream answered with `status` and `headers`.
    pub(crate) fn response_head(&mut self, status: u16, headers: &HeaderMap) {
        self.length = headers
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok());
        let row = &mut self.pending.row;
        row.status = Some(i32::from(status));
        row.upstream_request_id = ["request-id", "x-request-id"]
            .iter()
            .find_map(|name| headers.get(*name).and_then(|v| v.to_str().ok()).map(str::to_owned));
        self.pending.response_encoding = headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|e| !e.is_empty() && !e.eq_ignore_ascii_case("identity"))
            .map(str::to_owned);
    }

    /// A response chunk as received, still encoded.
    pub(crate) fn push(&mut self, chunk: &Bytes) {
        self.pending.response.push_bytes(chunk);
    }

    pub(crate) fn finish(self, rt: &crate::Runtime, facts: NativeFacts<'_>) {
        let mut pending = self.pending;
        // The router forwards the upstream's content-length, so the response ends
        // without its stream being polled to the end and reads as cancelled even when
        // every byte arrived.
        let delivered = self.length.is_some_and(|n| n == pending.response.total());
        let row = &mut pending.row;
        let text = |s: &str| (!s.is_empty()).then(|| s.to_owned());
        row.ended_at_us = Some(Utc::now().timestamp_micros());
        row.account = text(facts.account);
        row.model = text(facts.model);
        row.model_requested = text(facts.model);
        row.session_id = text(facts.session_id);
        row.stream = facts.stream;
        row.outcome = match facts.outcome {
            Outcome::Cancelled if delivered => Outcome::Complete,
            outcome => outcome,
        };
        row.latency_ms = Some(facts.latency_ms);
        row.ttft_ms = facts.ttft_ms;
        row.error = facts.error.filter(|_| row.outcome != Outcome::Complete);
        if let Some(detail) = facts.detail.filter(|d| **d != Detail::default()) {
            row.input_tokens = Some(detail.input);
            row.output_tokens = Some(detail.output);
            row.cache_read_tokens = Some(detail.cache_read);
            row.cache_write_tokens = Some(detail.cache_creation);
            row.cache_write_5m_tokens = detail.cache_creation_5m;
            row.cache_write_1h_tokens = detail.cache_creation_1h;
            row.total_tokens = Some(detail.total);
        }
        rt.history().submit(&rt.config(), pending);
    }
}
