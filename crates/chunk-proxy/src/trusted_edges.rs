use std::{net::IpAddr, str::FromStr, sync::Arc};

use ipnet::{IpNet, Ipv4Net};

/// The addresses of edges whose connections open with a PROXY protocol v2 header naming the player's address.
/// Connections from other addresses are never parsed for one.
#[derive(Debug, Clone, Default)]
pub struct TrustedEdges(Arc<[IpNet]>);

impl TrustedEdges {
    /// Whether connections from `address` come from an edge.
    #[must_use]
    pub fn contains(&self, address: IpAddr) -> bool {
        let address = address.to_canonical();
        self.0.iter().any(|network| network.contains(&address))
    }
}

/// Parses a comma-separated list of IP addresses and CIDR networks, such as `10.0.0.5, 100.64.0.0/10`.
impl FromStr for TrustedEdges {
    type Err = ipnet::AddrParseError;

    fn from_str(list: &str) -> Result<Self, Self::Err> {
        list.split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(network)
            .collect::<Result<Vec<_>, _>>()
            .map(|networks| Self(networks.into()))
    }
}

/// `entry` as a network, with IPv4-mapped IPv6 addresses and networks as IPv4 ones, the form peers are compared in.
fn network(entry: &str) -> Result<IpNet, ipnet::AddrParseError> {
    let network = entry.parse::<IpAddr>().map(IpNet::from).or_else(|_| entry.parse())?;
    if let IpNet::V6(v6) = network
        && let Some(prefix) = v6.prefix_len().checked_sub(96)
        && let Some(v4) = v6.addr().to_ipv4_mapped()
    {
        return Ok(Ipv4Net::new(v4, prefix).expect("a mapped prefix is at most 32 bits").into());
    }
    Ok(network)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_ipv4_mapped_entries_to_either_form_of_peer() {
        let trusted: TrustedEdges = "::ffff:10.0.0.5, ::ffff:100.64.0.0/106".parse().unwrap();
        for peer in ["10.0.0.5", "::ffff:10.0.0.5", "100.127.1.2", "::ffff:100.127.1.2"] {
            assert!(trusted.contains(peer.parse().unwrap()), "{peer}");
        }
        for peer in ["10.0.0.6", "::ffff:10.0.0.6", "100.128.0.1", "::10.0.0.5"] {
            assert!(!trusted.contains(peer.parse().unwrap()), "{peer}");
        }
    }
}
