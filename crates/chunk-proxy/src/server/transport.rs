use std::{future::Future, io, io::Write as _, time::Duration};

use bytes::{Bytes, BytesMut};
use chunk_protocol::{Decode, Encode, MAX_FRAME_SIZE, Packet, VarInt, decode_frame, encode_packet};
use flate2::{Compression, Decompress, FlushDecompress, Status, write::ZlibEncoder};
use openssl::symm::{Cipher, Crypter, Mode};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(super) struct Transport<S> {
    stream: S,
    buffer: BytesMut,
    encrypt: Option<Crypter>,
    decrypt: Option<Crypter>,
    compression: Option<usize>,
}

/// Framed packets compressed once for a listener's negotiated threshold.
/// Encryption remains connection-specific and is applied only when writing.
pub(super) struct PreparedPackets {
    compression: Option<usize>,
    wire: Vec<u8>,
}

impl PreparedPackets {
    pub(super) fn new(compression: Option<usize>) -> Self {
        Self { compression, wire: Vec::new() }
    }

    pub(super) fn push<P: Packet + Encode>(&mut self, packet: &P) -> io::Result<()> {
        self.push_frame(&encode_packet(packet).map_err(invalid_data)?)
    }

    pub(super) fn push_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        if let Some(threshold) = self.compression {
            let mut body = frame;
            VarInt::decode(&mut body).map_err(invalid_data)?;
            self.wire.extend(deflate(body, threshold)?);
        } else {
            self.wire.extend_from_slice(frame);
        }
        Ok(())
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Transport<S> {
    pub(super) fn new(stream: S) -> Self {
        Self { stream, buffer: BytesMut::new(), encrypt: None, decrypt: None, compression: None }
    }

    pub(super) fn enable_encryption(&mut self, secret: &[u8; 16]) -> io::Result<()> {
        self.encrypt = Some(Crypter::new(Cipher::aes_128_cfb8(), Mode::Encrypt, secret, Some(secret))?);
        let mut decrypt = Crypter::new(Cipher::aes_128_cfb8(), Mode::Decrypt, secret, Some(secret))?;
        // Bytes read beyond Encryption Response already belong to the encrypted stream.
        self.buffer = transform(&mut decrypt, &self.buffer)?.as_slice().into();
        self.decrypt = Some(decrypt);
        Ok(())
    }

    pub(super) fn enable_compression(&mut self, threshold: usize) {
        self.compression = Some(threshold);
    }

    pub(super) async fn read_frame(&mut self, limit: usize) -> io::Result<Bytes> {
        loop {
            // A compressed frame needs room for its uncompressed-length prefix and zlib overhead.
            let wire_limit = if self.compression.is_some() { limit.saturating_add(1024) } else { limit };
            if let Some(frame) = decode_frame(&mut self.buffer, wire_limit).map_err(invalid_data)? {
                return self.compression.map_or(Ok(frame.clone()), |threshold| inflate(&frame, threshold, limit));
            }
            let mut bytes = [0; 4096];
            let count = self.stream.read(&mut bytes).await?;
            if count == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "client closed the connection"));
            }
            if let Some(decrypt) = &mut self.decrypt {
                self.buffer.extend_from_slice(&transform(decrypt, &bytes[..count])?);
            } else {
                self.buffer.extend_from_slice(&bytes[..count]);
            }
        }
    }

    pub(super) fn has_buffered_data(&self) -> bool {
        !self.buffer.is_empty()
    }

    pub(super) async fn write_body(&mut self, body: &[u8]) -> io::Result<()> {
        if body.is_empty() || body.len() > MAX_FRAME_SIZE {
            return Err(invalid_data("invalid player frame size"));
        }
        let mut framed = Vec::with_capacity(body.len() + 5);
        VarInt(i32::try_from(body.len()).map_err(invalid_data)?).encode(&mut framed).map_err(invalid_data)?;
        framed.extend_from_slice(body);
        self.write_encoded(&framed).await
    }

    pub(super) async fn write_packet<P: Packet + Encode>(&mut self, packet: &P) -> io::Result<()> {
        self.write_encoded(&encode_packet(packet).map_err(invalid_data)?).await
    }

    pub(super) async fn write_encoded(&mut self, frame: &[u8]) -> io::Result<()> {
        let output = if let Some(threshold) = self.compression {
            let mut body = frame;
            VarInt::decode(&mut body).map_err(invalid_data)?;
            deflate(body, threshold)?
        } else {
            frame.to_vec()
        };
        self.write_wire(&output).await
    }

    pub(super) async fn write_prepared(&mut self, packets: &PreparedPackets) -> io::Result<()> {
        if packets.compression != self.compression {
            return Err(invalid_data("prepared packets have a different compression threshold"));
        }
        self.write_wire(&packets.wire).await
    }

    async fn write_wire(&mut self, wire: &[u8]) -> io::Result<()> {
        if let Some(encrypt) = &mut self.encrypt {
            self.stream.write_all(&transform(encrypt, wire)?).await
        } else {
            self.stream.write_all(wire).await
        }
    }

    pub(super) async fn shutdown(&mut self) -> io::Result<()> {
        self.stream.shutdown().await
    }
}

fn transform(cipher: &mut Crypter, bytes: &[u8]) -> io::Result<Vec<u8>> {
    let mut output = vec![0; bytes.len() + Cipher::aes_128_cfb8().block_size()];
    let count = cipher.update(bytes, &mut output)?;
    if count != bytes.len() {
        return Err(io::Error::other("unexpected CFB8 output length"));
    }
    output.truncate(count);
    Ok(output)
}

fn deflate(body: &[u8], threshold: usize) -> io::Result<Vec<u8>> {
    let mut payload = Vec::new();
    if body.len() >= threshold {
        VarInt(i32::try_from(body.len()).map_err(invalid_data)?).encode(&mut payload).map_err(invalid_data)?;
        let mut encoder = ZlibEncoder::new(payload, Compression::default());
        encoder.write_all(body)?;
        payload = encoder.finish()?;
    } else {
        payload.push(0);
        payload.extend_from_slice(body);
    }
    if payload.len() > MAX_FRAME_SIZE {
        return Err(invalid_data("compressed frame too large"));
    }
    let mut frame = Vec::new();
    VarInt(i32::try_from(payload.len()).map_err(invalid_data)?).encode(&mut frame).map_err(invalid_data)?;
    frame.extend_from_slice(&payload);
    Ok(frame)
}

fn inflate(mut frame: &[u8], threshold: usize, limit: usize) -> io::Result<Bytes> {
    let length = usize::try_from(VarInt::decode(&mut frame).map_err(invalid_data)?.0).map_err(invalid_data)?;
    if length == 0 {
        if frame.is_empty() || frame.len() >= threshold || frame.len() > limit {
            return Err(invalid_data("invalid uncompressed packet length"));
        }
        return Ok(Bytes::copy_from_slice(frame));
    }
    if length < threshold || length > limit.min(MAX_FRAME_SIZE) {
        return Err(invalid_data("invalid decompressed packet length"));
    }
    let mut output = vec![0; length + 1];
    let mut decoder = Decompress::new(true);
    let status = decoder.decompress(frame, &mut output, FlushDecompress::Finish).map_err(invalid_data)?;
    if status != Status::StreamEnd || decoder.total_in() != frame.len() as u64 || decoder.total_out() != length as u64 {
        return Err(invalid_data("invalid or mismatched zlib stream"));
    }
    output.truncate(length);
    Ok(output.into())
}

pub(super) fn timed_out(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, message)
}

pub(super) fn invalid_data(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

pub(super) const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) async fn within<T>(limit: Duration, future: impl Future<Output = io::Result<T>>) -> io::Result<T> {
    tokio::time::timeout(limit, future)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "operation timed out"))?
}

#[cfg(test)]
mod tests;
