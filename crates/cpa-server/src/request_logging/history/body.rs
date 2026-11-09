//! Bodies collected on the request path for the writer, under a shared byte budget.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use bytes::Bytes;

use super::row::Row;

// ponytail: each body keeps its first 32 MiB, as the request log does; raise with a
// spool-backed body if longer bodies need keeping.
pub(crate) const BODY_CAP: u64 = 32 << 20;
/// Body bytes held by sends in flight or queued, across the process.
pub(crate) const IN_FLIGHT_CAP: u64 = 512 << 20;

/// A byte budget shared by every [`Body`] that reserves from it.
#[derive(Clone)]
pub(crate) struct Budget(Arc<BudgetState>);

struct BudgetState {
    used: AtomicU64,
    cap: u64,
}

impl Budget {
    pub(crate) fn new(cap: u64) -> Self {
        Self(Arc::new(BudgetState {
            used: AtomicU64::new(0),
            cap,
        }))
    }

    pub(crate) fn global() -> &'static Budget {
        static GLOBAL: OnceLock<Budget> = OnceLock::new();
        GLOBAL.get_or_init(|| Budget::new(IN_FLIGHT_CAP))
    }

    fn try_reserve(&self, n: u64) -> bool {
        let used = &self.0.used;
        let mut current = used.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(n).filter(|total| *total <= self.0.cap) else {
                return false;
            };
            match used.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    fn release(&self, n: u64) {
        self.0.used.fetch_sub(n, Ordering::AcqRel);
    }

    #[cfg(test)]
    pub(crate) fn used(&self) -> u64 {
        self.0.used.load(Ordering::Acquire)
    }
}

/// A body collected as chunks. It keeps at most [`BODY_CAP`] bytes and only what the
/// budget allows; past either it counts the rest and marks itself truncated.
pub(crate) struct Body {
    chunks: Vec<Bytes>,
    kept: u64,
    total: u64,
    truncated: bool,
    lines: bool,
    budget: Budget,
}

impl Body {
    pub(crate) fn new(budget: &Budget) -> Self {
        Self {
            chunks: Vec::new(),
            kept: 0,
            total: 0,
            truncated: false,
            lines: false,
            budget: budget.clone(),
        }
    }

    /// Keeps a handle to `bytes`, without copying.
    pub(crate) fn push_bytes(&mut self, bytes: &Bytes) {
        self.keep(bytes.len(), |n| bytes.slice(..n));
    }

    pub(crate) fn push_slice(&mut self, bytes: &[u8]) {
        self.keep(bytes.len(), |n| Bytes::copy_from_slice(&bytes[..n]));
    }

    /// Appends one scanned line, with `\n` between lines.
    pub(crate) fn push_line(&mut self, line: &[u8]) {
        if std::mem::replace(&mut self.lines, true) {
            self.push_bytes(&Bytes::from_static(b"\n"));
        }
        self.push_slice(line);
    }

    fn keep(&mut self, len: usize, take: impl FnOnce(usize) -> Bytes) {
        let len = len as u64;
        self.total = self.total.saturating_add(len);
        if self.truncated || len == 0 {
            return;
        }
        let n = len.min(BODY_CAP - self.kept);
        if n < len {
            self.truncated = true;
        }
        if n == 0 {
            return;
        }
        if !self.budget.try_reserve(n) {
            self.truncated = true;
            return;
        }
        self.kept += n;
        self.chunks.push(take(n as usize));
    }

    pub(crate) fn total(&self) -> u64 {
        self.total
    }

    pub(crate) fn truncated(&self) -> bool {
        self.truncated
    }

    pub(crate) fn into_vec(mut self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.kept as usize);
        for chunk in std::mem::take(&mut self.chunks) {
            out.extend_from_slice(&chunk);
        }
        out
    }
}

impl Drop for Body {
    fn drop(&mut self) {
        self.budget.release(self.kept);
    }
}

/// An HTTP send's bodies.
pub(crate) struct HttpBodies {
    pub(crate) request: Body,
    pub(crate) response: Body,
    /// The response's `content-encoding`, when its bytes are still encoded.
    pub(crate) response_encoding: Option<String>,
}

impl HttpBodies {
    pub(crate) fn new(budget: &Budget) -> Self {
        Self {
            request: Body::new(budget),
            response: Body::new(budget),
            response_encoding: None,
        }
    }
}

/// A send's bodies, as its transport carried them; the transport column follows.
pub(crate) enum Bodies {
    Http(HttpBodies),
    /// One body per frame.
    Websocket {
        request: Vec<Body>,
        response: Vec<Body>,
    },
}

/// A send's row before its bodies are joined, decoded and split into columns.
pub(crate) struct PendingRow {
    pub(crate) row: Row,
    pub(crate) bodies: Bodies,
}

#[cfg(test)]
#[path = "body_tests.rs"]
mod tests;
