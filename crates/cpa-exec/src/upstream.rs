//! Turning a wreq response into the execution envelope. Shared by provider executors.

use std::collections::VecDeque;
use std::pin::Pin;
use std::time::Duration;

use async_compression::tokio::bufread::{BrotliDecoder, DeflateDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder};
use bytes::{Bytes, BytesMut};
use cpa_core::exec::{AfterAccept, ExecError, ExecResponse, ExecStream, FailureScope, ResponseBody};
use cpa_translate::sse::Framer;
use futures_util::{Stream, StreamExt};
use http::HeaderMap;
use tokio::io::{AsyncBufRead, AsyncReadExt, BufReader};
use tokio_util::io::{ReaderStream, StreamReader};

/// Largest error body [`into_response`] reads; the rest is never pulled off the socket.
/// Its callers are control-plane requests that never show the body to a client, so the
/// bound only limits the read. Inference errors reach clients and keep up to
/// [`crate::proxy::MAX_ERROR_BODY`].
pub(crate) const MAX_CONTROL_ERROR_BODY: usize = 64 * 1024;

pub(crate) fn transport_error(e: wreq::Error) -> ExecError {
    // wreq errors may include a URL with credential-bearing query parameters.
    // wreq tags only connection acquisition errors: HTTP headers/body have not been sent.
    let scope = if e.is_connect() {
        FailureScope::Transport
    } else {
        FailureScope::Request
    };
    ExecError::local(502, scope, "upstream request failed")
}

/// A failed response body read cannot establish whether inference completed.
pub(crate) fn body_read_error_after_response(mut error: ExecError) -> ExecError {
    if error.scope == FailureScope::Transport && error.headers.is_empty() {
        error.scope = FailureScope::Request;
        error.after_accept = Some(AfterAccept::Interrupted);
    }
    error
}

/// A statusless failure before the request was sent (a plain Go error, answered 500):
/// nothing reached the upstream, so dispatch may try another credential.
pub(crate) fn plain_error(message: impl Into<String>) -> ExecError {
    ExecError::local(500, FailureScope::Transport, message)
}

/// A statusless failure before the send that the provider charges to the credential,
/// such as missing or unusable credential fields.
pub(crate) fn credential_error(message: impl Into<String>) -> ExecError {
    ExecError::local(500, FailureScope::Credential, message)
}

/// A statusless failure after the upstream response started: inference may have run,
/// so the error is request-scoped and dispatch never resends it.
pub(crate) fn plain_error_after_response(message: impl Into<String>) -> ExecError {
    ExecError::local(500, FailureScope::Request, message).accepted(AfterAccept::Unprocessed)
}

/// A statusless read failure or early end of an accepted response. Dispatch may fail it
/// over only while the client has received nothing.
pub(crate) fn interrupted_after_response(message: impl Into<String>) -> ExecError {
    ExecError::local(500, FailureScope::Request, message).accepted(AfterAccept::Interrupted)
}

/// Go's `statusErr{code, msg}` classified by [`scope_for`]; an empty message reads
/// `status N`.
pub(crate) fn status_error(status: u16, message: impl Into<Bytes>) -> ExecError {
    status_error_scoped(status, scope_for(status), message.into())
}

/// Go's `statusErr{code, msg}` classified by [`model_scope_for`]; an empty message reads
/// `status N`.
pub(crate) fn model_status_error(status: u16, message: impl Into<Bytes>) -> ExecError {
    status_error_scoped(status, model_scope_for(status), message.into())
}

fn status_error_scoped(status: u16, scope: FailureScope, mut body: Bytes) -> ExecError {
    if body.is_empty() {
        body = Bytes::from(format!("status {status}"));
    }
    ExecError {
        status,
        scope,
        body,
        headers: Box::default(),
        retry_after: None,
        direct: false,
        addon: None,
        after_accept: None,
    }
}

/// The 502 for apply_patch tool input or a response translation the proxy cannot
/// deliver. The upstream already answered, so the error is request-scoped.
pub(crate) fn apply_patch_error() -> ExecError {
    ExecError::local(502, FailureScope::Request, cpa_translate::APPLY_PATCH_UPSTREAM_ERROR)
}

pub(crate) fn body_after_response(body: ExecStream) -> ExecStream {
    body.map(|item| item.map_err(body_read_error_after_response)).boxed()
}

/// A response whose body went through Go's Claude `decodeResponseBody`. `body` is
/// the decoded stream, or the decoder-construction error text Go reports.
pub(crate) struct Decoded {
    pub status: u16,
    /// Without Content-Encoding and Content-Length, which described the raw bytes.
    pub headers: HeaderMap,
    pub body: Result<ExecStream, String>,
}

/// Decodes a Go standard-transport or native exchange ([`crate::proxy::send_routed`]).
pub(crate) async fn decode_upstream(upstream: crate::proxy::Upstream) -> Decoded {
    let mut headers = upstream.headers;
    let encoding = content_encoding(&headers);
    headers.remove(http::header::CONTENT_ENCODING);
    headers.remove(http::header::CONTENT_LENGTH);
    let body = decode_body(upstream.body.map(|r| r.map_err(std::io::Error::other)), &encoding)
        .await
        .map(body_after_response);
    Decoded {
        status: upstream.status,
        headers,
        body,
    }
}

/// `claudeResponseContentEncoding`: every Content-Encoding value, joined by commas.
fn content_encoding(headers: &HeaderMap) -> String {
    headers
        .get_all(http::header::CONTENT_ENCODING)
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .collect::<Vec<_>>()
        .join(",")
}

/// Non-2xx responses become [`ExecError`]; event streams are framed; anything else is
/// buffered. Decompression runs before framing and applies to errors too.
pub(crate) async fn into_response(res: wreq::Response) -> Result<ExecResponse, ExecError> {
    let status = res.status().as_u16();
    let mut headers = res.headers().clone();
    let encoding = content_encoding(&headers);
    let body = decode_body(res.bytes_stream().map(|r| r.map_err(std::io::Error::other)), &encoding)
        .await
        .map_err(|_| decode_error())?;
    let body = body_after_response(body);
    headers.remove(http::header::CONTENT_ENCODING);
    headers.remove(http::header::CONTENT_LENGTH);
    if !(200..300).contains(&status) {
        let body = read_bounded(body, MAX_CONTROL_ERROR_BODY).await?;
        return Err(ExecError {
            status,
            scope: scope_for(status),
            retry_after: retry_after(&headers),
            headers: Box::new(headers),
            body,
            direct: false,
            addon: None,
            after_accept: None,
        });
    }
    let is_sse = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    let body = if is_sse {
        ResponseBody::Stream(framed(body))
    } else {
        let bytes = read_bounded(body, usize::MAX).await?;
        ResponseBody::Buffered(bytes)
    };
    Ok(ExecResponse { status, headers, body })
}

pub(crate) async fn read_bounded<S>(mut body: S, limit: usize) -> Result<Bytes, ExecError>
where
    S: Stream<Item = Result<Bytes, ExecError>> + Unpin,
{
    let mut out = BytesMut::new();
    while out.len() < limit {
        match body.next().await {
            Some(Ok(chunk)) => out.extend_from_slice(&chunk[..chunk.len().min(limit - out.len())]),
            Some(Err(error)) => return Err(error),
            None => break,
        }
    }
    Ok(out.freeze())
}

type Reader = Pin<Box<dyn AsyncBufRead + Send>>;

/// Reads up to `n` bytes without losing them: the bytes, the reader positioned before
/// them again, and the read error that stopped short of `n` (EOF is not an error).
async fn peek(mut reader: Reader, n: usize) -> (Vec<u8>, Reader, Option<std::io::Error>) {
    let mut prefix = Vec::with_capacity(n);
    let mut error = None;
    while prefix.len() < n {
        let mut byte = [0];
        match reader.read(&mut byte).await {
            Ok(0) => break,
            Ok(_) => prefix.push(byte[0]),
            Err(e) => {
                error = Some(e);
                break;
            }
        }
    }
    let rest: Reader = Box::pin(BufReader::new(std::io::Cursor::new(prefix.clone()).chain(reader)));
    (prefix, rest, error)
}

/// `gzip.NewReader`'s eager header read: `EOF`, `unexpected EOF` or the bad header.
async fn gzip_reader(reader: Reader) -> Result<Reader, String> {
    let (head, reader, error) = peek(reader, 10).await;
    if let Some(error) = error {
        return Err(io_text(&error));
    }
    match head.len() {
        0 => Err("EOF".into()),
        1..10 => Err("unexpected EOF".into()),
        _ if head[..3] != [0x1f, 0x8b, 8] => Err("gzip: invalid header".into()),
        _ => {
            let mut decoder = GzipDecoder::new(reader);
            decoder.multiple_members(true);
            Ok(Box::pin(BufReader::new(decoder)))
        }
    }
}

fn zstd_reader(reader: Reader) -> Reader {
    let mut decoder = ZstdDecoder::new(reader);
    decoder.multiple_members(true);
    Box::pin(BufReader::new(decoder))
}

/// `decodeResponseBody`. Without any Content-Encoding header, gzip and zstd are
/// recognised by their magic bytes; otherwise each listed coding is undone in reverse
/// order, `identity` and empty entries are skipped, and anything else is refused.
pub(crate) async fn decode_body<S>(body: S, encoding: &str) -> Result<ExecStream, String>
where
    S: Stream<Item = std::io::Result<Bytes>> + Send + Unpin + 'static,
{
    let mut reader: Reader = Box::pin(BufReader::new(StreamReader::new(body)));
    if encoding.is_empty() {
        // Sniff across arbitrary TCP fragmentation, not only the first chunk.
        let (magic, rest, error) = peek(reader, 4).await;
        reader = rest;
        // bufio.Peek: a short read only counts at EOF with two bytes; a failed read
        // leaves the body as is, so the error surfaces to the caller's read.
        if error.is_none() && magic.starts_with(&[0x1f, 0x8b]) {
            reader = gzip_reader(reader)
                .await
                .map_err(|e| format!("magic-byte gzip: failed to create reader: {e}"))?;
        } else if error.is_none() && magic.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            reader = zstd_reader(reader);
        } else if let Some(error) = error {
            let failed = futures_util::stream::iter([Err(read_error(error))]);
            return Ok(ReaderStream::new(reader)
                .map(|r| r.map_err(read_error))
                .chain(failed)
                .boxed());
        }
        return Ok(ReaderStream::new(reader).map(|r| r.map_err(read_error)).boxed());
    }
    for coding in encoding.split(',').rev() {
        reader = match coding.trim().to_lowercase().as_str() {
            "" | "identity" => continue,
            "gzip" => gzip_reader(reader)
                .await
                .map_err(|e| format!("failed to create gzip reader: {e}"))?,
            "deflate" => {
                // newClaudeDeflateReader: zlib when the first two bytes say so, else raw.
                let (head, rest, _) = peek(reader, 2).await;
                let zlib = head.len() == 2
                    && head[0] & 15 == 8
                    && head[0] >> 4 <= 7
                    && (u16::from(head[0]) * 256 + u16::from(head[1])) % 31 == 0;
                if zlib {
                    Box::pin(BufReader::new(ZlibDecoder::new(rest)))
                } else {
                    Box::pin(BufReader::new(DeflateDecoder::new(rest)))
                }
            }
            "br" => Box::pin(BufReader::new(BrotliDecoder::new(reader))),
            "zstd" => zstd_reader(reader),
            other => return Err(format!("unsupported content encoding {other:?}")),
        };
    }
    Ok(ReaderStream::new(reader).map(|r| r.map_err(read_error)).boxed())
}

/// Go's text for a body read failure: a truncated or reset body is `unexpected EOF`.
// ponytail: decoder failures keep the Rust decoder's wording, not compress/*'s; port
// Go's wording if a client depends on it.
pub(crate) fn io_text(error: &std::io::Error) -> String {
    match error.get_ref() {
        Some(inner) if inner.is::<wreq::Error>() || inner.is::<ExecError>() => "unexpected EOF".into(),
        _ if error.kind() == std::io::ErrorKind::UnexpectedEof => "unexpected EOF".into(),
        _ => error.to_string(),
    }
}

fn read_error(error: std::io::Error) -> ExecError {
    match error.get_ref() {
        Some(inner) if inner.is::<wreq::Error>() => {
            ExecError::local(502, FailureScope::Transport, "upstream request failed")
        }
        Some(inner) => match inner.downcast_ref::<ExecError>() {
            Some(error) => error.clone(),
            None => read_decode_error(&error),
        },
        None => read_decode_error(&error),
    }
}

/// A decompressor failing mid-body: a plain Go error from `io.ReadAll`.
fn read_decode_error(error: &std::io::Error) -> ExecError {
    ExecError::local(500, FailureScope::Transport, io_text(error))
}

fn decode_error() -> ExecError {
    ExecError::local(
        502,
        FailureScope::Request,
        "upstream request failed: invalid or unsupported response compression",
    )
}

/// Frames an SSE body into whole events. The first `Err` is the last item.
pub(crate) fn framed<S>(body: S) -> ExecStream
where
    S: Stream<Item = Result<Bytes, ExecError>> + Send + Unpin + 'static,
{
    struct State<S> {
        body: S,
        framer: Framer,
        ready: VecDeque<Bytes>,
        done: bool,
    }
    let state = State {
        body,
        framer: Framer::default(),
        ready: VecDeque::new(),
        done: false,
    };
    futures_util::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(event) = st.ready.pop_front() {
                return Some((Ok(event), st));
            }
            if st.done {
                return None;
            }
            let fail = match st.body.next().await {
                Some(Ok(chunk)) => {
                    match st
                        .framer
                        .push(&chunk)
                        .map_err(|e| ExecError::local(502, FailureScope::Request, e.to_string()))
                    {
                        Ok(events) => {
                            st.ready.extend(events);
                            continue;
                        }
                        Err(e) => e,
                    }
                }
                Some(Err(e)) => e,
                None => {
                    st.done = true;
                    st.ready.extend(st.framer.finish());
                    continue;
                }
            };
            st.done = true;
            st.ready.clear();
            return Some((Err(fail), st));
        }
    })
    .boxed()
}

// ponytail: status-only classification; Go's per-provider and per-credential
// request-error rules (sdk/cliproxy/auth) are not ported. Classify here by provider
// and credential if a provider's status needs another scope.
pub(crate) fn scope_for(status: u16) -> FailureScope {
    match status {
        401 | 402 | 403 | 408 | 429 | 500.. => FailureScope::Credential,
        _ => FailureScope::Request,
    }
}

/// Go's plain `statusErr` in the API-key executors: never credential-scoped, so a 429
/// cools only the model; other failures by status.
pub(crate) fn model_scope_for(status: u16) -> FailureScope {
    match status {
        429 => FailureScope::Model,
        401 | 402 | 403 | 408 | 500.. => FailureScope::Credential,
        _ => FailureScope::Request,
    }
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let now = std::time::SystemTime::now();
    crate::quota::retry_after(headers.get(http::header::RETRY_AFTER)?.to_str().ok()?, now)?
        .duration_since(now)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(
        items: Vec<std::io::Result<Bytes>>,
    ) -> impl Stream<Item = std::io::Result<Bytes>> + Send + Unpin + 'static {
        futures_util::stream::iter(items)
    }

    async fn decoded<S>(body: S, headers: &HeaderMap) -> Result<ExecStream, String>
    where
        S: Stream<Item = std::io::Result<Bytes>> + Send + Unpin + 'static,
    {
        decode_body(body, &content_encoding(headers)).await
    }

    #[tokio::test]
    async fn error_body_read_is_bounded() {
        let endless = futures_util::stream::repeat_with(|| Ok(Bytes::from_static(&[b'x'; 1000])));
        let body = read_bounded(endless, 2500).await.unwrap();
        assert_eq!(body.len(), 2500, "must stop reading at the limit");
    }

    #[tokio::test]
    async fn framed_stream_stops_after_first_error() {
        let failed = futures_util::stream::iter(vec![Err(decode_error()), Ok(Bytes::from_static(b"data: x\n\n"))]);
        let items: Vec<_> = framed(failed).collect().await;
        assert_eq!(items.len(), 1);
        assert!(items[0].is_err());

        let ok = futures_util::stream::iter(vec![
            Ok(Bytes::from_static(b"data: a\n\ndata: b")),
            Ok(Bytes::from_static(b"\n\n")),
        ]);
        let items: Vec<Bytes> = framed(ok).map(Result::unwrap).collect().await;
        assert_eq!(
            items,
            [Bytes::from_static(b"data: a\n\n"), Bytes::from_static(b"data: b\n\n")]
        );
    }

    #[tokio::test]
    async fn declared_encodings_follow_go_decode_response_body() {
        let mut h = HeaderMap::new();
        h.insert(http::header::CONTENT_ENCODING, "compress".parse().unwrap());
        let error = decoded(chunks(vec![Ok(Bytes::from_static(b"bad"))]), &h).await.err();
        assert_eq!(error.as_deref(), Some(r#"unsupported content encoding "compress""#));
        // An explicit identity disables magic-byte sniffing: gzip bytes pass through.
        let gzip_magic = Bytes::from_static(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3]);
        h.insert(http::header::CONTENT_ENCODING, "identity".parse().unwrap());
        let stream = decoded(chunks(vec![Ok(gzip_magic.clone())]), &h).await.unwrap();
        assert_eq!(read_bounded(stream, usize::MAX).await.unwrap(), gzip_magic);
        // gzip.NewReader reads its header eagerly.
        h.insert(http::header::CONTENT_ENCODING, "gzip".parse().unwrap());
        let error = decoded(chunks(vec![Ok(Bytes::from_static(b"bad body"))]), &h)
            .await
            .err();
        assert_eq!(error.as_deref(), Some("failed to create gzip reader: unexpected EOF"));
        let error = decoded(chunks(vec![Ok(Bytes::from_static(b"not a gzip body"))]), &h)
            .await
            .err();
        assert_eq!(
            error.as_deref(),
            Some("failed to create gzip reader: gzip: invalid header")
        );
        let error = decoded(chunks(vec![]), &h).await.err();
        assert_eq!(error.as_deref(), Some("failed to create gzip reader: EOF"));
    }

    #[tokio::test]
    async fn compression_decodes_fragmented_gzip_zstd_brotli_and_deflate() {
        use async_compression::tokio::bufread::{BrotliEncoder, DeflateEncoder, GzipEncoder, ZlibEncoder, ZstdEncoder};
        const PLAIN: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        for name in ["gzip", "zstd", "br", "deflate", "raw-deflate"] {
            let reader = BufReader::new(std::io::Cursor::new(PLAIN));
            let mut encoder: Pin<Box<dyn tokio::io::AsyncRead + Send>> = match name {
                "gzip" => Box::pin(GzipEncoder::new(reader)),
                "zstd" => Box::pin(ZstdEncoder::new(reader)),
                "br" => Box::pin(BrotliEncoder::new(reader)),
                "deflate" => Box::pin(ZlibEncoder::new(reader)),
                _ => Box::pin(DeflateEncoder::new(reader)),
            };
            let mut encoded = Vec::new();
            encoder.read_to_end(&mut encoded).await.unwrap();
            for declared in [true, false] {
                if !declared && !matches!(name, "gzip" | "zstd") {
                    continue;
                }
                let mut headers = HeaderMap::new();
                if declared {
                    headers.insert("content-encoding", name.trim_start_matches("raw-").parse().unwrap());
                }
                let chunks = encoded
                    .iter()
                    .map(|byte| Ok(Bytes::copy_from_slice(&[*byte])))
                    .collect();
                let stream = decoded(self::chunks(chunks), &headers).await.unwrap();
                assert_eq!(
                    read_bounded(stream, usize::MAX).await.unwrap(),
                    PLAIN,
                    "{name}, declared={declared}"
                );
            }
            if matches!(name, "gzip" | "zstd") {
                let doubled = [encoded.as_slice(), encoded.as_slice()].concat();
                let stream = decoded(chunks(vec![Ok(Bytes::from(doubled))]), &HeaderMap::new())
                    .await
                    .unwrap();
                assert_eq!(read_bounded(stream, usize::MAX).await.unwrap(), [PLAIN, PLAIN].concat());
            }
            if name == "gzip" {
                *encoded.last_mut().unwrap() ^= 1;
                let stream = decoded(chunks(vec![Ok(Bytes::from(encoded))]), &HeaderMap::new())
                    .await
                    .unwrap();
                assert!(
                    read_bounded(stream, usize::MAX).await.is_err(),
                    "corrupt gzip trailer must not be buffered as success"
                );
            }
        }
    }

    #[tokio::test]
    async fn invalid_request_is_terminal_and_raw_body_errors_keep_their_origin() {
        let error = wreq::Client::new().get("not a URL").send().await.unwrap_err();
        assert_eq!(transport_error(error).scope, FailureScope::Request);
        let error = wreq::Client::new().get("not a URL").send().await.unwrap_err();
        let body = chunks(vec![
            Ok(Bytes::from_static(b"data: ok\n\n")),
            Err(std::io::Error::other(error)),
        ]);
        let stream = decoded(body, &HeaderMap::new()).await.unwrap();
        let items: Vec<_> = framed(stream).collect().await;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].as_ref().unwrap(), b"data: ok\n\n".as_slice());
        assert_eq!(items[1].as_ref().unwrap_err().scope, FailureScope::Transport);
        // A Go-transport body error keeps its own classification through decoding.
        let failed = ExecError::local(502, FailureScope::Transport, "upstream request failed");
        let body = chunks(vec![Err(std::io::Error::other(failed))]);
        let stream = decoded(body, &HeaderMap::new()).await.unwrap();
        let items: Vec<_> = stream.collect().await;
        assert_eq!(items[0].as_ref().unwrap_err().scope, FailureScope::Transport);
    }

    #[tokio::test]
    async fn response_read_errors_are_terminal_without_reclassifying_explicit_origins() {
        for status in [199, 200, 204, 206, 299, 302, 400, 503] {
            for (scope, classified) in [
                (FailureScope::Transport, false),
                (FailureScope::Transport, true),
                (FailureScope::Credential, false),
                (FailureScope::Model, false),
                (FailureScope::Request, false),
            ] {
                let mut error = ExecError::local(502, scope, "synthetic body read failure");
                if classified {
                    error.headers.insert("x-classification", "retained".parse().unwrap());
                }
                let original = error.clone();
                let upstream = crate::proxy::Upstream {
                    status,
                    headers: HeaderMap::new(),
                    body: futures_util::stream::iter([Ok(Bytes::from_static(b"partial")), Err(error)]).boxed(),
                };
                let decoded = decode_upstream(upstream).await;
                assert_eq!(decoded.status, status);
                let items = decoded.body.unwrap().collect::<Vec<_>>().await;
                let partial = items
                    .iter()
                    .filter_map(|item| item.as_ref().ok())
                    .flat_map(|bytes| bytes.iter().copied())
                    .collect::<Vec<_>>();
                assert_eq!(partial, b"partial");
                let error = items.last().unwrap().as_ref().unwrap_err();
                let expected = if scope == FailureScope::Transport && !classified {
                    FailureScope::Request
                } else {
                    scope
                };
                assert_eq!(
                    error.scope, expected,
                    "status={status}, scope={scope:?}, classified={classified}"
                );
                assert_eq!(error.status, original.status);
                assert_eq!(error.body, original.body);
                assert_eq!(error.headers, original.headers);
            }
        }
    }

    #[tokio::test]
    async fn into_response_fences_only_successful_body_reads_for_buffered_and_sse() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for status in [200, 206, 400, 503] {
            for sse in [true, false] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let server = tokio::spawn(async move {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = [0u8; 4096];
                    let _ = socket.read(&mut request).await.unwrap();
                    let kind = if sse { "text/event-stream" } else { "application/json" };
                    socket.write_all(format!("HTTP/1.1 {status} test\r\nContent-Type: {kind}\r\nContent-Length: 100\r\nConnection: close\r\n\r\ndata: partial").as_bytes()).await.unwrap();
                });
                let response = wreq::Client::new()
                    .get(format!("http://{address}/fixture"))
                    .send()
                    .await
                    .unwrap();
                let error = match into_response(response).await {
                    Err(error) => error,
                    Ok(response) => match response.body {
                        ResponseBody::Stream(stream) => {
                            let items = stream.collect::<Vec<_>>().await;
                            assert!(
                                items.first().is_some_and(Result::is_err),
                                "failure before first SSE payload"
                            );
                            items.into_iter().find_map(Result::err).expect("read failure")
                        }
                        ResponseBody::Buffered(_) => panic!("truncated body accepted"),
                    },
                };
                assert_eq!(error.scope, FailureScope::Request, "status={status}, sse={sse}");
                assert!(error.headers.is_empty());
                server.await.unwrap();
            }
        }
    }
}
