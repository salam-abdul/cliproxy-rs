use bytes::Bytes;
use cpa_core::exec::{UpstreamStage, UsageSink};
use futures_util::StreamExt;

// ---- exchange timings ------------------------------------------------------------
//
// Cost for a timed request (one with a usage record): a clock read and a lock of the
// attempt's record per stage, one boxed body per HTTP/1.1 hop, a task-local scope per
// hop, and an addition per response chunk; a request body piece costs a reference
// count. Untimed requests pay nothing here.

tokio::task_local! {
    /// Set by [`watch_connections`]' hook: the timed hop being sent dialled a
    /// connection.
    pub(super) static DIALLED: std::cell::Cell<bool>;
}

/// The connection hook of the clients that timed requests use (Claude's native client
/// and [`GoClients::timed`]): a timed request learns whether a connection was dialled
/// while it waited ([`UpstreamStage::Headers`]). The connector runs in the task that
/// sends the request until the connection is ready; a dial that loses to a pooled
/// connection finishes in the background and is not counted. wreq races the pool
/// against the whole new connection, its HTTP handshake included, so a dial that
/// finished can still lose to a pooled connection that came free: a dial does not say
/// which connection served the request, only its absence does (a pooled one).
///
/// Cost, on every request of a hooked client, timed or not: wreq type-erases a
/// connector stack with layers, so each request clones a few boxed services (also for a
/// pooled connection) and each dial boxes a few more futures. Other clients have no
/// hook and pay nothing.
// ponytail: a caller-built client (`ClaudeExecutor::with_client`, unit tests) has no
// hook, so its connections read as reused. Build clients through this function to time
// them.
pub(crate) fn watch_connections(builder: wreq::ClientBuilder) -> wreq::ClientBuilder {
    builder.connector_layer(Dials)
}

#[derive(Clone, Copy)]
struct Dials;

impl<S> tower::Layer<S> for Dials {
    type Service = Dialling<S>;
    fn layer(&self, inner: S) -> Dialling<S> {
        Dialling(inner)
    }
}

#[derive(Clone)]
struct Dialling<S>(S);

impl<S: tower::Service<R>, R> tower::Service<R> for Dialling<S> {
    type Response = S::Response;
    type Error = S::Error;
    type Future = futures_util::future::Inspect<S::Future, fn(&Result<S::Response, S::Error>)>;

    fn poll_ready(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), S::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, target: R) -> Self::Future {
        use futures_util::FutureExt;
        self.0.call(target).inspect(dialled::<S::Response, S::Error>)
    }
}

fn dialled<T, E>(result: &Result<T, E>) {
    if result.is_ok() {
        let _ = DIALLED.try_with(|dialled| dialled.set(true));
    }
}

/// How much of a timed request body the client is handed at a time. The client takes
/// the next piece while its write buffer holds less than 408 KiB (wreq's HTTP/1.1
/// limit), so the last piece marks the body as handed over once all but under 472 KiB
/// went to the socket.
pub(super) const BODY_PIECE: usize = 64 << 10;

/// A timed request body for hop `hop`: the same bytes and exact length as a plain one,
/// in [`BODY_PIECE`] pieces of the same buffer, reporting [`UpstreamStage::Connected`]
/// when the client first asks for it and [`UpstreamStage::BodySent`] when it takes the
/// last piece.
pub(super) struct TimedBody {
    rest: Bytes,
    len: u64,
    asked: bool,
    hop: u32,
    timing: UsageSink,
}

impl TimedBody {
    pub(super) fn new(body: Bytes, timing: UsageSink, hop: u32) -> Self {
        Self {
            len: body.len() as u64,
            rest: body,
            asked: false,
            hop,
            timing,
        }
    }
}

impl http_body::Body for TimedBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        let this = &mut *self;
        if !std::mem::replace(&mut this.asked, true) {
            this.timing.upstream_stage(UpstreamStage::Connected { hop: this.hop });
        }
        if this.rest.is_empty() {
            return std::task::Poll::Ready(None);
        }
        let piece = this.rest.split_to(this.rest.len().min(BODY_PIECE));
        if this.rest.is_empty() {
            this.timing.upstream_stage(UpstreamStage::BodySent {
                hop: this.hop,
                bytes: this.len,
            });
        }
        std::task::Poll::Ready(Some(Ok(http_body::Frame::data(piece))))
    }

    fn is_end_stream(&self) -> bool {
        self.rest.is_empty()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::SizeHint::with_exact(self.rest.len() as u64)
    }
}

/// A timed response body: reports [`UpstreamStage::FirstByte`] at its first bytes and
/// [`UpstreamStage::BodyRead`] when it ends or is dropped.
pub(super) struct Counted<S> {
    inner: S,
    bytes: u64,
    done: bool,
    timing: UsageSink,
}

impl<S> Counted<S> {
    pub(super) fn new(inner: S, timing: UsageSink) -> Self {
        Self {
            inner,
            bytes: 0,
            done: false,
            timing,
        }
    }

    fn finish(&mut self) {
        if !std::mem::replace(&mut self.done, true) {
            self.timing
                .upstream_stage(UpstreamStage::BodyRead { bytes: self.bytes });
        }
    }
}

impl<S: futures_util::Stream<Item = std::io::Result<Bytes>> + Unpin> futures_util::Stream for Counted<S> {
    type Item = S::Item;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let item = std::task::ready!(self.inner.poll_next_unpin(cx));
        match &item {
            Some(Ok(chunk)) if !chunk.is_empty() => {
                if self.bytes == 0 {
                    self.timing.upstream_stage(UpstreamStage::FirstByte);
                }
                self.bytes += chunk.len() as u64;
            }
            None => self.finish(),
            Some(_) => {}
        }
        std::task::Poll::Ready(item)
    }
}

impl<S> Drop for Counted<S> {
    fn drop(&mut self) {
        self.finish();
    }
}
