use bytes::Bytes;
use cpa_core::exec::{ExecError, FailureScope};
use futures_util::StreamExt;
use futures_util::stream::BoxStream;

/// `bufio.Scanner` with `ScanLines` and `Buffer(nil, max)`: one item per line without
/// its `\n`, a trailing `\r` dropped, the final unterminated line included, and
/// `bufio.ErrTooLong` when no newline falls within `max` bytes of a line's start (the
/// scanner's buffer never holds more). An I/O error ends the stream after the line
/// buffered before it, as `Scan` returns that last token before reporting `Err`.
// ponytail: the body's end (or error) is taken to arrive with its last bytes, as net/http
// returns EOF for a Content-Length body and gzip.Reader at its end; the stream cannot
// tell. When Go's reader reports the end on a read of its own, Go rejects an
// unterminated final line of exactly `max` bytes that this accepts.
pub fn lines(
    body: BoxStream<'static, Result<Bytes, ExecError>>,
    max: usize,
) -> BoxStream<'static, Result<Bytes, ExecError>> {
    struct State {
        body: BoxStream<'static, Result<Bytes, ExecError>>,
        /// Unconsumed input from the current line's start.
        buf: bytes::BytesMut,
        /// Bytes of `buf` already searched for a newline.
        scanned: usize,
        /// The reader ended (EOF or the error below).
        ended: bool,
        error: Option<ExecError>,
        finished: bool,
    }
    let drop_cr = |line: &[u8]| Bytes::copy_from_slice(line.strip_suffix(b"\r").unwrap_or(line));
    futures_util::stream::unfold(
        State {
            body,
            buf: bytes::BytesMut::new(),
            scanned: 0,
            ended: false,
            error: None,
            finished: false,
        },
        move |mut st| async move {
            if st.finished {
                return None;
            }
            loop {
                let window = st.buf.len().min(max);
                if let Some(pos) = st.buf[st.scanned.min(window)..window].iter().position(|b| *b == b'\n') {
                    let at = st.scanned.min(window) + pos;
                    let line = st.buf.split_to(at + 1);
                    st.scanned = 0;
                    return Some((Ok(drop_cr(&line[..at])), st));
                }
                st.scanned = window;
                // A full buffer without a newline: more data is too long, while the end
                // of the body makes it the final line.
                if st.buf.len() > max {
                    st.finished = true;
                    let error = ExecError::local(500, FailureScope::Request, "bufio.Scanner: token too long");
                    return Some((Err(error), st));
                }
                if st.ended {
                    if !st.buf.is_empty() {
                        let line = st.buf.split();
                        st.scanned = 0;
                        return Some((Ok(drop_cr(&line)), st));
                    }
                    st.finished = true;
                    return st.error.take().map(|error| (Err(error), st));
                }
                match st.body.next().await {
                    Some(Ok(chunk)) => st.buf.extend_from_slice(&chunk),
                    Some(Err(error)) => {
                        st.ended = true;
                        st.error = Some(error);
                    }
                    None => st.ended = true,
                }
            }
        },
    )
    .boxed()
}
