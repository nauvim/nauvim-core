use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

const MAX_RESPONSE_HEADERS: usize = 8192;

enum State {
    Headers,
    Pending { written: usize, body: u64 },
    Body(u64),
}

/// Keeps Hyper's automatic parser-error responses off the wire.
///
/// The file service supplies an explicit origin and an exact body length for
/// each response. Hyper's automatic responses bypass that service. Buffering
/// only their headers allows those responses to be suppressed before any bytes
/// are sent, while file contents continue directly through the transport.
pub(super) struct OriginIo<S> {
    inner: S,
    state: State,
    headers: Vec<u8>,
}

impl<S> OriginIo<S> {
    pub(super) fn new(inner: S) -> Self {
        Self {
            inner,
            state: State::Headers,
            headers: Vec::with_capacity(256),
        }
    }
}

impl<S: AsyncWrite + Unpin> OriginIo<S> {
    fn poll_headers(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            let State::Pending { written, body } = &mut self.state else {
                return Poll::Ready(Ok(()));
            };
            let Some(bytes) = self.headers.get(*written..) else {
                return Poll::Ready(Err(invalid("Invalid HTTP response header position")));
            };
            if bytes.is_empty() {
                let body = *body;
                self.headers.clear();
                self.state = if body == 0 {
                    State::Headers
                } else {
                    State::Body(body)
                };
                continue;
            }
            let n = ready!(Pin::new(&mut self.inner).poll_write(cx, bytes))?;
            if n == 0 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "Write HTTP response headers",
                )));
            }
            if n > bytes.len() {
                return Poll::Ready(Err(invalid("Invalid HTTP transport write length")));
            }
            let Some(next) = written.checked_add(n) else {
                return Poll::Ready(Err(invalid("HTTP response header position overflow")));
            };
            *written = next;
        }
    }

    fn complete_headers(&self) -> io::Result<()> {
        if matches!(self.state, State::Headers) && !self.headers.is_empty() {
            Err(invalid("Incomplete HTTP response headers"))
        } else {
            Ok(())
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for OriginIo<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for OriginIo<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(this.poll_headers(cx))?;
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        match &mut this.state {
            State::Headers => {
                let mut consumed = 0_usize;
                for &byte in buf {
                    if this.headers.len() == MAX_RESPONSE_HEADERS {
                        return Poll::Ready(Err(invalid("HTTP response headers exceed limit")));
                    }
                    this.headers.push(byte);
                    let Some(next) = consumed.checked_add(1) else {
                        return Poll::Ready(Err(invalid("HTTP response header length overflow")));
                    };
                    consumed = next;
                    if this.headers.ends_with(b"\r\n\r\n") {
                        let body = body_length(&this.headers)?;
                        this.state = State::Pending { written: 0, body };
                        break;
                    }
                }
                Poll::Ready(Ok(consumed))
            }
            State::Body(remaining) => {
                let len = usize::try_from(*remaining)
                    .unwrap_or(usize::MAX)
                    .min(buf.len());
                let Some(bytes) = buf.get(..len) else {
                    return Poll::Ready(Err(invalid("Invalid HTTP response body length")));
                };
                let n = ready!(Pin::new(&mut this.inner).poll_write(cx, bytes))?;
                let n_u64 = u64::try_from(n)
                    .map_err(|_| invalid("HTTP transport write length exceeds range"))?;
                if n > len {
                    return Poll::Ready(Err(invalid("Invalid HTTP transport write length")));
                }
                let Some(next) = remaining.checked_sub(n_u64) else {
                    return Poll::Ready(Err(invalid("HTTP response body length underflow")));
                };
                *remaining = next;
                if next == 0 {
                    this.state = State::Headers;
                }
                Poll::Ready(Ok(n))
            }
            State::Pending { .. } => {
                Poll::Ready(Err(invalid("HTTP response headers were not flushed")))
            }
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(this.poll_headers(cx))?;
        let len = bufs.iter().try_fold(0_u64, |len, bytes| {
            len.checked_add(u64::try_from(bytes.len()).ok()?)
        });
        if let State::Body(remaining) = &mut this.state
            && let Some(len) = len
            && len <= *remaining
        {
            let n = ready!(Pin::new(&mut this.inner).poll_write_vectored(cx, bufs))?;
            let n_u64 = u64::try_from(n)
                .map_err(|_| invalid("HTTP transport write length exceeds range"))?;
            if n_u64 > len {
                return Poll::Ready(Err(invalid("Invalid HTTP transport write length")));
            }
            let Some(next) = remaining.checked_sub(n_u64) else {
                return Poll::Ready(Err(invalid("HTTP response body length underflow")));
            };
            *remaining = next;
            if next == 0 {
                this.state = State::Headers;
            }
            return Poll::Ready(Ok(n));
        }
        let first = bufs.iter().find(|bytes| !bytes.is_empty());
        Pin::new(this).poll_write(cx, first.map_or(&[], |bytes| bytes.as_ref()))
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_headers(cx))?;
        this.complete_headers()?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_headers(cx))?;
        this.complete_headers()?;
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

fn body_length(headers: &[u8]) -> io::Result<u64> {
    use std::str;

    let headers =
        str::from_utf8(headers).map_err(|_| invalid("HTTP response headers are not UTF-8"))?;
    let mut lines = headers.split("\r\n");
    if !lines
        .next()
        .is_some_and(|line| line.starts_with("HTTP/1.1 ") || line.starts_with("HTTP/1.0 "))
    {
        return Err(invalid("Invalid HTTP response status line"));
    }
    let mut origin = false;
    let mut length = None;
    for line in lines.filter(|line| !line.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return Err(invalid("Invalid HTTP response header"));
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("access-control-allow-origin") {
            if origin || value.is_empty() || matches!(value, "*" | "null") {
                return Err(invalid("HTTP response has no single explicit origin"));
            }
            origin = true;
        }
        if name.eq_ignore_ascii_case("content-length") {
            let n = value
                .parse::<u64>()
                .map_err(|_| invalid("Invalid HTTP response content length"))?;
            if length.is_some_and(|previous| previous != n) {
                return Err(invalid("Conflicting HTTP response content lengths"));
            }
            length = Some(n);
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(invalid("Unexpected HTTP response transfer encoding"));
        }
    }
    if !origin {
        return Err(invalid("HTTP response has no explicit origin"));
    }
    length.ok_or_else(|| invalid("HTTP response has no exact content length"))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, duplex};
    use tokio::spawn;
    use tokio::time::timeout;

    use std::time::Duration;

    #[tokio::test]
    async fn retains_headers_and_bodies_across_partial_writes_and_persistent_responses() {
        timeout(Duration::from_secs(5), async {
            let body = b"\r\n\r\nHTTP/1.1\r\n\r\n";
            let mut expected = format!(
                "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: https://127.0.0.1:1234\r\nContent-Length: {}\r\n\r\n",
                body.len(),
            ).into_bytes();
            expected.extend_from_slice(body);
            expected.extend_from_slice(b"HTTP/1.1 405 Method Not Allowed\r\naccess-control-allow-origin: https://127.0.0.1:1234\r\ncontent-length: 0\r\n\r\n");
            for (size, vectored) in [(1, false), (2, false), (7, false), (64, false), (1024, false), (3, true), (64, true)] {
                let (mut client, server) = duplex(1);
                let input = expected.clone();
                let sender = spawn(async move {
                    let mut server = OriginIo::new(server);
                    if vectored {
                        let mut slices: Vec<_> = input.chunks(size).map(io::IoSlice::new).collect();
                        let mut remaining = slices.as_mut_slice();
                        while !remaining.is_empty() {
                            let n = server.write_vectored(remaining).await.unwrap();
                            assert_ne!(n, 0);
                            io::IoSlice::advance_slices(&mut remaining, n);
                        }
                    } else {
                        for bytes in input.chunks(size) {
                            server.write_all(bytes).await.unwrap();
                        }
                    }
                    server.shutdown().await.unwrap();
                });
                let mut actual = Vec::new();
                client.read_to_end(&mut actual).await.unwrap();
                sender.await.unwrap();
                assert_eq!(actual, expected, "chunk size {size}, vectored {vectored}");
            }
        }).await.unwrap();
    }

    #[test]
    fn rejects_automatic_errors_and_invalid_response_framing() {
        for headers in [
            "HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\naccess-control-allow-origin: *\r\ncontent-length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\naccess-control-allow-origin: null\r\ncontent-length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\naccess-control-allow-origin: https://127.0.0.1:1234\r\ntransfer-encoding: chunked\r\n\r\n",
            "HTTP/1.1 200 OK\r\naccess-control-allow-origin: https://127.0.0.1:1234\r\ncontent-length: 0\r\ncontent-length: 1\r\n\r\n",
        ] {
            assert_eq!(
                body_length(headers.as_bytes()).unwrap_err().kind(),
                io::ErrorKind::InvalidData,
                "{headers:?}"
            );
        }
    }
}
