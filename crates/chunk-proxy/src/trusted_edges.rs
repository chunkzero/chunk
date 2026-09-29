use std::{net::IpAddr, str::FromStr, sync::Arc};

use ipnet::IpNet;

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
            .map(|entry| entry.parse::<IpAddr>().map(IpNet::from).or_else(|_| entry.parse()))
            .collect::<Result<Vec<_>, _>>()
            .map(|networks| Self(networks.into()))
    }
}
