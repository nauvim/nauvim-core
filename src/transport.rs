#[cfg(feature = "ssh")]
use openrussh_client::russh::{ChannelStream, client::Msg};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UnixStream;

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

/// The concrete local or SSH stream of a Nauvim RPC attachment.
///
/// Variants are internal; no dynamic I/O dispatch is used.
pub struct Transport(Inner);

enum Inner {
    Local(UnixStream),
    #[cfg(feature = "ssh")]
    Remote(ChannelStream<Msg>),
}

impl Transport {
    pub(crate) fn local(stream: UnixStream) -> Self {
        Self(Inner::Local(stream))
    }
    #[cfg(feature = "ssh")]
    pub(crate) fn remote(stream: ChannelStream<Msg>) -> Self {
        Self(Inner::Remote(stream))
    }
}

impl AsyncRead for Transport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut self.get_mut().0 {
            Inner::Local(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(feature = "ssh")]
            Inner::Remote(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Transport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut self.get_mut().0 {
            Inner::Local(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(feature = "ssh")]
            Inner::Remote(s) => Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.get_mut().0 {
            Inner::Local(s) => Pin::new(s).poll_flush(cx),
            #[cfg(feature = "ssh")]
            Inner::Remote(s) => Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.get_mut().0 {
            Inner::Local(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(feature = "ssh")]
            Inner::Remote(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}
