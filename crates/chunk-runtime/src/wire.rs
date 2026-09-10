use std::io;

use chunk_protocol::{Decode, Encode, Packet, decode_packet, encode_packet};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(crate) async fn read_packet<S: AsyncRead + Unpin, P: Packet + Decode>(stream: &mut S) -> io::Result<P> {
    let mut length = 0_usize;
    for index in 0..3 {
        let byte = stream.read_u8().await?;
        length |= usize::from(byte & 127) << (index * 7);
        if byte & 128 == 0 {
            if !(1..=65_536).contains(&length) {
                return Err(io::Error::other("invalid login frame length"));
            }
            let mut bytes = vec![0; length];
            stream.read_exact(&mut bytes).await?;
            return decode_packet(&bytes).map_err(io::Error::other);
        }
    }
    Err(io::Error::other("invalid login frame prefix"))
}

pub(crate) async fn write_packet<S: AsyncWrite + Unpin, P: Packet + Encode>(
    stream: &mut S,
    packet: &P,
) -> io::Result<()> {
    stream.write_all(&encode_packet(packet).map_err(io::Error::other)?).await
}
