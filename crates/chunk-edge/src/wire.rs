//! The few uncompressed packets the edge reads and writes itself, the same in every modern protocol version.

use std::io;

use bytes::{Bytes, BytesMut};
use chunk_protocol::{Decode, Encode, McString, VarInt, decode_frame};
use tokio::io::{AsyncRead, AsyncReadExt};

/// Reads frames from a stream, starting with bytes already read from it.
pub(crate) struct Frames<S> {
    pub stream: S,
    pending: BytesMut,
}

impl<S: AsyncRead + Unpin> Frames<S> {
    pub(crate) fn new(stream: S, pending: &[u8]) -> Self {
        Self { stream, pending: BytesMut::from(pending) }
    }

    /// The next frame's packet ID and payload, of at most `limit` bytes.
    pub(crate) async fn next(&mut self, limit: usize) -> io::Result<Bytes> {
        loop {
            if let Some(frame) = decode_frame(&mut self.pending, limit).map_err(invalid)? {
                return Ok(frame);
            }
            if self.stream.read_buf(&mut self.pending).await? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
    }
}

/// `body` with its length prefix.
pub(crate) fn frame(body: &[u8]) -> io::Result<Vec<u8>> {
    let mut frame = Vec::with_capacity(body.len() + 3);
    VarInt(i32::try_from(body.len()).map_err(invalid)?).encode(&mut frame).map_err(invalid)?;
    frame.extend_from_slice(body);
    Ok(frame)
}

/// A framed packet whose only field is `value`, as status responses and login disconnects are.
pub(crate) fn string_packet(id: i32, value: &str) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    VarInt(id).encode(&mut body).map_err(invalid)?;
    McString::<32767>::new(value).map_err(invalid)?.encode(&mut body).map_err(invalid)?;
    frame(&body)
}

/// The string field of packet `id`.
pub(crate) fn read_string_packet(id: i32, mut packet: &[u8]) -> io::Result<String> {
    let input = &mut packet;
    if VarInt::decode(input).map_err(invalid)?.0 != id {
        return Err(invalid("an unexpected packet"));
    }
    let value = McString::<32767>::decode(input).map_err(invalid)?;
    if !input.is_empty() {
        return Err(invalid("trailing bytes"));
    }
    Ok(value.as_str().to_owned())
}

/// A handshake packet, framed.
pub(crate) fn handshake(protocol: i32, hostname: &str, port: u16, intent: i32) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    VarInt(0).encode(&mut body).map_err(invalid)?;
    VarInt(protocol).encode(&mut body).map_err(invalid)?;
    McString::<255>::new(hostname).map_err(invalid)?.encode(&mut body).map_err(invalid)?;
    port.encode(&mut body).map_err(invalid)?;
    VarInt(intent).encode(&mut body).map_err(invalid)?;
    frame(&body)
}

/// A login-state Disconnect showing `message`.
pub(crate) fn disconnect(message: &str) -> io::Result<Vec<u8>> {
    string_packet(0, &serde_json::json!({ "text": message }).to_string())
}

pub(crate) fn invalid(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
