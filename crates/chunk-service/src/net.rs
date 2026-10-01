//! Address policy for traffic between chunk processes, and for traffic backend code sends out.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// AWS's and GCP's IPv6 instance metadata services, which sit inside `fc00::/7`.
const METADATA: [Ipv6Addr; 2] =
    [Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254), Ipv6Addr::new(0xfd20, 0xce, 0, 0, 0, 0, 0, 0x254)];

/// Whether `address` is loopback or in `10/8`, `172.16/12`, `192.168/16`, `100.64/10` or `fc00::/7`.
/// IPv4-mapped IPv6 addresses classify like their IPv4 address. Link-local addresses (`169.254/16`,
/// `fe80::/10`) and cloud metadata addresses (`fd00:ec2::254`, `fd20:ce::254`) are not private.
#[must_use]
pub fn private(address: IpAddr) -> bool {
    match address.to_canonical() {
        IpAddr::V4(address) => {
            let [a, b, ..] = address.octets();
            address.is_loopback() || address.is_private() || (a == 100 && b & 0xc0 == 64)
        }
        IpAddr::V6(address) => address.is_loopback() || (address.is_unique_local() && !METADATA.contains(&address)),
    }
}

/// Whether `address` may be reached from backend code: not loopback, unspecified, private, CGNAT, link-local, cloud
/// metadata, unique-local, multicast, broadcast or reserved. IPv6 addresses that embed an IPv4 address (mapped,
/// compatible, translated, NAT64 and 6to4) classify like it, and Teredo addresses, which obscure theirs, are refused.
#[must_use]
pub fn public(address: IpAddr) -> bool {
    match address.to_canonical() {
        IpAddr::V4(address) => public_v4(address),
        IpAddr::V6(address) => {
            let segments = address.segments();
            let embedded = |high: usize| {
                let [a, b] = segments[high].to_be_bytes();
                let [c, d] = segments[high + 1].to_be_bytes();
                Ipv4Addr::new(a, b, c, d)
            };
            match segments {
                // The well-known NAT64 prefix, IPv4-compatible (`::` and `::1` included) and IPv4-translated.
                [0x64, 0xff9b, 0, 0, 0, 0, ..] | [0, 0, 0, 0, 0 | 0xffff, 0, ..] => public_v4(embedded(6)),
                // 6to4.
                [0x2002, ..] => public_v4(embedded(1)),
                // The local-use NAT64 prefix, and Teredo.
                [0x64, 0xff9b, 1, ..] | [0x2001, 0, ..] => false,
                _ => {
                    !(address.is_loopback()
                        || address.is_unspecified()
                        || address.is_multicast()
                        || address.is_unique_local()
                        || address.is_unicast_link_local()
                        // Deprecated site-local.
                        || segments[0] & 0xffc0 == 0xfec0)
                }
            }
        }
    }
}

fn public_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    // Azure's WireServer, which serves instance metadata and credentials outside link-local space.
    !(address == Ipv4Addr::new(168, 63, 129, 16)
        || a == 0
        || address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || (a == 100 && b & 0xc0 == 64)
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && b & 0xfe == 18)
        || a >= 224)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_covers_loopback_and_private_ranges_only() {
        for (address, expected) in [
            ("127.0.0.1", true),
            ("127.255.255.254", true),
            ("10.0.0.1", true),
            ("10.255.255.255", true),
            ("172.16.0.1", true),
            ("172.31.255.255", true),
            ("172.15.255.255", false),
            ("172.32.0.1", false),
            ("192.168.1.1", true),
            ("192.169.0.1", false),
            ("100.64.0.1", true),
            ("100.127.255.255", true),
            ("100.63.255.255", false),
            ("100.128.0.1", false),
            ("169.254.169.254", false),
            ("0.0.0.0", false),
            ("224.0.0.1", false),
            ("255.255.255.255", false),
            ("8.8.8.8", false),
            ("203.0.113.1", false),
            ("::1", true),
            ("fc00::1", true),
            ("fdaa::1", true),
            ("fdff:ffff::1", true),
            ("fe80::1", false),
            ("fd00:ec2::254", false),
            ("fd20:ce::254", false),
            ("fd00:ec2::253", true),
            ("::", false),
            ("ff02::1", false),
            ("2001:db8::1", false),
            ("2606:4700::1111", false),
            ("::ffff:127.0.0.1", true),
            ("::ffff:10.1.2.3", true),
            ("::ffff:100.64.0.1", true),
            ("::ffff:169.254.169.254", false),
            ("::ffff:8.8.8.8", false),
        ] {
            assert_eq!(private(address.parse().unwrap()), expected, "{address}");
        }
    }

    #[test]
    fn public_refuses_internal_ipv4_ranges_in_every_ipv6_form() {
        let refused = [
            "0.0.0.0",
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "100.64.0.1",
            "169.254.169.254",
            "168.63.129.16",
            "192.0.0.1",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
        ];
        for v4 in refused {
            let address: Ipv4Addr = v4.parse().unwrap();
            let [a, b, c, d] = address.octets();
            let (high, low) = (u16::from_be_bytes([a, b]), u16::from_be_bytes([c, d]));
            let forms = [
                IpAddr::V4(address),
                IpAddr::V6(address.to_ipv6_mapped()),
                IpAddr::V6(address.to_ipv6_compatible()),
                IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0xffff, 0, high, low)),
                IpAddr::V6(Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, high, low)),
                IpAddr::V6(Ipv6Addr::new(0x2002, high, low, 0, 0, 0, 0, 1)),
            ];
            for form in forms {
                assert!(!public(form), "{form} embeds {v4}");
            }
        }
        for address in ["::", "::1", "fe80::1", "fc00::1", "fd00:ec2::254", "ff02::1", "64:ff9b:1::1", "2001:0:4136::1"]
        {
            assert!(!public(address.parse().unwrap()), "{address}");
        }
        for address in
            ["8.8.8.8", "168.63.129.17", "::ffff:8.8.8.8", "64:ff9b::808:808", "2002:808:808::1", "2606:4700::1111"]
        {
            assert!(public(address.parse().unwrap()), "{address}");
        }
    }
}
