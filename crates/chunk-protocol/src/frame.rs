use bytes::{Buf, Bytes, BytesMut};

use crate::{Decode, Encode, Error, Packet, Result, VarInt};

/// Minecraft's outer length prefix is at most three bytes.
pub const MAX_FRAME_SIZE: usize = (1 << 21) - 1;

/// Removes one complete, uncompressed frame (packet ID and payload).
/// Incomplete input is left untouched. `limit` may impose a smaller phase limit.
///
/// # Errors
/// Rejects empty frames, oversized lengths and prefixes longer than three bytes.
pub fn decode_frame(input: &mut BytesMut, limit: usize) -> Result<Option<Bytes>> {
    let mut length = 0;
    for index in 0..3 {
        let Some(&byte) = input.get(index) else {
            return Ok(None);
        };
        length |= usize::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            if length == 0 || length > limit.min(MAX_FRAME_SIZE) {
                return Err(Error::InvalidFrameLength);
            }
            let prefix = index + 1;
            if input.len() < prefix + length {
                return Ok(None);
            }
            input.advance(prefix);
            return Ok(Some(input.split_to(length).freeze()));
        }
    }
    Err(Error::InvalidFrameLength)
}

/// Decodes a packet ID and its complete payload. The caller selects the packet
/// type using the connection's state, direction and protocol version.
///
/// # Errors
/// Rejects mismatched IDs, malformed fields and trailing bytes.
pub fn decode_packet<P: Packet + Decode>(mut frame: &[u8]) -> Result<P> {
    if VarInt::decode(&mut frame)?.0 != P::ID {
        return Err(Error::UnexpectedPacket);
    }
    let packet = P::decode(&mut frame)?;
    if !frame.is_empty() {
        return Err(Error::TrailingBytes);
    }
    Ok(packet)
}

/// Encodes a packet with its ID and uncompressed outer length prefix.
///
/// # Errors
/// Returns an error if a field or the complete packet exceeds its wire limit.
pub fn encode_packet<P: Packet + Encode>(packet: &P) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    VarInt(P::ID).encode(&mut body)?;
    packet.encode(&mut body)?;
    if body.len() > MAX_FRAME_SIZE {
        return Err(Error::InvalidFrameLength);
    }
    let mut frame = Vec::with_capacity(body.len() + 3);
    VarInt(i32::try_from(body.len()).map_err(|_| Error::InvalidFrameLength)?).encode(&mut frame)?;
    frame.extend_from_slice(&body);
    Ok(frame)
}
