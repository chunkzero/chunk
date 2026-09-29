//! The receiving side of PROXY protocol v2, which edges use to name the player behind a connection.

use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
};

use tokio::io::{AsyncRead, AsyncReadExt};

use super::transport::invalid_data;
use crate::TrustedEdges;

const SIGNATURE: [u8; 12] = *b"\r\n\r\n\0\r\nQUIT\n";
const TCP_OVER_IPV4: u8 = 0x11;
const TCP_OVER_IPV6: u8 = 0x21;

/// The player's address: the source a trusted edge's header names, or `peer` itself. Only connections from a trusted
/// edge are read, and those must open with a header; its `LOCAL` command and non-IP families keep `peer`.
pub(super) async fn player_address<S: AsyncRead + Unpin>(
    stream: &mut S,
    peer: SocketAddr,
    trusted: &TrustedEdges,
) -> io::Result<SocketAddr> {
    if !trusted.contains(peer.ip()) {
        return Ok(peer);
    }
    let mut fixed = [0; 16];
    stream.read_exact(&mut fixed).await?;
    if fixed[..12] != SIGNATURE || fixed[12] >> 4 != 2 {
        return Err(invalid_data("an edge connection did not open with a PROXY protocol v2 header"));
    }
    let mut addresses = vec![0; usize::from(u16::from_be_bytes([fixed[14], fixed[15]]))];
    stream.read_exact(&mut addresses).await?;
    let source = |ip: IpAddr, port: &[u8]| SocketAddr::new(ip.to_canonical(), u16::from_be_bytes([port[0], port[1]]));
    match (fixed[12] & 0x0f, fixed[13]) {
        (1, TCP_OVER_IPV4) if addresses.len() >= 12 => {
            let ip: [u8; 4] = addresses[..4].try_into().expect("four bytes");
            Ok(source(Ipv4Addr::from(ip).into(), &addresses[8..10]))
        }
        (1, TCP_OVER_IPV6) if addresses.len() >= 36 => {
            let ip: [u8; 16] = addresses[..16].try_into().expect("sixteen bytes");
            Ok(source(Ipv6Addr::from(ip).into(), &addresses[32..34]))
        }
        (1, TCP_OVER_IPV4 | TCP_OVER_IPV6) => Err(invalid_data("truncated PROXY protocol v2 addresses")),
        (0 | 1, _) => Ok(peer),
        _ => Err(invalid_data("unknown PROXY protocol v2 command")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_the_player_address_only_from_trusted_edges() {
        let trusted: TrustedEdges = "10.0.0.5, 100.64.0.0/10".parse().unwrap();
        let mut connection = SIGNATURE.to_vec();
        connection.extend_from_slice(&[
            0x21,
            TCP_OVER_IPV4,
            0,
            12,
            203,
            0,
            113,
            7,
            10,
            0,
            0,
            9,
            0xc8,
            0x12,
            0x63,
            0xdd,
        ]);
        connection.extend_from_slice(b"handshake");

        for edge in ["10.0.0.5:40000", "[::ffff:100.64.1.2]:40000"] {
            let mut stream = &connection[..];
            let address = player_address(&mut stream, edge.parse().unwrap(), &trusted).await.unwrap();
            assert_eq!(address, "203.0.113.7:51218".parse().unwrap(), "{edge}");
            assert_eq!(stream, b"handshake");
        }

        let mut stream = &connection[..];
        let stranger = "10.0.0.6:40000".parse().unwrap();
        assert_eq!(player_address(&mut stream, stranger, &trusted).await.unwrap(), stranger);
        assert_eq!(stream, &connection[..], "an untrusted peer's bytes are left for the handshake");

        let mut stream = &b"\x10\x00\x88\x06\x09localhost\x63\xdd\x02"[..];
        assert!(player_address(&mut stream, "10.0.0.5:40000".parse().unwrap(), &trusted).await.is_err());
    }
}
