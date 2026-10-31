//! Native Claude SSE relay (claude_executor_stream.go).
//!
//! Upstream bytes are read as lines like Go's `bufio.Scanner` (LF split, one
//! trailing CR dropped), MCP aliases are restored per line, and each blank line ends
//! an event. The relay stops after the event carrying `message_stop`, so anything an
//! upstream appends after the terminal event is never forwarded. A transport read
//! failure before `message_stop` becomes Go's `unexpected EOF`; clean truncation is 502.

use std::collections::VecDeque;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use cpa_core::exec::{ExecError, ExecStream, FailureScope};
use futures_util::StreamExt;

use super::alias::{self, Reverse};
use crate::rawjson;

/// Go `bufio.Scanner` buffer limit (50 MB).
const MAX_LINE: usize = 52_428_800;

/// Go `observeClaudeSSEEvent`: only a complete event decides completion or failure.
fn observe(event: &[u8], message_id: &mut String, completed: &mut bool, error: &mut Option<ExecError>) {
    let trim = cpa_common::gostr::trim_space;
    let data: Vec<&[u8]> = event
        .split(|byte| *byte == b'\n')
        .filter_map(|line| {
            trim(line)
                .strip_prefix(b"data:")
                .map(trim)
                .filter(|data| !data.is_empty())
        })
        .collect();
    let has_data = !data.is_empty();
    let payload = if has_data {
        data.join(&b'\n')
    } else {
        trim(event).to_vec()
    };
    if !has_data && !payload.starts_with(b"{") {
        return;
    }
    let Ok(payload) = std::str::from_utf8(&payload) else {
        return;
    };
    if !gjson::valid(payload) {
        return;
    }
    match rawjson::get(payload, "type").str() {
        "message_start" => {
            let id = rawjson::string(payload, "message.id");
            if !id.trim().is_empty() {
                *message_id = id.trim().to_owned();
            }
        }
        "message_stop" => *completed = true,
        "message" if !has_data && !rawjson::string(payload, "stop_reason").trim().is_empty() => {
            let id = rawjson::string(payload, "id");
            if !id.trim().is_empty() {
                *message_id = id.trim().to_owned();
            }
            *completed = true;
        }
        "error" if has_data && error.is_none() => {
            *error = Some(ExecError::local(
                502,
                crate::upstream::scope_for(502),
                payload.to_owned(),
            ));
        }
        _ => {}
    }
}

/// `reverseRemapOAuthToolNamesFromStreamLine` on one raw line.
pub(crate) fn restore_line(line: &[u8], reverse: &Reverse) -> Result<Vec<u8>, String> {
    if reverse.is_empty() {
        return Ok(line.to_vec());
    }
    let trimmed = line.trim_ascii();
    if trimmed.is_empty() || trimmed == b"[DONE]" || trimmed.starts_with(b"event:") {
        return Ok(line.to_vec());
    }
    let is_data = trimmed.starts_with(b"data:");
    let payload = if is_data { trimmed[5..].trim_ascii() } else { trimmed };
    let Ok(payload) = std::str::from_utf8(payload) else {
        return Ok(line.to_vec());
    };
    Ok(match alias::restore_event_payload(payload, reverse)? {
        Some(updated) if is_data => [b"data: ".as_slice(), updated.as_bytes()].concat(),
        Some(updated) => updated.into_bytes(),
        None => line.to_vec(),
    })
}

/// Message ID of a completed stream; `None` when `message_stop` was never seen.
pub(crate) type OnComplete = Box<dyn FnOnce(String) + Send>;

struct Relay {
    body: ExecStream,
    reverse: Arc<Reverse>,
    line: BytesMut,
    event: BytesMut,
    ready: VecDeque<Result<Bytes, ExecError>>,
    message_id: String,
    completed: bool,
    finished: bool,
    upstream_error: Option<ExecError>,
    done: Option<OnComplete>,
    /// Go's usage reporter: every upstream line, before alias restore.
    usage: cpa_core::exec::UsageSink,
    /// Request logs: every scanner line before restore (`AppendAPIResponseChunk`) and
    /// the error that ends the stream (`RecordAPIResponseError`).
    capture: cpa_core::exec::CaptureSink,
}

impl Relay {
    /// Handles one line; returns false once the relay has finished.
    fn accept(&mut self, line: &[u8]) -> bool {
        if self.finished {
            return false;
        }
        // reporter.ObserveResponseModel and StreamUsageBuffer.ObserveClaudeStream.
        self.usage.response_line(cpa_core::format::Format::Claude, line);
        self.capture.record(cpa_core::exec::CaptureEvent::ResponseChunk(line));
        let restored = match restore_line(line, &self.reverse) {
            Ok(restored) => restored,
            Err(message) => {
                self.fail(super::plain_error(format!(
                    "restore Claude OAuth tool name from streaming response: {message}"
                )));
                return false;
            }
        };
        self.event.extend_from_slice(&restored);
        self.event.extend_from_slice(b"\n");
        if cpa_common::gostr::trim_space(&restored).is_empty() {
            observe(
                &self.event,
                &mut self.message_id,
                &mut self.completed,
                &mut self.upstream_error,
            );
            self.flush();
            if self.completed {
                self.finish();
                return false;
            }
            if let Some(error) = self.upstream_error.take() {
                self.fail(error);
                return false;
            }
        }
        true
    }

    fn take_partial(&mut self) {
        let mut partial = std::mem::take(&mut self.line);
        if partial.is_empty() {
            return;
        }
        if partial.last() == Some(&b'\r') {
            partial.truncate(partial.len() - 1);
        }
        self.accept(&partial);
    }

    fn flush(&mut self) {
        if !self.event.is_empty() {
            let event = std::mem::take(&mut self.event).freeze();
            self.ready.push_back(Ok(event));
        }
    }

    fn fail(&mut self, error: ExecError) {
        self.capture_error(&error);
        self.event.clear();
        self.ready.push_back(Err(error));
        self.finished = true;
        self.done = None;
    }

    fn capture_error(&self, error: &ExecError) {
        if self.capture.enabled() {
            let text = String::from_utf8_lossy(&error.body);
            self.capture.record(cpa_core::exec::CaptureEvent::ResponseError(&text));
        }
    }

    fn finish(&mut self) {
        self.finished = true;
        if self.completed
            && let Some(done) = self.done.take()
        {
            done(std::mem::take(&mut self.message_id));
        }
    }

    fn chunk(&mut self, chunk: &[u8]) {
        self.line.extend_from_slice(chunk);
        while let Some(pos) = self.line.iter().position(|b| *b == b'\n') {
            // bufio.Scanner holds at most MAX_LINE bytes: the line and its newline must fit.
            if pos + 1 > MAX_LINE {
                self.fail(super::plain_error("bufio.Scanner: token too long"));
                return;
            }
            let mut line = self.line.split_to(pos + 1);
            line.truncate(pos);
            if line.last() == Some(&b'\r') {
                line.truncate(line.len() - 1);
            }
            if !self.accept(&line) {
                return;
            }
        }
        if self.line.len() > MAX_LINE {
            self.fail(super::plain_error("bufio.Scanner: token too long"));
        }
    }

    fn end(&mut self, error: Option<ExecError>) {
        // bufio.Scanner hands an unterminated final line to the split function on EOF
        // and on read errors alike (atEOF is true once any error is set).
        self.take_partial();
        if self.finished {
            return;
        }
        observe(
            &self.event,
            &mut self.message_id,
            &mut self.completed,
            &mut self.upstream_error,
        );
        self.flush();
        if !self.completed {
            // A submitted inference may have completed remotely; local truncation cannot replay.
            let read_error = error.is_some();
            let mut error = error.or_else(|| self.upstream_error.take()).unwrap_or_else(|| {
                ExecError::local(502, FailureScope::Request, "upstream stream closed before message_stop")
            });
            if read_error {
                error.body = Bytes::from_static(b"unexpected EOF");
                error = crate::upstream::body_read_error_after_response(200, error);
            }
            self.capture_error(&error);
            self.ready.push_back(Err(error));
        }
        self.finish();
    }
}

/// Relays decoded upstream bytes as whole native SSE events. `done` runs once with
/// the message ID when the upstream completed with `message_stop`.
/// Every upstream line is reported to `usage` and `capture` before alias restore.
pub(crate) fn relay(
    body: ExecStream,
    reverse: Arc<Reverse>,
    done: OnComplete,
    usage: cpa_core::exec::UsageSink,
    capture: cpa_core::exec::CaptureSink,
) -> ExecStream {
    let relay = Relay {
        usage,
        capture,
        body,
        reverse,
        line: BytesMut::new(),
        event: BytesMut::new(),
        ready: VecDeque::new(),
        message_id: String::new(),
        completed: false,
        finished: false,
        upstream_error: None,
        done: Some(done),
    };
    futures_util::stream::unfold(relay, |mut st| async move {
        loop {
            if let Some(item) = st.ready.pop_front() {
                return Some((item, st));
            }
            if st.finished {
                return None;
            }
            match st.body.next().await {
                Some(Ok(chunk)) => st.chunk(&chunk),
                Some(Err(error)) => st.end(Some(error)),
                None => st.end(None),
            }
        }
    })
    .boxed()
}

/// `validateClaudeStreamingResponse` for buffered SSE behind a non-stream client.
pub(crate) fn validate_buffered(data: &[u8]) -> Result<(), ExecError> {
    // statusErr{502}: classified by status like any upstream 502.
    let bad = |m: &str| ExecError::local(502, crate::upstream::scope_for(502), m.to_owned());
    let (mut has_data, mut start, mut delta) = (false, false, false);
    for line in data.split(|b| *b == b'\n') {
        let line = line.trim_ascii();
        let Some(payload) = line.strip_prefix(b"data:") else {
            continue;
        };
        let payload = payload.trim_ascii();
        if payload.is_empty() || payload == b"[DONE]" {
            continue;
        }
        has_data = true;
        let payload = std::str::from_utf8(payload).unwrap_or("");
        if !gjson::valid(payload) {
            return Err(bad("claude executor: upstream returned malformed stream data"));
        }
        match rawjson::get(payload, "type").str() {
            "error" => {
                let mut message = rawjson::string(payload, "error.message").trim().to_owned();
                if message.is_empty() {
                    message = rawjson::string(payload, "error.type").trim().to_owned();
                }
                if message.is_empty() {
                    message = "unknown upstream error".into();
                }
                return Err(bad(&format!(
                    "claude executor: upstream returned error event: {message}"
                )));
            }
            "message_start" => {
                if rawjson::string(payload, "message.id").trim().is_empty()
                    || rawjson::string(payload, "message.model").trim().is_empty()
                {
                    return Err(bad(
                        "claude executor: upstream stream message_start is missing id or model",
                    ));
                }
                start = true;
            }
            "message_delta" => delta = true,
            _ => {}
        }
    }
    if !has_data {
        return Err(bad("claude executor: upstream returned empty stream response"));
    }
    if !start {
        return Err(bad(
            "claude executor: upstream stream response is missing message_start",
        ));
    }
    if !delta {
        return Err(bad(
            "claude executor: upstream stream response ended before message completion",
        ));
    }
    Ok(())
}

/// `claudeMessageIDFromSSE`: the message ID only when the buffered stream completed.
pub(crate) fn buffered_message_id(data: &[u8]) -> String {
    let (mut id, mut completed) = (String::new(), false);
    let mut event = Vec::new();
    for line in data.split(|b| *b == b'\n') {
        event.extend_from_slice(line);
        event.push(b'\n');
        if cpa_common::gostr::trim_space(line).is_empty() {
            observe(&event, &mut id, &mut completed, &mut None);
            event.clear();
        }
    }
    observe(&event, &mut id, &mut completed, &mut None);
    if completed { id } else { String::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(items: Vec<Result<&'static [u8], ExecError>>) -> ExecStream {
        futures_util::stream::iter(items.into_iter().map(|r| r.map(Bytes::from_static))).boxed()
    }

    /// Request logs: every scanner line before restore, then Go's
    /// RecordAPIResponseError for the scanner error (`unexpected EOF`) or the restore
    /// failure that ends the stream.
    #[tokio::test]
    async fn request_logs_record_lines_then_the_ending_error() {
        let run = |body: ExecStream, reverse: Arc<Reverse>| async move {
            let captured = std::sync::Arc::new(crate::claude::tests::Captured::default());
            let _: Vec<_> = relay(
                body,
                reverse,
                Box::new(|_| {}),
                Default::default(),
                cpa_core::exec::CaptureSink::new(captured.clone()),
            )
            .collect()
            .await;
            std::mem::take(&mut *captured.0.lock().unwrap())
        };
        let broken = chunks(vec![
            Ok(b"event: ping\r\ndata: {}\n\npartial"),
            Err(ExecError::local(502, FailureScope::Transport, "connection reset")),
        ]);
        assert_eq!(
            run(broken, Arc::default()).await,
            [
                "chunk event: ping",
                "chunk data: {}",
                "chunk ",
                "chunk partial",
                "error unexpected EOF"
            ]
        );
        // Two declared aliases share the suffix: the restore fails on the alias line.
        let mut reverse = Reverse::new();
        reverse.insert("mcp__srv1__query".into(), "mcp__srv1__query".into());
        reverse.insert("mcp__srv2__query".into(), "mcp__srv2__query".into());
        reverse.insert("mcp__virt__word_other".into(), "other".into());
        let ambiguous = chunks(vec![Ok(
            b"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"mcp__virt__query\",\"input\":{}}}\n\n",
        )]);
        let events = run(ambiguous, Arc::new(reverse)).await;
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(events[0].starts_with("chunk data: "), "{events:?}");
        assert!(
            events[1].starts_with("error restore Claude OAuth tool name from streaming response: "),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn relay_waits_for_a_complete_terminal_event() {
        let input = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n";
        let open = futures_util::stream::iter([Ok(Bytes::from_static(input))])
            .chain(futures_util::stream::pending())
            .boxed();
        let mut stream = relay(
            open,
            Arc::default(),
            Box::new(|_| {}),
            Default::default(),
            Default::default(),
        );
        assert!(stream.next().await.is_some());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), stream.next())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn crlf_terminal_and_truncation_follow_go() {
        let done = std::sync::Arc::new(std::sync::Mutex::new(None));
        let seen = done.clone();
        let body = chunks(vec![
            Ok(b"event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\"}}\r\n\r\nevent: message_st"),
            Ok(b"op\r\ndata: {\"type\":\"message_stop\"}\r\n\r\nevent: ping\r\ndata: {}\r\n\r\n"),
        ]);
        let out: Vec<_> = relay(
            body,
            Arc::default(),
            Box::new(move |id| *seen.lock().unwrap() = Some(id)),
            Default::default(),
            Default::default(),
        )
        .map(Result::unwrap)
        .collect()
        .await;
        assert_eq!(
            out,
            [
                Bytes::from_static(
                    b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\"}}\n\n"
                ),
                Bytes::from_static(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
            ]
        );
        assert_eq!(done.lock().unwrap().as_deref(), Some("msg_1"));

        let body = chunks(vec![
            Ok(b"event: content_block_delta\ndata: {}\n\nevent: x\ndata: {"),
            Err(ExecError::local(
                502,
                FailureScope::Transport,
                "upstream request failed",
            )),
        ]);
        let out: Vec<_> = relay(
            body,
            Arc::default(),
            Box::new(|_| panic!("not completed")),
            Default::default(),
            Default::default(),
        )
        .collect()
        .await;
        assert_eq!(out.len(), 3);
        assert_eq!(out[1].as_ref().unwrap().as_ref(), b"event: x\ndata: {\n");
        assert_eq!(out[2].as_ref().unwrap_err().body.as_ref(), b"unexpected EOF");
        assert_eq!(out[2].as_ref().unwrap_err().scope, FailureScope::Request);
    }

    #[tokio::test]
    async fn every_upstream_line_is_reported_before_alias_restore() {
        let usage = std::sync::Arc::new(crate::claude::tests::Usage::default());
        let mut reverse = Reverse::new();
        reverse.insert("mcp__poem_real__leisure_fixture_lookup".into(), "fixture_lookup".into());
        let body = chunks(vec![
            Ok(b"event: content_block_start\r\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"mcp__poem_real__leisure_fixture_lookup\",\"input\":{}}}\r\n\r\n"),
            Ok(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\nevent: ping\n"),
        ]);
        let out: Vec<_> = relay(
            body,
            Arc::new(reverse),
            Box::new(|_| {}),
            cpa_core::exec::UsageSink::new(usage.clone()),
            Default::default(),
        )
        .map(Result::unwrap)
        .collect()
        .await;
        assert!(String::from_utf8_lossy(&out[0]).contains("\"name\":\"fixture_lookup\""));
        let lines: Vec<String> = usage.0.lock().unwrap().iter().map(|(_, _, l)| l.clone()).collect();
        // Scanner lines (CR dropped), the alias as upstream sent it, up to the terminal
        // event's blank line where the relay stops reading.
        assert_eq!(
            lines,
            [
                "event: content_block_start",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"mcp__poem_real__leisure_fixture_lookup\",\"input\":{}}}",
                "",
                "event: message_stop",
                "data: {\"type\":\"message_stop\"}",
                "",
            ]
        );
    }

    #[tokio::test]
    async fn clean_eof_without_message_stop_preserves_partial_then_fails() {
        for bytes in [
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}\n\npartial".as_slice(),
            b"data: [DONE]\n\n",
        ] {
            let body = futures_util::stream::iter([Ok(Bytes::copy_from_slice(bytes))]).boxed();
            let out: Vec<_> = relay(
                body,
                Arc::default(),
                Box::new(|_| panic!("incomplete")),
                Default::default(),
                Default::default(),
            )
            .collect()
            .await;
            let error = out
                .last()
                .expect("terminal error")
                .as_ref()
                .expect_err("EOF is incomplete");
            assert_eq!(error.status, 502);
            assert_eq!(error.scope, FailureScope::Request);
            assert!(error.headers.is_empty());
            assert_eq!(error.body.as_ref(), b"upstream stream closed before message_stop");
            let forwarded: Vec<u8> = out[..out.len() - 1]
                .iter()
                .flat_map(|item| item.as_ref().unwrap().iter().copied())
                .collect();
            let mut expected = bytes.to_vec();
            if !expected.ends_with(b"\n") {
                expected.push(b'\n');
            }
            assert_eq!(forwarded, expected);
        }
    }

    #[tokio::test]
    async fn split_claude_events_observe_completion_and_error_once() {
        let bytes = b"event: message_start\ndata: {\"type\":\"message_start\",\ndata: \"message\":{\"id\":\"split-message\"}}\n\nevent: message_stop\ndata: {\"type\":\ndata: \"message_stop\"}\n\nevent: discarded\n\n";
        let seen = Arc::new(std::sync::Mutex::new(None));
        let callback = seen.clone();
        let out: Vec<_> = relay(
            chunks(vec![Ok(bytes)]),
            Arc::default(),
            Box::new(move |id| *callback.lock().unwrap() = Some(id)),
            Default::default(),
            Default::default(),
        )
        .collect()
        .await;
        assert!(out.iter().all(Result::is_ok));
        assert_eq!(seen.lock().unwrap().as_deref(), Some("split-message"));
        assert_eq!(out.len(), 2);
        assert_eq!(buffered_message_id(bytes), "split-message");
        let bytes = b"event: error\ndata: {\"type\":\"error\",\ndata: \"error\":{\"type\":\"overloaded_error\",\"message\":\"synthetic failure\"}}\n\n";
        let out: Vec<_> = relay(
            chunks(vec![Ok(bytes)]),
            Arc::default(),
            Box::new(|_| panic!("not complete")),
            Default::default(),
            Default::default(),
        )
        .collect()
        .await;
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].as_ref().unwrap().as_ref(), bytes);
        let error = out[1].as_ref().unwrap_err();
        assert_eq!(error.status, 502);
        assert!(String::from_utf8_lossy(&error.body).contains("synthetic failure"));
    }

    #[tokio::test]
    async fn bare_terminal_messages_and_unterminated_errors_follow_event_semantics() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let callback = seen.clone();
        let bytes = br#"{"type":"message","id":"bare-message","stop_reason":"end_turn"}"#;
        let out: Vec<_> = relay(
            chunks(vec![Ok(bytes)]),
            Arc::default(),
            Box::new(move |id| *callback.lock().unwrap() = Some(id)),
            Default::default(),
            Default::default(),
        )
        .collect()
        .await;
        assert!(out.iter().all(Result::is_ok));
        assert_eq!(seen.lock().unwrap().as_deref(), Some("bare-message"));
        assert_eq!(buffered_message_id(bytes), "bare-message");

        let bytes = b"data: {\"type\":\"message\",\"id\":\"not-terminal\",\"stop_reason\":\"end_turn\"}\n\n";
        let out: Vec<_> = relay(
            chunks(vec![Ok(bytes)]),
            Arc::default(),
            Box::new(|_| panic!("SSE still needs message_stop")),
            Default::default(),
            Default::default(),
        )
        .collect()
        .await;
        assert_eq!(out.last().unwrap().as_ref().unwrap_err().status, 502);

        let bytes = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"synthetic failure\"}}";
        let out: Vec<_> = relay(
            chunks(vec![Ok(bytes)]),
            Arc::default(),
            Box::new(|_| panic!("error is not completion")),
            Default::default(),
            Default::default(),
        )
        .collect()
        .await;
        assert_eq!(out.len(), 2);
        assert!(out[0].is_ok());
        let error = out[1].as_ref().unwrap_err();
        assert_eq!(error.status, 502);
        assert_eq!(
            error.body.as_ref(),
            br#"{"type":"error","error":{"message":"synthetic failure"}}"#
        );
    }
    #[tokio::test]
    async fn terminal_read_origin_keeps_unexpected_eof_text_after_scope_fencing() {
        let body = chunks(vec![
            Ok(b"data: {}\n\npartial"),
            Err(ExecError::local(502, FailureScope::Request, "upstream request failed")),
        ]);
        let out = relay(
            body,
            Arc::default(),
            Box::new(|_| panic!("not complete")),
            Default::default(),
            Default::default(),
        )
        .collect::<Vec<_>>()
        .await;
        let error = out.last().unwrap().as_ref().unwrap_err();
        assert_eq!(error.scope, FailureScope::Request);
        assert_eq!(error.body.as_ref(), b"unexpected EOF");
        assert_eq!(out[1].as_ref().unwrap().as_ref(), b"partial\n");
    }
}
