//! One player's connection: route it by its handshake, then splice it to a gateway.

use std::{io, net::SocketAddr, time::Duration};

use tokio::{io::AsyncWriteExt, net::TcpStream, time::timeout};

use crate::{
    handshake::{self, Hello},
    proxy_header,
    routes::Routes,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) async fn serve(client: TcpStream, peer: SocketAddr, routes: Routes, handshake_timeout: Duration) {
    if let Err(error) = forward(client, peer, &routes, handshake_timeout).await {
        tracing::debug!(%peer, %error, "connection closed");
    }
}

async fn forward(
    mut client: TcpStream,
    peer: SocketAddr,
    routes: &Routes,
    handshake_timeout: Duration,
) -> io::Result<()> {
    let (hello, replay) = timeout(handshake_timeout, handshake::read(&mut client))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no handshake in time"))??;
    let hostname = match hello {
        Hello::Handshake { hostname } => hostname,
        Hello::LegacyPing => return Err(io::Error::other("a legacy ping, which gateways do not serve")),
    };
    let gateways = routes.gateways(&hostname).ok_or_else(|| io::Error::other(format!("no route for {hostname:?}")))?;
    let mut gateway = connect(&gateways, peer).await?;
    let mut preamble = proxy_header::v2(peer, client.local_addr()?);
    preamble.extend_from_slice(&replay);
    gateway.write_all(&preamble).await?;
    _ = client.set_nodelay(true);
    _ = gateway.set_nodelay(true);
    tokio::io::copy_bidirectional(&mut client, &mut gateway).await?;
    Ok(())
}

/// Connects to one of `gateways`, starting from one picked by the player's port and trying the rest in turn.
async fn connect(gateways: &[SocketAddr], peer: SocketAddr) -> io::Result<TcpStream> {
    let start = usize::from(peer.port()) % gateways.len().max(1);
    let mut failure = io::Error::other("no gateway is ready");
    for gateway in gateways[start..].iter().chain(&gateways[..start]) {
        failure = match timeout(CONNECT_TIMEOUT, TcpStream::connect(gateway)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(error)) => error,
            Err(_) => io::Error::new(io::ErrorKind::TimedOut, "gateway connect timed out"),
        };
        tracing::warn!(%gateway, error = %failure, "gateway unreachable");
    }
    Err(failure)
}
