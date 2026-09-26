use std::{
    cell::RefCell,
    future::{Future, poll_fn},
    io,
    pin::Pin,
    sync::atomic::{AtomicI32, Ordering},
    task::{Context, Poll, ready},
    time::Duration,
};

use bytes::{Bytes, BytesMut};
use chunk_protocol::{Decode, Encode, MAX_FRAME_SIZE, Packet, VarInt, decode_frame, encode_packet};
use libdeflater::{CompressionLvl, Compressor, Decompressor};
use openssl::{cipher::Cipher, cipher_ctx::CipherCtx};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    time::Instant,
};
use tokio_util::io::poll_read_buf;

/// Space reserved for each socket read.
const READ_SIZE: usize = 8 * 1024;
/// Buffer capacity kept after an unusually large packet has been handled.
const RETAINED_CAPACITY: usize = 64 * 1024;
/// Bytes queued output must drain within each `WRITE_TIMEOUT`, so a peer that
/// reads a trickle cannot keep a backlog alive indefinitely.
const MIN_WRITE_PROGRESS: usize = 16 * 1024;

/// libdeflate level for outgoing packets: level 1 saves ~30% gateway CPU on chunk
/// data but sends ~10% more bytes, and egress costs more than the CPU saved.
/// `chunk-bench` overrides it before any compressor exists to compare levels.
static DEFLATE_LEVEL: AtomicI32 = AtomicI32::new(6);

thread_local! {
    // Every Minecraft packet is an independent zlib stream, so each worker
    // thread resets one shared state instead of allocating one per packet.
    static DEFLATE: RefCell<Compressor> = RefCell::new(Compressor::new(
        CompressionLvl::new(DEFLATE_LEVEL.load(Ordering::Relaxed)).unwrap_or_default(),
    ));
    static INFLATE: RefCell<Decompressor> = RefCell::new(Decompressor::new());
}

/// A framed connection. Outgoing frames are encoded and encrypted into one
/// reusable buffer and sent by `flush` or `pump`, so several can share a write.
pub(super) struct Transport<S> {
    stream: S,
    input: BytesMut,
    output: Vec<u8>,
    written: usize,
    /// When queued output was last emptied or drained by `MIN_WRITE_PROGRESS`,
    /// and the bytes written since.
    progress: Instant,
    progressed: usize,
    encrypt: Option<CipherCtx>,
    decrypt: Option<CipherCtx>,
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
        encode_frame(&mut self.wire, unframe(frame)?, self.compression)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Transport<S> {
    pub(super) fn new(stream: S) -> Self {
        Self {
            stream,
            input: BytesMut::new(),
            output: Vec::new(),
            written: 0,
            progress: Instant::now(),
            progressed: 0,
            encrypt: None,
            decrypt: None,
            compression: None,
        }
    }

    pub(super) fn enable_encryption(&mut self, secret: &[u8; 16]) -> io::Result<()> {
        self.encrypt = Some(cfb8(secret, true)?);
        let mut decrypt = cfb8(secret, false)?;
        // Bytes read beyond Encryption Response already belong to the encrypted stream.
        apply(&mut decrypt, &mut self.input)?;
        self.decrypt = Some(decrypt);
        Ok(())
    }

    pub(super) fn enable_compression(&mut self, threshold: usize) {
        self.compression = Some(threshold);
    }

    pub(super) async fn read_frame(&mut self, limit: usize) -> io::Result<Bytes> {
        poll_fn(|cx| self.poll_frame(cx, limit)).await
    }

    /// The next frame if it has already been received, without reading the socket.
    pub(super) fn buffered_frame(&mut self, limit: usize) -> io::Result<Option<Bytes>> {
        // A compressed frame needs room for its uncompressed-length prefix and zlib overhead.
        let wire_limit = if self.compression.is_some() { limit.saturating_add(1024) } else { limit };
        let Some(frame) = decode_frame(&mut self.input, wire_limit).map_err(invalid_data)? else {
            return Ok(None);
        };
        match self.compression {
            Some(threshold) => inflate(&frame, threshold, limit).map(Some),
            None => Ok(Some(frame)),
        }
    }

    pub(super) fn has_buffered_data(&self) -> bool {
        !self.input.is_empty()
    }

    /// Encodes a frame body for the next flush or pump.
    pub(super) fn queue(&mut self, body: &[u8]) -> io::Result<()> {
        self.compact();
        let start = self.output.len();
        encode_frame(&mut self.output, body, self.compression)?;
        self.seal(start)
    }

    pub(super) fn queue_encoded(&mut self, frame: &[u8]) -> io::Result<()> {
        self.queue(unframe(frame)?)
    }

    /// Queued bytes not yet accepted by the socket.
    pub(super) fn queued(&self) -> usize {
        self.output.len() - self.written
    }

    /// When queued output that stops draining should be abandoned.
    pub(super) fn write_deadline(&self) -> Option<Instant> {
        (!self.output.is_empty()).then(|| self.progress + WRITE_TIMEOUT)
    }

    pub(super) async fn write_body(&mut self, body: &[u8]) -> io::Result<()> {
        self.queue(body)?;
        self.flush().await
    }

    pub(super) async fn write_packet<P: Packet + Encode>(&mut self, packet: &P) -> io::Result<()> {
        self.write_encoded(&encode_packet(packet).map_err(invalid_data)?).await
    }

    pub(super) async fn write_encoded(&mut self, frame: &[u8]) -> io::Result<()> {
        self.queue_encoded(frame)?;
        self.flush().await
    }

    pub(super) async fn write_prepared(&mut self, packets: &PreparedPackets) -> io::Result<()> {
        if packets.compression != self.compression {
            return Err(invalid_data("prepared packets have a different compression threshold"));
        }
        self.compact();
        let start = self.output.len();
        self.output.extend_from_slice(&packets.wire);
        self.seal(start)?;
        self.flush().await
    }

    /// Sends all queued output. Cancelling keeps unsent bytes queued in order.
    pub(super) async fn flush(&mut self) -> io::Result<()> {
        poll_fn(|cx| self.poll_output(cx)).await
    }

    /// Sends queued output while waiting for a frame when `limit` is set.
    /// Returns `None` once output that was queued at the start is flushed.
    pub(super) async fn pump(&mut self, limit: Option<usize>) -> io::Result<Option<Bytes>> {
        let flushing = !self.output.is_empty();
        poll_fn(|cx| {
            if flushing && self.poll_output(cx)?.is_ready() {
                return Poll::Ready(Ok(None));
            }
            match limit {
                Some(limit) => self.poll_frame(cx, limit).map_ok(Some),
                None => Poll::Pending,
            }
        })
        .await
    }

    pub(super) async fn shutdown(&mut self) -> io::Result<()> {
        self.flush().await?;
        self.stream.shutdown().await
    }

    fn poll_frame(&mut self, cx: &mut Context<'_>, limit: usize) -> Poll<io::Result<Bytes>> {
        loop {
            if let Some(frame) = self.buffered_frame(limit)? {
                return Poll::Ready(Ok(frame));
            }
            ready!(self.poll_fill(cx))?;
        }
    }

    fn poll_fill(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.input.reserve(READ_SIZE);
        if self.input.is_empty() && self.input.capacity() > RETAINED_CAPACITY {
            self.input = BytesMut::with_capacity(READ_SIZE);
        }
        let start = self.input.len();
        if ready!(poll_read_buf(Pin::new(&mut self.stream), cx, &mut self.input))? == 0 {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::UnexpectedEof, "client closed the connection")));
        }
        if let Some(decrypt) = &mut self.decrypt {
            apply(decrypt, &mut self.input[start..])?;
        }
        Poll::Ready(Ok(()))
    }

    fn poll_output(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.written < self.output.len() {
            let count = ready!(Pin::new(&mut self.stream).poll_write(cx, &self.output[self.written..]))?;
            if count == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.written += count;
            self.progressed += count;
            if self.progressed >= MIN_WRITE_PROGRESS {
                self.progress = Instant::now();
                self.progressed = 0;
            }
        }
        ready!(Pin::new(&mut self.stream).poll_flush(cx))?;
        self.output.clear();
        self.output.shrink_to(RETAINED_CAPACITY);
        self.written = 0;
        self.progressed = 0;
        Poll::Ready(Ok(()))
    }

    /// Drops output the socket already accepted once it outweighs what remains,
    /// so a peer that never fully drains cannot grow the buffer past twice its backlog.
    fn compact(&mut self) {
        if self.written > 0 && self.written >= self.queued() {
            self.output.drain(..self.written);
            self.written = 0;
        }
    }

    /// Encrypts output queued from `start`.
    fn seal(&mut self, start: usize) -> io::Result<()> {
        if start == 0 {
            self.progress = Instant::now();
            self.progressed = 0;
        }
        if let Some(encrypt) = &mut self.encrypt {
            apply(encrypt, &mut self.output[start..])?;
        }
        Ok(())
    }
}

fn cfb8(secret: &[u8; 16], encrypt: bool) -> io::Result<CipherCtx> {
    let mut context = CipherCtx::new()?;
    if encrypt {
        context.encrypt_init(Some(Cipher::aes_128_cfb8()), Some(secret), Some(secret))?;
    } else {
        context.decrypt_init(Some(Cipher::aes_128_cfb8()), Some(secret), Some(secret))?;
    }
    Ok(context)
}

fn apply(cipher: &mut CipherCtx, bytes: &mut [u8]) -> io::Result<()> {
    if cipher.cipher_update_inplace(bytes, bytes.len())? != bytes.len() {
        return Err(io::Error::other("unexpected CFB8 output length"));
    }
    Ok(())
}

/// The body of an encoded frame, without its length prefix.
fn unframe(mut frame: &[u8]) -> io::Result<&[u8]> {
    VarInt::decode(&mut frame).map_err(invalid_data)?;
    Ok(frame)
}

/// Appends one wire frame containing `body` (a packet ID and payload).
fn encode_frame(output: &mut Vec<u8>, body: &[u8], compression: Option<usize>) -> io::Result<()> {
    if body.is_empty() || body.len() > MAX_FRAME_SIZE {
        return Err(invalid_data("invalid frame size"));
    }
    match compression {
        None => {
            varint(output, body.len())?;
            output.extend_from_slice(body);
        }
        Some(threshold) if body.len() < threshold => {
            varint(output, body.len() + 1)?;
            output.push(0);
            output.extend_from_slice(body);
        }
        Some(_) => {
            let start = output.len();
            varint(output, body.len())?;
            deflate(body, output)?;
            let length = output.len() - start;
            if length > MAX_FRAME_SIZE {
                output.truncate(start);
                return Err(invalid_data("compressed frame too large"));
            }
            let mut prefix = Vec::with_capacity(3);
            varint(&mut prefix, length)?;
            output.splice(start..start, prefix);
        }
    }
    Ok(())
}

/// Sets the level for compressors created afterwards; existing threads keep theirs.
#[cfg(feature = "bench-support")]
pub(super) fn set_compression_level(level: i32) -> io::Result<()> {
    if !(1..=12).contains(&level) {
        return Err(invalid_data("compression level must be 1..=12"));
    }
    DEFLATE_LEVEL.store(level, Ordering::Relaxed);
    Ok(())
}

/// Wire length of one frame, before encryption.
#[cfg(feature = "bench-support")]
pub(super) fn frame_len(body: &[u8], compression: Option<usize>) -> io::Result<usize> {
    let mut output = Vec::new();
    encode_frame(&mut output, body, compression)?;
    Ok(output.len())
}

fn varint(output: &mut Vec<u8>, value: usize) -> io::Result<()> {
    VarInt(i32::try_from(value).map_err(invalid_data)?).encode(output).map_err(invalid_data)
}

fn deflate(body: &[u8], output: &mut Vec<u8>) -> io::Result<()> {
    DEFLATE.with_borrow_mut(|deflate| {
        let start = output.len();
        output.resize(start + deflate.zlib_compress_bound(body.len()), 0);
        let count = deflate.zlib_compress(body, &mut output[start..]).map_err(invalid_data)?;
        output.truncate(start + count);
        Ok(())
    })
}

fn inflate(frame: &Bytes, threshold: usize, limit: usize) -> io::Result<Bytes> {
    let mut body = frame.as_ref();
    let length = usize::try_from(VarInt::decode(&mut body).map_err(invalid_data)?.0).map_err(invalid_data)?;
    if length == 0 {
        if body.is_empty() || body.len() >= threshold || body.len() > limit {
            return Err(invalid_data("invalid uncompressed packet length"));
        }
        return Ok(frame.slice(frame.len() - body.len()..));
    }
    if length < threshold || length > limit.min(MAX_FRAME_SIZE) {
        return Err(invalid_data("invalid decompressed packet length"));
    }
    let mut output = vec![0; length];
    if INFLATE.with_borrow_mut(|inflate| inflate.zlib_decompress(body, &mut output)).map_err(invalid_data)? != length {
        return Err(invalid_data("invalid or mismatched zlib stream"));
    }
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
