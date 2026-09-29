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
/// The channel of a 1.6 ping's plugin message, which carries the hostname.
const PING_HOST: &str = "MC|PingHost";
/// Room for a handshake whose server address is at vanilla's 255-character limit.
const MAX_HANDSHAKE: usize = 1024;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Hello {
    Handshake(Handshake),
    /// A pre-1.7 server-list ping, with the normalised hostname a 1.6 client names. Older clients name none.
    LegacyPing {
        hostname: Option<String>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Handshake {
    pub protocol: i32,
    /// Normalised for routing.
    pub hostname: String,
    pub port: u16,
    pub intent: Intent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Intent {
    Status,
    Login,
    Transfer,
}

/// What a client opened with.
pub(crate) struct Opening {
    pub hello: Hello,
    /// Every byte read, which may run past the handshake.
    pub bytes: Vec<u8>,
    /// How many of `bytes` the handshake took.
    pub length: usize,
}

/// Reads until `stream` has sent its handshake. A client that sent only `FE` or `FE 01` when `within` runs out is a
/// pre-1.6 ping.
pub(crate) async fn read<S: AsyncRead + Unpin>(stream: &mut S, within: Duration) -> io::Result<Opening> {
    let deadline = Instant::now() + within;
    let mut bytes = Vec::with_capacity(512);
    loop {
        match timeout_at(deadline, stream.read_buf(&mut bytes)).await {
            Ok(Ok(0)) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(read) => {
                read?;
            }
            Err(_) if matches!(bytes[..], [LEGACY_PING] | [LEGACY_PING, LEGACY_PING_PAYLOAD]) => {
                let length = bytes.len();
                return Ok(Opening { hello: Hello::LegacyPing { hostname: None }, bytes, length });
            }
            Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, "no handshake in time")),
        }
        if let Some((hello, length)) = parse(&bytes)? {
            return Ok(Opening { hello, bytes, length });
        }
    }
}

/// What `buffer` opens with and its length, or None while it holds only part of it.
fn parse(buffer: &[u8]) -> io::Result<Option<(Hello, usize)>> {
    if let [LEGACY_PING, LEGACY_PING_PAYLOAD, LEGACY_PLUGIN_MESSAGE, rest @ ..] = buffer {
        return Ok(legacy(rest)?.map(|(hostname, length)| (Hello::LegacyPing { hostname }, 3 + length)));
    }
    let mut input = BytesMut::from(buffer);
    let Some(frame) = decode_frame(&mut input, MAX_HANDSHAKE).map_err(invalid)? else {
        return Ok(None);
    };
    Ok(Some((Hello::Handshake(handshake(&frame)?), buffer.len() - input.len())))
}

/// A whole handshake packet: its ID, protocol version, server address, port and intent, with nothing after.
fn handshake(mut packet: &[u8]) -> io::Result<Handshake> {
    let input = &mut packet;
    if VarInt::decode(input).map_err(invalid)?.0 != 0 {
        return Err(invalid("the first packet is not a handshake"));
    }
    let protocol = VarInt::decode(input).map_err(invalid)?.0;
    let address = McString::<255>::decode(input).map_err(invalid)?;
    let port = u16::decode(input).map_err(invalid)?;
    let intent = match VarInt::decode(input).map_err(invalid)?.0 {
        1 => Intent::Status,
        2 => Intent::Login,
        3 => Intent::Transfer,
        _ => return Err(invalid("an unknown handshake intent")),
    };
    if !input.is_empty() {
        return Err(invalid("trailing bytes in the handshake"));
    }
    Ok(Handshake { protocol, hostname: normalize(address.as_str()), port, intent })
}

/// A 1.6 ping's plugin message after its `FA`, as the hostname it names and the message's length, or None while it is
/// incomplete. A message on another channel, or whose data doesn't parse, names no hostname.
fn legacy(message: &[u8]) -> io::Result<Option<(Option<String>, usize)>> {
    let Some(channel_length) = u16_at(message, 0) else { return Ok(None) };
    let data_at = 2 + 2 * usize::from(channel_length);
    let Some(data_length) = u16_at(message, data_at) else { return Ok(None) };
    let length = data_at + 2 + usize::from(data_length);
    if 3 + length > MAX_HANDSHAKE {
        return Err(invalid("a legacy ping beyond the handshake limit"));
    }
    let Some(data) = message.get(data_at + 2..length) else { return Ok(None) };
    let hostname = (utf16(&message[2..data_at]).as_deref() == Some(PING_HOST))
        .then(|| {
            // The protocol version, then the hostname and a 4-byte port.
            let hostname_length = 2 * usize::from(u16_at(data, 1)?);
            let hostname = utf16(data.get(3..3 + hostname_length)?)?;
            (data.len() == 3 + hostname_length + 4).then(|| normalize(&hostname))
        })
        .flatten();
    Ok(Some((hostname, length)))
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn utf16(bytes: &[u8]) -> Option<String> {
    let (units, []) = bytes.as_chunks::<2>() else { return None };
    String::from_utf16(&units.iter().map(|unit| u16::from_be_bytes(*unit)).collect::<Vec<_>>()).ok()
}

/// The hostname a handshake's server address routes by: lowercase, without anything from a NUL on (which Forge and
/// forwarding setups append), a port or a trailing dot.
pub(crate) fn normalize(address: &str) -> String {
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

    fn utf16_string(value: &str) -> Vec<u8> {
        let units: Vec<u16> = value.encode_utf16().collect();
        let mut bytes = u16::try_from(units.len()).unwrap().to_be_bytes().to_vec();
        bytes.extend(units.iter().flat_map(|unit| unit.to_be_bytes()));
        bytes
    }

    /// A 1.6 ping naming `hostname` on `channel`.
    fn legacy_ping(channel: &str, hostname: &str) -> Vec<u8> {
        let mut data = vec![78];
        data.extend(utf16_string(hostname));
        data.extend_from_slice(&25565_i32.to_be_bytes());
        let mut ping = vec![LEGACY_PING, LEGACY_PING_PAYLOAD, LEGACY_PLUGIN_MESSAGE];
        ping.extend(utf16_string(channel));
        ping.extend_from_slice(&u16::try_from(data.len()).unwrap().to_be_bytes());
        ping.extend(data);
        ping
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
        for (handshake, hostname, intent) in [
            (handshake("Play.Example.com.\0", b"\x63\xdd\x01"), "play.example.com", Intent::Status),
            (long_handshake, &long, Intent::Login),
        ] {
            for end in 0..handshake.len() {
                assert_eq!(parse(&handshake[..end]).unwrap(), None, "{end} bytes");
            }
            let expected =
                Hello::Handshake(Handshake { protocol: 776, hostname: hostname.into(), port: 25565, intent });
            assert_eq!(parse(&handshake).unwrap(), Some((expected, handshake.len())));
            let pipelined = [&handshake[..], b"\x06\x00\x04Alex"].concat();
            assert_eq!(parse(&pipelined).unwrap().unwrap().1, handshake.len());
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

    #[test]
    fn reads_the_hostname_a_1_6_ping_names() {
        let ping = legacy_ping(PING_HOST, "Play.Example.com.");
        for end in 0..ping.len() {
            assert_eq!(parse(&ping[..end]).unwrap(), None, "{end} bytes");
        }
        let named = Hello::LegacyPing { hostname: Some("play.example.com".into()) };
        assert_eq!(parse(&ping).unwrap(), Some((named, ping.len())));
        let other = legacy_ping("MC|Other", "play.example.com");
        assert_eq!(parse(&other).unwrap(), Some((Hello::LegacyPing { hostname: None }, other.len())));
    }

    #[tokio::test]
    async fn tells_legacy_pings_from_modern_frames() {
        for (opening, legacy) in [(&b"\xfe"[..], true), (b"\xfe\x01", true), (b"\xfe\x01\x00", false)] {
            let (mut client, mut edge) = tokio::io::duplex(64);
            client.write_all(opening).await.unwrap();
            let read = read(&mut edge, Duration::from_millis(20)).await;
            if legacy {
                let opened = read.unwrap();
                assert_eq!((opened.hello, opened.bytes), (Hello::LegacyPing { hostname: None }, opening.to_vec()));
            } else {
                assert_eq!(read.err().unwrap().kind(), io::ErrorKind::TimedOut);
            }
        }
    }
}
