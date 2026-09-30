//! Minecraft framing on one connection: length prefixes and zlib compression. Large frames whose packet the bot
//! ignores are skipped as they arrive, reading their ID from the first inflated bytes, so chunk and registry bodies are
//! never buffered whole or inflated.
use std::io::Write;

use anyhow::{Context, Result, bail, ensure};
use bytes::{Buf, Bytes, BytesMut};
use chunk_protocol::{Decode, Encode, Packet, VarInt};
use flate2::{Compression, Decompress, FlushDecompress, write::ZlibEncoder};

/// Frames longer than this are skipped in place once their ID is known and unwanted.
const SKIP_ABOVE: usize = 4 * 1024;
/// The largest packet the bot inflates: disconnect reasons and the like are far smaller.
const MAX_INFLATED: usize = 1 << 20;

/// One received packet: its ID, and its payload unless the caller did not want it.
#[derive(Debug)]
pub struct Frame {
    pub id: i32,
    pub body: Option<Bytes>,
}

pub struct Decoder {
    /// Bytes read from the socket and not yet framed.
    pub buffer: BytesMut,
    /// Bytes of a skipped frame still to discard.
    skip: usize,
    compressed: bool,
    inflate: Decompress,
}

impl Decoder {
    pub fn new() -> Self {
        Self { buffer: BytesMut::with_capacity(16 * 1024), skip: 0, compressed: false, inflate: Decompress::new(true) }
    }

    pub fn enable_compression(&mut self) {
        self.compressed = true;
    }

    /// The next complete or skippable frame. `wanted` names the packet IDs whose payload the caller reads.
    pub fn next(&mut self, wanted: impl Fn(i32) -> bool) -> Result<Option<Frame>> {
        if self.skip > 0 {
            let discard = self.skip.min(self.buffer.len());
            self.buffer.advance(discard);
            self.skip -= discard;
            if self.skip > 0 {
                return Ok(None);
            }
        }
        let Some((length, prefix)) = frame_length(&self.buffer)? else { return Ok(None) };
        if self.buffer.len() >= prefix + length {
            self.buffer.advance(prefix);
            let frame = self.buffer.split_to(length).freeze();
            return self.decode(&frame, &wanted).map(Some);
        }
        if length > SKIP_ABOVE
            && let Some(id) = peek(&mut self.inflate, self.compressed, &self.buffer[prefix..])?
            && !wanted(id)
        {
            self.skip = prefix + length - self.buffer.len();
            self.buffer.clear();
            return Ok(Some(Frame { id, body: None }));
        }
        Ok(None)
    }

    fn decode(&mut self, frame: &Bytes, wanted: &impl Fn(i32) -> bool) -> Result<Frame> {
        let mut data = &frame[..];
        let inflated = if self.compressed { VarInt::decode(&mut data)?.0 } else { 0 };
        if inflated == 0 {
            let id = VarInt::decode(&mut data)?.0;
            let body = wanted(id).then(|| frame.slice(frame.len() - data.len()..));
            return Ok(Frame { id, body });
        }
        let id = peek(&mut self.inflate, true, frame)?.context("compressed frame without a packet ID")?;
        if !wanted(id) {
            return Ok(Frame { id, body: None });
        }
        let inflated = usize::try_from(inflated)?;
        ensure!(inflated <= MAX_INFLATED, "compressed packet {id:#x} too large to inflate");
        let mut body = Vec::with_capacity(inflated);
        self.inflate.reset(true);
        self.inflate.decompress_vec(data, &mut body, FlushDecompress::Finish)?;
        ensure!(body.len() == inflated, "compressed packet length mismatch");
        let mut rest = &body[..];
        VarInt::decode(&mut rest)?;
        let start = body.len() - rest.len();
        Ok(Frame { id, body: Some(Bytes::from(body).slice(start..)) })
    }
}

/// The frame's length and the size of its prefix, once the prefix is complete.
fn frame_length(buffer: &[u8]) -> Result<Option<(usize, usize)>> {
    let mut length = 0;
    for (index, &byte) in buffer.iter().take(3).enumerate() {
        length |= usize::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            ensure!(length > 0, "empty frame");
            return Ok(Some((length, index + 1)));
        }
    }
    if buffer.len() >= 3 {
        bail!("frame length prefix longer than three bytes");
    }
    Ok(None)
}

/// The packet ID at the start of a frame's content, inflating only its first bytes; None if more input is needed.
fn peek(inflate: &mut Decompress, compressed: bool, mut content: &[u8]) -> Result<Option<i32>> {
    let id = |mut bytes: &[u8]| match VarInt::decode(&mut bytes) {
        Ok(id) => Ok(Some(id.0)),
        Err(chunk_protocol::Error::Incomplete) => Ok(None),
        Err(error) => Err(error.into()),
    };
    if !compressed {
        return id(content);
    }
    let inflated = match VarInt::decode(&mut content) {
        Ok(length) => length.0,
        Err(chunk_protocol::Error::Incomplete) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if inflated == 0 {
        return id(content);
    }
    let mut head = [0; 5];
    inflate.reset(true);
    inflate.decompress(content, &mut head, FlushDecompress::None)?;
    id(&head[..usize::try_from(inflate.total_out())?])
}

/// Frames outgoing packets into `output`, compressing those at or above the threshold once compression is on.
pub struct Encoder {
    pub output: Vec<u8>,
    threshold: Option<usize>,
}

impl Encoder {
    pub fn new() -> Self {
        Self { output: Vec::with_capacity(256), threshold: None }
    }

    pub fn enable_compression(&mut self, threshold: usize) {
        self.threshold = Some(threshold);
    }

    pub fn packet<P: Packet + Encode>(&mut self, packet: &P) -> Result<()> {
        let mut body = Vec::with_capacity(64);
        VarInt(P::ID).encode(&mut body)?;
        packet.encode(&mut body)?;
        self.body(&body)
    }

    /// Frames a packet chunk-protocol doesn't generate, from its ID and encoded payload.
    pub fn raw(&mut self, id: i32, payload: &[u8]) -> Result<()> {
        let mut body = Vec::with_capacity(payload.len() + 5);
        VarInt(id).encode(&mut body)?;
        body.extend_from_slice(payload);
        self.body(&body)
    }

    fn body(&mut self, body: &[u8]) -> Result<()> {
        let length = |value: usize| i32::try_from(value).map(VarInt);
        match self.threshold {
            None => {
                length(body.len())?.encode(&mut self.output)?;
                self.output.extend_from_slice(body);
            }
            Some(threshold) if body.len() < threshold => {
                length(body.len() + 1)?.encode(&mut self.output)?;
                self.output.push(0);
                self.output.extend_from_slice(body);
            }
            Some(_) => {
                let mut content = Vec::new();
                length(body.len())?.encode(&mut content)?;
                let mut zlib = ZlibEncoder::new(content, Compression::fast());
                zlib.write_all(body)?;
                let content = zlib.finish()?;
                length(content.len())?.encode(&mut self.output)?;
                self.output.extend_from_slice(&content);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(encoder: &Encoder, decoder: &mut Decoder, wanted: impl Fn(i32) -> bool + Copy) -> Vec<Frame> {
        let mut received = Vec::new();
        // Feed a byte at a time, as a slow socket would, so every partial state is exercised.
        for &byte in &encoder.output {
            decoder.buffer.extend_from_slice(&[byte]);
            while let Some(frame) = decoder.next(wanted).unwrap() {
                received.push(frame);
            }
        }
        assert!(decoder.buffer.is_empty() && decoder.skip == 0);
        received
    }

    #[test]
    fn skips_large_unwanted_frames_and_reads_the_rest() {
        let large: Vec<u8> = (0..40_000_u32).map(|value| (value * 7 % 251) as u8).collect();
        for threshold in [None, Some(256)] {
            let mut encoder = Encoder::new();
            let mut decoder = Decoder::new();
            if let Some(threshold) = threshold {
                encoder.enable_compression(threshold);
                decoder.enable_compression();
            }
            encoder.raw(0x2d, &large).unwrap();
            encoder.raw(0x2c, &[1, 2, 3]).unwrap();
            encoder.raw(0x20, &large[..300]).unwrap();
            encoder.raw(0x0b, &[]).unwrap();
            let received = frames(&encoder, &mut decoder, |id| id != 0x2d);
            let ids: Vec<_> = received.iter().map(|frame| frame.id).collect();
            assert_eq!(ids, [0x2d, 0x2c, 0x20, 0x0b]);
            assert!(received[0].body.is_none());
            assert_eq!(received[1].body.as_deref(), Some(&[1, 2, 3][..]));
            assert_eq!(received[2].body.as_deref(), Some(&large[..300]));
            assert_eq!(received[3].body.as_deref(), Some(&[][..]));
        }
    }
}
