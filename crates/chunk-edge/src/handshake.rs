//! The first bytes a Minecraft client sends: a handshake naming the hostname it dialled, or a pre-1.7 ping.

use std::{io, time::Duration};

use bytes::BytesMut;
use chunk_protocol::{Decode, McString, VarInt, decode_frame};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    time::{Instant, timeout_at},
};

/// Pre-1.7 pings open with `FE`; 1.4 and 1.5 add `01`, and 1.6 then sends a plugin message, `FA`. A modern frame's
/// length prefix can also begin `FE 01`, but the handshake's packet ID, 0, follows it.
const LEGACY_PING: u8 = 0xfe;
const LEGACY_PING_PAYLOAD: u8 = 0x01;
const LEGACY_PLUGIN_MESSAGE: u8 = 0xfa;
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

/// Reads until `stream` has sent its handshake, returning it with every byte read, which may run past it. A client
/// that sent only `FE` or `FE 01` when `within` runs out is a pre-1.6 ping.
pub(crate) async fn read<S: AsyncRead + Unpin>(stream: &mut S, within: Duration) -> io::Result<(Hello, Vec<u8>)> {
    let deadline = Instant::now() + within;
    let mut buffer = Vec::with_capacity(512);
    loop {
        match timeout_at(deadline, stream.read_buf(&mut buffer)).await {
            Ok(Ok(0)) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(read) => {
                read?;
            }
            Err(_) if matches!(buffer[..], [LEGACY_PING] | [LEGACY_PING, LEGACY_PING_PAYLOAD]) => {
                return Ok((Hello::LegacyPing, buffer));
            }
            Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, "no handshake in time")),
        }
        if let Some(hello) = parse(&buffer)? {
            return Ok((hello, buffer));
        }
    }
}

/// What `buffer` opens with, or None while it holds only part of a handshake.
fn parse(buffer: &[u8]) -> io::Result<Option<Hello>> {
    if let [LEGACY_PING, LEGACY_PING_PAYLOAD, LEGACY_PLUGIN_MESSAGE, ..] = buffer {
        return Ok(Some(Hello::LegacyPing));
    }
    let Some(frame) = decode_frame(&mut BytesMut::from(buffer), MAX_HANDSHAKE).map_err(invalid)? else {
        return Ok(None);
    };
    let hostname = hostname(&frame)?;
    Ok(Some(Hello::Handshake { hostname }))
}

/// The routing hostname of a whole handshake packet: its ID, protocol version, server address, port and intent, with
/// nothing after.
fn hostname(mut packet: &[u8]) -> io::Result<String> {
    let input = &mut packet;
    if VarInt::decode(input).map_err(invalid)?.0 != 0 {
        return Err(invalid("the first packet is not a handshake"));
    }
    VarInt::decode(input).map_err(invalid)?;
    let address = McString::<255>::decode(input).map_err(invalid)?;
    u16::decode(input).map_err(invalid)?;
    if !(1..=3).contains(&VarInt::decode(input).map_err(invalid)?.0) {
        return Err(invalid("an unknown handshake intent"));
    }
    if !input.is_empty() {
        return Err(invalid("trailing bytes in the handshake"));
    }
    Ok(normalize(address.as_str()))
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
    use chunk_protocol::Encode;
    use tokio::io::AsyncWriteExt;

    use super::*;

    /// A framed handshake packet whose body is `address`, then `rest`.
    fn handshake(address: &str, rest: &[u8]) -> Vec<u8> {
        let mut packet = vec![0, 0x88, 0x06];
        McString::<255>::new(address).unwrap().encode(&mut packet).unwrap();
        packet.extend_from_slice(rest);
        let mut frame = Vec::new();
        VarInt(i32::try_from(packet.len()).unwrap()).encode(&mut frame).unwrap();
        frame.extend_from_slice(&packet);
        frame
    }

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
    fn parses_whole_handshakes_complete_and_fragmented() {
        let long = format!("{}.example.com", "a".repeat(234));
        let long_handshake = handshake(&long, b"\x63\xdd\x02");
        assert_eq!(long_handshake[..3], [LEGACY_PING, LEGACY_PING_PAYLOAD, 0], "a length prefix like a legacy ping's");
        for (handshake, hostname) in
            [(handshake("Play.Example.com.\0", b"\x63\xdd\x02"), "play.example.com"), (long_handshake, &long)]
        {
            for end in 0..handshake.len() {
                assert_eq!(parse(&handshake[..end]).unwrap(), None, "{end} bytes");
            }
            let expected = Some(Hello::Handshake { hostname: hostname.to_owned() });
            assert_eq!(parse(&handshake).unwrap(), expected);
            let pipelined = [&handshake[..], b"\x06\x00\x04Alex"].concat();
            assert_eq!(parse(&pipelined).unwrap(), expected);
        }

        for (malformed, why) in [
            (&b"\x02\x01\x00"[..], "a non-handshake packet"),
            (b"\xff\x7f", "a frame beyond the handshake limit"),
            (&handshake("play.example.com", b"")[..], "no port or intent"),
            (&handshake("play.example.com", b"\x63\xdd")[..], "no intent"),
            (&handshake("play.example.com", b"\x63\xdd\x04")[..], "an unknown intent"),
            (&handshake("play.example.com", b"\x63\xdd\x02\x00")[..], "trailing bytes"),
        ] {
            assert!(parse(malformed).is_err(), "{why}");
        }
    }

    #[tokio::test]
    async fn tells_legacy_pings_from_modern_frames() {
        assert_eq!(parse(b"\xfe\x01\xfa\x00\x0b").unwrap(), Some(Hello::LegacyPing));
        for (opening, legacy) in [(&b"\xfe"[..], true), (b"\xfe\x01", true), (b"\xfe\x01\x00", false)] {
            let (mut client, mut edge) = tokio::io::duplex(64);
            client.write_all(opening).await.unwrap();
            let read = read(&mut edge, Duration::from_millis(20)).await;
            if legacy {
                assert_eq!(read.unwrap(), (Hello::LegacyPing, opening.to_vec()));
            } else {
                assert_eq!(read.unwrap_err().kind(), io::ErrorKind::TimedOut);
            }
        }
    }
}
