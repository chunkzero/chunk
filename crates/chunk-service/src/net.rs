//! Address policy for traffic between chunk processes.

use std::net::IpAddr;

/// Whether `address` is loopback or in `10/8`, `172.16/12`, `192.168/16`, `100.64/10` or `fc00::/7`.
/// IPv4-mapped IPv6 addresses classify like their IPv4 address. Link-local addresses (`169.254/16`,
/// `fe80::/10`) are not private.
#[must_use]
pub fn private(address: IpAddr) -> bool {
    match address.to_canonical() {
        IpAddr::V4(address) => {
            let [a, b, ..] = address.octets();
            address.is_loopback() || address.is_private() || (a == 100 && b & 0xc0 == 64)
        }
        IpAddr::V6(address) => address.is_loopback() || address.is_unique_local(),
    }
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
}
