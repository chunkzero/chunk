//! Accepted connections a transport closes once its graceful shutdown overruns.

use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

/// How long a transport's graceful shutdown waits for its connections to drain.
pub const GRACE: Duration = Duration::from_secs(5);

/// A transport's accepted connections, closed together when its graceful shutdown overruns [`GRACE`], such as when
/// a client stopped reading and a stream's final frames never flush.
#[derive(Clone, Default)]
pub struct Connections(CancellationToken);

impl Connections {
    /// Makes `io` one of these connections.
    pub fn track<T>(&self, io: T) -> Closable<T> {
        Closable { io, closed: Box::pin(self.0.clone().cancelled_owned()) }
    }

    /// Awaits `server`, whose graceful shutdown has begun, closing the connections still open after [`GRACE`].
    /// # Errors
    /// Reports `server`'s error.
    pub async fn drain<E>(&self, transport: &str, server: impl Future<Output = Result<(), E>>) -> Result<(), E> {
        tokio::pin!(server);
        if let Ok(result) = tokio::time::timeout(GRACE, &mut server).await {
            return result;
        }
        tracing::warn!(transport, waited = ?GRACE, "connections did not drain; closing them");
        self.0.cancel();
        server.await
    }
}

/// A connection whose reads and writes fail once its [`Connections`] close.
pub struct Closable<T> {
    io: T,
    closed: Pin<Box<WaitForCancellationFutureOwned>>,
}

impl<T> Closable<T> {
    fn check(&mut self, context: &mut Context<'_>) -> io::Result<()> {
        match self.closed.as_mut().poll(context) {
            Poll::Ready(()) => Err(io::ErrorKind::ConnectionAborted.into()),
            Poll::Pending => Ok(()),
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Closable<T> {
    fn poll_read(mut self: Pin<&mut Self>, context: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        self.check(context)?;
        Pin::new(&mut self.io).poll_read(context, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Closable<T> {
    fn poll_write(mut self: Pin<&mut Self>, context: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.check(context)?;
        Pin::new(&mut self.io).poll_write(context, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.check(context)?;
        Pin::new(&mut self.io).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(context)
    }
}

impl<T: tonic::transport::server::Connected> tonic::transport::server::Connected for Closable<T> {
    type ConnectInfo = T::ConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.io.connect_info()
    }
}
