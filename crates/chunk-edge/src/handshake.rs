//! The first bytes a Minecraft client sends: a handshake naming the hostname it dialled, or a pre-1.7 ping.

use std::io;

use bytes::BytesMut;
use chunk_protocol::{Decode, McString, VarInt, decode_frame};
use tokio::io::{AsyncRead, AsyncReadExt};

/// Pre-1.7 clients open a server-list ping with this byte. No modern handshake frame starts with it at this size.
const LEGACY_PING: u8 = 0xfe;
/// Room for a handshake whose server address is at vanilla's 255-character limit.
const MAX_HANDSHAKE: usize = 1024;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Hello {
    /// A modern handshake, with its hostname normalised for routing.
    Handshake {
        hostname: String,
    },
    LegacyPing,
}

/// Reads until `stream` has sent its handshake, returning it with every byte read, which may run past it.
pub(crate) async fn read<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<(Hello, Vec<u8>)> {
    let mut buffer = Vec::with_capacity(512);
    loop {
        if stream.read_buf(&mut buffer).await? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if let Some(hello) = parse(&buffer)? {
            return Ok((hello, buffer));
        }
    }
}

/// What `buffer` opens with, or None while it holds only part of a handshake.
fn parse(buffer: &[u8]) -> io::Result<Option<Hello>> {
    if buffer.first() == Some(&LEGACY_PING) {
        return Ok(Some(Hello::LegacyPing));
    }
    let Some(frame) = decode_frame(&mut BytesMut::from(buffer), MAX_HANDSHAKE).map_err(invalid)? else {
        return Ok(None);
    };
    let mut input = &frame[..];
    if VarInt::decode(&mut input).map_err(invalid)?.0 != 0 {
        return Err(invalid("the first packet is not a handshake"));
    }
    VarInt::decode(&mut input).map_err(invalid)?;
    let address = McString::<255>::decode(&mut input).map_err(invalid)?;
    Ok(Some(Hello::Handshake { hostname: normalize(address.as_str()) }))
}

/// The hostname a handshake's server address routes by: lowercase, without anything from a NUL on (which Forge and
/// forwarding setups append), a port or a trailing dot.
fn normalize(address: &str) -> String {
    let host = address.split('\0').next().unwrap_or_default();
    let host = match host.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') && !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            name
        }
        _ => host,
    };
    host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase()
}

fn invalid(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_hostnames() {
        for (address, hostname) in [
            ("play.example.com", "play.example.com"),
            ("Play.Example.COM.", "play.example.com"),
            ("play.example.com:25565", "play.example.com"),
            ("play.example.com.\0FML3\0", "play.example.com"),
            ("PLAY.example.com\x00203.0.113.7\x00uuid", "play.example.com"),
            ("play.example.com.:25565\0FML2\0", "play.example.com"),
            ("2001:db8::1", "2001:db8::1"),
            ("", ""),
        ] {
            assert_eq!(normalize(address), hostname, "{address:?}");
        }
    }

    #[test]
    fn parses_modern_and_legacy_openings() {
        let handshake = b"\x19\x00\x88\x06\x12Play.Example.com.\0\x63\xdd\x02";
        for end in 0..handshake.len() {
            assert_eq!(parse(&handshake[..end]).unwrap(), None, "{end} bytes");
        }
        let hostname = "play.example.com".to_owned();
        assert_eq!(parse(handshake).unwrap(), Some(Hello::Handshake { hostname: hostname.clone() }));
        let pipelined = [&handshake[..], b"\x06\x00\x04Alex"].concat();
        assert_eq!(parse(&pipelined).unwrap(), Some(Hello::Handshake { hostname }));

        for legacy in [&b"\xfe"[..], b"\xfe\x01", b"\xfe\x01\xfa\x00\x0b"] {
            assert_eq!(parse(legacy).unwrap(), Some(Hello::LegacyPing));
        }

        assert!(parse(b"\x02\x01\x00").is_err(), "a non-handshake packet");
        assert!(parse(b"\xff\x7f").is_err(), "a frame beyond the handshake limit");
    }
}
