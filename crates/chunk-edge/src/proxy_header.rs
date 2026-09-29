//! PROXY protocol v2 headers, which tell a gateway the player's address.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};

const SIGNATURE: [u8; 12] = *b"\r\n\r\n\0\r\nQUIT\n";
/// Version 2, `PROXY` command.
const PROXY: u8 = 0x21;
const TCP_OVER_IPV4: u8 = 0x11;
const TCP_OVER_IPV6: u8 = 0x21;

/// A header for a TCP connection from `source` to `destination`. Mixed families are sent as IPv6, with IPv4
/// addresses mapped.
pub(crate) fn v2(source: SocketAddr, destination: SocketAddr) -> Vec<u8> {
    let mut header = SIGNATURE.to_vec();
    header.push(PROXY);
    match (source.ip().to_canonical(), destination.ip().to_canonical()) {
        (IpAddr::V4(from), IpAddr::V4(to)) => {
            header.extend_from_slice(&[TCP_OVER_IPV4, 0, 12]);
            header.extend_from_slice(&from.octets());
            header.extend_from_slice(&to.octets());
        }
        (from, to) => {
            header.extend_from_slice(&[TCP_OVER_IPV6, 0, 36]);
            header.extend_from_slice(&ipv6(from).octets());
            header.extend_from_slice(&ipv6(to).octets());
        }
    }
    header.extend_from_slice(&source.port().to_be_bytes());
    header.extend_from_slice(&destination.port().to_be_bytes());
    header
}

fn ipv6(address: IpAddr) -> Ipv6Addr {
    match address {
        IpAddr::V4(address) => address.to_ipv6_mapped(),
        IpAddr::V6(address) => address,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_mixed_families_to_ipv6() {
        let header = v2("203.0.113.7:51218".parse().unwrap(), "[2001:db8::1]:25565".parse().unwrap());
        assert_eq!(header.len(), 16 + 36);
        assert_eq!(header[12..16], [PROXY, TCP_OVER_IPV6, 0, 36]);
        assert_eq!(header[16..32], "::ffff:203.0.113.7".parse::<Ipv6Addr>().unwrap().octets());
        assert_eq!(header[48..], [0xc8, 0x12, 0x63, 0xdd]);
    }
}
