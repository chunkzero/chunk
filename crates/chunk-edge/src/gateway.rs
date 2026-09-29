//! Reaching an environment's gateways.

use std::{io, net::SocketAddr, time::Duration};

use tokio::{net::TcpStream, time::timeout};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Connects to one of `gateways`, starting from one picked by the player's port and trying the rest in turn.
pub(crate) async fn connect(gateways: &[SocketAddr], peer: SocketAddr) -> io::Result<TcpStream> {
    let start = usize::from(peer.port()) % gateways.len().max(1);
    let mut failure = io::Error::other("no gateway is ready");
    for gateway in gateways[start..].iter().chain(&gateways[..start]) {
        failure = match timeout(CONNECT_TIMEOUT, TcpStream::connect(gateway)).await {
            Ok(Ok(stream)) => {
                _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Ok(Err(error)) => error,
            Err(_) => io::Error::new(io::ErrorKind::TimedOut, "gateway connect timed out"),
        };
        tracing::warn!(%gateway, error = %failure, "gateway unreachable");
    }
    Err(failure)
}

#[cfg(test)]
mod tests {
    use tokio::net::TcpListener;

    use super::*;

    #[tokio::test]
    async fn fails_over_to_the_next_gateway() {
        let refused = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        let gateway = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateways = [refused, gateway.local_addr().unwrap()];
        // An even port starts at the first gateway.
        let stream = connect(&gateways, "203.0.113.7:51218".parse().unwrap()).await.unwrap();
        assert_eq!(stream.peer_addr().unwrap(), gateways[1]);
    }
}
