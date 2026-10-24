use super::{Account, State};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, BufReader},
    task::JoinHandle,
};

#[derive(Default)]
pub(super) struct UsageScanner {
    line: Vec<u8>,
    json: JsonUsage,
    usage: serde_json::Map<String, Value>,
    oversized_line: bool,
    stopped: bool,
    error: Option<&'static str>,
    data: Vec<u8>,
    data_overflow: bool,
    pub(super) observer_failed: bool,
    pub(super) error_body: Option<Vec<u8>>,
}

#[derive(Default)]
struct JsonUsage {
    depth: i32,
    string: bool,
    escaped: bool,
    word: Vec<u8>,
    key: bool,
    want: bool,
    capture: Option<Vec<u8>>,
    value: Option<Value>,
}
impl JsonUsage {
    pub(super) fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if let Some(capture) = self.capture.as_mut() {
                if capture.len() < 65536 {
                    capture.push(b)
                } else {
                    self.capture = None;
                }
            }
            if self.string {
                if self.escaped {
                    self.escaped = false;
                    continue;
                }
                if b == b'\\' {
                    self.escaped = true;
                    continue;
                }
                if b == b'"' {
                    self.string = false;
                    self.key = self.depth == 1 && self.word == b"usage";
                    self.word.clear();
                } else if self.depth == 1 && self.word.len() < 16 {
                    self.word.push(b)
                }
                continue;
            }
            if b.is_ascii_whitespace() {
                continue;
            }
            if self.want {
                self.want = false;
                if b == b'{' {
                    self.capture = Some(vec![b]);
                }
            }
            match b {
                b'"' => {
                    self.string = true;
                    self.word.clear();
                }
                b':' => {
                    self.want = self.key;
                    self.key = false;
                }
                b'{' | b'[' => {
                    self.depth += 1;
                    self.key = false;
                }
                b'}' | b']' => {
                    self.depth -= 1;
                    if self.depth == 1
                        && let Some(raw) = self.capture.take()
                    {
                        self.value = serde_json::from_slice(&raw).ok();
                    }
                    self.key = false;
                }
                _ => self.key = false,
            }
        }
    }
}
impl UsageScanner {
    pub(super) fn push(&mut self, bytes: &[u8]) {
        self.json.push(bytes);
        if let Some(body) = &mut self.error_body {
            if bytes.len() <= 65536usize.saturating_sub(body.len()) {
                body.extend_from_slice(bytes);
            } else {
                self.error_body = None;
            }
        }
        for &byte in bytes {
            if byte == b'\n' {
                if self.line.last() == Some(&b'\r') {
                    self.line.pop();
                }
                if self.line.is_empty() && !self.oversized_line {
                    self.event();
                } else if !self.oversized_line
                    && let Some(data) = self.line.strip_prefix(b"data:")
                {
                    let data = data.strip_prefix(b" ").unwrap_or(data);
                    if data.len() < 65536usize.saturating_sub(self.data.len()) && !self.data_overflow {
                        self.data.extend_from_slice(data);
                        self.data.push(b'\n');
                    } else {
                        self.data.clear();
                        self.data_overflow = true;
                    }
                }
                self.line.clear();
                self.oversized_line = false;
            } else if self.line.len() < 1048576 && !self.oversized_line {
                self.line.push(byte)
            } else {
                self.line.clear();
                self.oversized_line = true;
                self.data_overflow = true;
            }
        }
    }
    fn event(&mut self) {
        if !self.data_overflow
            && let Ok(v) = serde_json::from_slice::<Value>(&self.data)
        {
            if let Some(u) = v["message"]["usage"].as_object().or(v["usage"].as_object()) {
                self.usage.extend(sanitized_usage(u));
            }
            match v["type"].as_str() {
                Some("message_stop") => self.stopped = true,
                Some("error") => self.error = Some(error_type(&v)),
                _ => {}
            }
        }
        self.data.clear();
        self.data_overflow = false;
    }
    pub(super) fn outcome(&self, status: u16, sse: bool, end: &'static str) -> &'static str {
        if end != "complete" {
            return end;
        }
        if let Some(error) = self.error {
            return error;
        }
        if status >= 400 {
            if let Some(body) = &self.error_body
                && let Ok(v) = serde_json::from_slice::<Value>(body)
            {
                let kind = error_type(&v);
                if kind != "provider_error" {
                    return kind;
                }
            }
            return match status {
                401 | 403 => "authentication_error",
                404 => "not_found",
                429 => "rate_limit_error",
                503 | 529 => "overloaded_error",
                _ => "provider_error",
            };
        }
        if self.observer_failed {
            return "observation_incomplete";
        }
        if sse && !self.stopped {
            return "incomplete_stream";
        }
        "success"
    }
    pub(super) fn value(&self) -> Value {
        if !self.usage.is_empty() {
            Value::Object(self.usage.clone())
        } else {
            self.json
                .value
                .as_ref()
                .and_then(Value::as_object)
                .map(|u| Value::Object(sanitized_usage(u)))
                .unwrap_or(Value::Null)
        }
    }
}

fn sanitized_usage(u: &serde_json::Map<String, Value>) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    for key in [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
        "thinking_tokens",
    ] {
        if let Some(n) = u.get(key).and_then(Value::as_u64) {
            out.insert(key.into(), n.into());
        }
    }
    for (group, keys) in [
        (
            "cache_creation",
            ["ephemeral_5m_input_tokens", "ephemeral_1h_input_tokens"],
        ),
        ("output_tokens_details", ["thinking_tokens", "reasoning_tokens"]),
    ] {
        let mut nested = serde_json::Map::new();
        for key in keys {
            if let Some(n) = u.get(group).and_then(|v| v.get(key)).and_then(Value::as_u64) {
                nested.insert(key.into(), n.into());
            }
        }
        if !nested.is_empty() {
            out.insert(group.into(), Value::Object(nested));
        }
    }
    out
}

fn error_type(v: &Value) -> &'static str {
    match v["error"]["type"].as_str() {
        Some("invalid_request_error") => "invalid_request_error",
        Some("authentication_error") => "authentication_error",
        Some("permission_error") => "permission_error",
        Some("not_found_error") => "not_found",
        Some("rate_limit_error") => "rate_limit_error",
        Some("overloaded_error") => "overloaded_error",
        Some("api_error") => "provider_error",
        _ => "provider_error",
    }
}
#[derive(Clone)]
pub(super) struct Exchange {
    pub(super) request_id: String,
    pub(super) sse: bool,
    pub(super) timing: Value,
    pub(super) headers_at: Instant,
    pub(super) attempts: usize,
    pub(super) a: Account,
    pub(super) model: String,
    pub(super) sid: String,
    pub(super) status: u16,
    pub(super) digest: String,
    pub(super) body_bytes: usize,
    pub(super) started: Instant,
}
pub(super) struct Observation {
    s: Arc<State>,
    facts: Exchange,
    usage: Arc<Mutex<UsageScanner>>,
    writer: Option<tokio::io::DuplexStream>,
    decoder: Option<JoinHandle<()>>,
    published: bool,
    first_byte_ms: Option<u128>,
}
impl Observation {
    pub(super) fn new(s: Arc<State>, facts: Exchange, encoding: &str) -> Self {
        use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder};
        let (writer, reader) = tokio::io::duplex(65536);
        let source = BufReader::new(reader);
        let mut reader: Box<dyn AsyncRead + Unpin + Send> = match encoding {
            "gzip" => Box::new(GzipDecoder::new(source)),
            "br" => Box::new(BrotliDecoder::new(source)),
            "deflate" => Box::new(ZlibDecoder::new(source)),
            "zstd" => Box::new(ZstdDecoder::new(source)),
            _ => Box::new(source),
        };
        let mut scanner = UsageScanner::default();
        if facts.status >= 400 {
            scanner.error_body = Some(Vec::new());
        }
        let usage = Arc::new(Mutex::new(scanner));
        let sink = usage.clone();
        let decoder = tokio::spawn(async move {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer).await {
                    Ok(0) => break,
                    Err(_) => {
                        sink.lock().unwrap().observer_failed = true;
                        break;
                    }
                    Ok(n) => sink.lock().unwrap().push(&buffer[..n]),
                }
            }
        });
        Self {
            s,
            facts,
            usage,
            writer: Some(writer),
            decoder: Some(decoder),
            published: false,
            first_byte_ms: None,
        }
    }
    pub(super) async fn observe(&mut self, b: &[u8]) {
        use tokio::io::AsyncWriteExt;
        if !b.is_empty() && self.first_byte_ms.is_none() {
            self.first_byte_ms = Some(self.facts.started.elapsed().as_millis());
        }
        if let Some(writer) = self.writer.as_mut()
            && writer.write_all(b).await.is_err()
        {
            self.writer.take();
        }
    }
    pub(super) async fn finish(mut self, end: &'static str) {
        self.writer.take();
        if let Some(mut task) = self.decoder.take()
            && tokio::time::timeout(Duration::from_secs(1), &mut task).await.is_err()
        {
            task.abort();
            self.usage.lock().unwrap().observer_failed = true;
        }
        self.publish(end);
    }
    fn publish(&mut self, end: &'static str) {
        if std::mem::replace(&mut self.published, true) {
            return;
        }
        let f = &self.facts;
        let (usage, outcome) = {
            let scan = self.usage.lock().unwrap();
            (scan.value(), scan.outcome(f.status, f.sse, end))
        };
        let success = outcome == "success";
        let complete = end == "complete";
        let status = if f.status >= 400 {
            f.status
        } else {
            match outcome {
                "success" => f.status,
                "client_cancelled" => 499,
                "overloaded_error" => 529,
                _ => 502,
            }
        };
        let mut timing = f.timing.clone();
        timing["firstByteMs"] = json!(self.first_byte_ms);
        timing["responseStreamMs"] = json!(f.headers_at.elapsed().as_millis());
        timing["totalMs"] = json!(f.started.elapsed().as_millis());
        self.s.event(json!({"type":"inference","requestId":f.request_id,"session":f.sid,"account":f.a.id,"model":f.model,"status":status,"upstreamStatus":f.status,"complete":complete,"outcome":outcome,"attempts":f.attempts,"recovered":success && f.attempts>1,"timing":timing,"bodySha256":f.digest,"bodyBytes":f.body_bytes,"usage":usage}));
        self.s.rt.store().note_activity(&f.a.file_id, success);
        let credential = self.s.rt.store().snapshot().into_iter().find(|c| c.id == f.a.file_id);
        let auth_index = credential
            .as_ref()
            .map(|c| cpa_core::config::credentials::auth_index(c))
            .unwrap_or_default();
        let detail = crate::usage_record::parse_claude(&serde_json::to_vec(&json!({"usage":usage})).unwrap());
        let record = crate::usage_record::Record {
            timestamp: chrono::Utc::now().to_rfc3339(),
            execution_id: uuid::Uuid::new_v4().to_string(),
            provider: "claude".into(),
            executor_type: "claude".into(),
            model: f.model.clone(),
            alias: f.model.clone(),
            response_model: String::new(),
            source: f.a.name.clone(),
            auth_index,
            auth_type: "oauth".into(),
            generate: true,
            stream: f.sse,
            failed: !success,
            fail_status: if success { 0 } else { status as i64 },
            fail_body: if success { String::new() } else { outcome.into() },
            latency_ms: f.started.elapsed().as_millis() as i64,
            detail,
            ..Default::default()
        };
        self.s.rt.usage_queue().enqueue(crate::usage_record::queued(
            &record,
            &crate::usage_record::Client {
                session_id: f.sid.clone(),
                session_source: "native",
                endpoint: "POST /v1/messages".into(),
                ..Default::default()
            },
        ));
    }
}
impl Drop for Observation {
    fn drop(&mut self) {
        self.writer.take();
        if self.published {
            if let Some(task) = self.decoder.take() {
                task.abort();
            }
            return;
        }
        self.published = true;
        let mut finalizer = Self {
            s: self.s.clone(),
            facts: self.facts.clone(),
            usage: self.usage.clone(),
            writer: None,
            decoder: self.decoder.take(),
            published: false,
            first_byte_ms: self.first_byte_ms,
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                finalizer.finish("client_cancelled").await;
            });
        } else {
            finalizer.publish("client_cancelled");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn usage_retains_only_bounded_numeric_counters() {
        let mut scan = UsageScanner::default();
        for i in 0..1000 {
            scan.push(
                format!("data: {{\"usage\":{{\"private-{i}\":\"secret\",\"output_tokens\":{i}}}}}\n\n").as_bytes(),
            );
        }
        assert_eq!(scan.value(), serde_json::json!({"output_tokens":999}));
        let mut scan = UsageScanner::default();
        scan.push(br#"{"usage":{"input_tokens":1,"private":"secret","cache_creation":{"ephemeral_5m_input_tokens":2,"private":"secret"}}}"#);
        assert_eq!(
            scan.value(),
            serde_json::json!({"input_tokens":1,"cache_creation":{"ephemeral_5m_input_tokens":2}})
        );
    }
    #[test]
    fn multiline_crlf_and_oversized_events_remain_bounded() {
        let mut scan = UsageScanner::default();
        scan.push(b"data: {\"type\":\"error\",\r\ndata: \"error\":{\"type\":\"permission_error\"}}\r\n\r\n");
        assert_eq!(scan.outcome(200, true, "complete"), "permission_error");
        let mut scan = UsageScanner::default();
        scan.push(b"data: ");
        for _ in 0..2048 {
            scan.push(&[b'x'; 1024]);
        }
        scan.push(b"\n\ndata: {\"type\":\"message_stop\"}\n\n");
        assert_eq!(scan.outcome(200, true, "complete"), "success");
        assert!(scan.line.len() <= 1048576 && scan.data.len() <= 65536);
    }
    #[test]
    fn http_error_classification_never_copies_provider_text() {
        let mut scan = UsageScanner {
            error_body: Some(Vec::new()),
            ..Default::default()
        };
        scan.push(br#"{"error":{"type":"private-token","message":"secret"}}"#);
        assert_eq!(scan.outcome(404, false, "complete"), "not_found");
        scan.observer_failed = true;
        assert_eq!(scan.outcome(200, false, "complete"), "observation_incomplete");
    }
    #[test]
    fn stream_outcomes_require_stop_and_retain_error() {
        let mut scan = UsageScanner::default();
        for byte in b"data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"private\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n" {
            scan.push(&[*byte]);
        }
        assert_eq!(scan.outcome(200, true, "complete"), "overloaded_error");
        let mut scan = UsageScanner::default();
        assert_eq!(scan.outcome(200, true, "complete"), "incomplete_stream");
        scan.push(b"data: {\"type\":\"message_stop\"}\n\n");
        assert_eq!(scan.outcome(200, true, "complete"), "success");
        assert_eq!(scan.outcome(200, true, "client_cancelled"), "client_cancelled");
        assert_eq!(scan.outcome(200, false, "complete"), "success");
        assert_eq!(scan.outcome(404, false, "complete"), "not_found");
    }
}
