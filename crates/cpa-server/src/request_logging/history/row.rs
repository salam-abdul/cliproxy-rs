//! A history row: one upstream send, its outcome and its bodies.

/// Which server path sent the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Source {
    #[default]
    Proxy,
    Native,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proxy => "proxy",
            Self::Native => "native",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        [Self::Proxy, Self::Native].into_iter().find(|v| v.as_str() == s)
    }
}

/// How one upstream send ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Outcome {
    #[default]
    Complete,
    Failed,
    Failover,
    StreamError,
    TransportLost,
    Cancelled,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Failover => "failover",
            Self::StreamError => "stream_error",
            Self::TransportLost => "transport_lost",
            Self::Cancelled => "cancelled",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        [
            Self::Complete,
            Self::Failed,
            Self::Failover,
            Self::StreamError,
            Self::TransportLost,
            Self::Cancelled,
        ]
        .into_iter()
        .find(|v| v.as_str() == s)
    }
}

/// One upstream send. Every field is chosen here; nothing is copied wholesale from
/// usage records or client facts, which carry keys.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Row {
    pub exchange_id: String,
    pub turn: i32,
    pub send: i32,
    pub started_at_us: i64,
    pub ended_at_us: Option<i64>,
    pub source: Source,
    pub endpoint: Option<String>,
    pub transport: Option<String>,
    pub provider: Option<String>,
    pub model_requested: Option<String>,
    pub model: Option<String>,
    pub response_model: Option<String>,
    pub account: Option<String>,
    pub auth_index: Option<String>,
    pub session_id: Option<String>,
    pub user_agent: Option<String>,
    pub stream: bool,
    pub status: Option<i32>,
    pub outcome: Outcome,
    pub error: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub cache_write_5m_tokens: Option<i64>,
    pub cache_write_1h_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub latency_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub upstream_request_id: Option<String>,
    pub request_body: Option<String>,
    pub request_body_raw: Option<Vec<u8>>,
    pub response_body: Option<String>,
    pub response_body_raw: Option<Vec<u8>>,
    pub request_frames: Option<Vec<String>>,
    pub response_frames: Option<Vec<String>>,
    pub request_bytes: i64,
    pub response_bytes: i64,
    pub request_truncated: bool,
    pub response_truncated: bool,
}

impl Row {
    /// Bytes of body text, raw bodies and frames the row holds.
    pub(crate) fn body_bytes(&self) -> u64 {
        let text = |s: &Option<String>| s.as_ref().map_or(0, String::len);
        let raw = |b: &Option<Vec<u8>>| b.as_ref().map_or(0, Vec::len);
        let frames = |f: &Option<Vec<String>>| f.as_ref().map_or(0, |f| f.iter().map(String::len).sum());
        (text(&self.request_body)
            + text(&self.response_body)
            + raw(&self.request_body_raw)
            + raw(&self.response_body_raw)
            + frames(&self.request_frames)
            + frames(&self.response_frames)) as u64
    }

    /// Token columns from a usage detail; all left empty when it reports no counts.
    pub(crate) fn set_tokens(&mut self, detail: &crate::usage_record::Detail) {
        let counts = [
            detail.input,
            detail.output,
            detail.reasoning,
            detail.cached,
            detail.cache_read,
            detail.cache_creation,
            detail.total,
        ];
        if counts.iter().all(|n| *n == 0) && detail.cache_creation_5m.is_none() && detail.cache_creation_1h.is_none() {
            return;
        }
        self.input_tokens = Some(detail.input);
        self.output_tokens = Some(detail.output);
        self.reasoning_tokens = Some(detail.reasoning);
        self.cache_read_tokens = Some(if detail.cache_read > 0 {
            detail.cache_read
        } else {
            detail.cached
        });
        self.cache_write_tokens = Some(detail.cache_creation);
        self.cache_write_5m_tokens = detail.cache_creation_5m;
        self.cache_write_1h_tokens = detail.cache_creation_1h;
        self.total_tokens = Some(detail.total);
    }
}

/// A body as a string column when it is UTF-8, else as the raw column: one of the two.
/// A `truncated` body cut inside a character loses that character's start, so a cut
/// text body stays text.
pub(crate) fn body_columns(mut bytes: Vec<u8>, truncated: bool) -> (Option<String>, Option<Vec<u8>>) {
    if truncated
        && let Err(error) = std::str::from_utf8(&bytes)
        && error.error_len().is_none()
    {
        bytes.truncate(error.valid_up_to());
    }
    match String::from_utf8(bytes) {
        Ok(text) => (Some(text), None),
        Err(error) => (None, Some(error.into_bytes())),
    }
}
