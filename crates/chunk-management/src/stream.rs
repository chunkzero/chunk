use std::marker::PhantomData;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use prost::Message;
use serde::Deserialize;

use crate::error::{Error, WireError};

const COMPRESSED: u8 = 0x01;
const END_STREAM: u8 = 0x02;
const HEADER_BYTES: usize = 5;
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// Messages of a server stream, in order. Dropping it closes the stream.
pub struct Stream<T> {
    response: reqwest::Response,
    buffer: BytesMut,
    finished: bool,
    message: PhantomData<fn() -> T>,
}

impl<T: Message + Default> Stream<T> {
    pub(crate) fn new(response: reqwest::Response) -> Self {
        Self { response, buffer: BytesMut::new(), finished: false, message: PhantomData }
    }

    /// The next message, or None once the service ended the stream successfully.
    ///
    /// # Errors
    /// The status the service ended the stream with, a transport failure, or a malformed or truncated stream. The
    /// stream is over after any error.
    pub async fn message(&mut self) -> Result<Option<T>, Error> {
        while !self.finished {
            if let Some((flags, payload)) = self.frame()? {
                if flags & END_STREAM != 0 {
                    self.finished = true;
                    return end_of_stream(&payload).map(|()| None);
                }
                if flags & COMPRESSED != 0 {
                    return self.fail("a compressed message, which the client did not ask for".into());
                }
                return match T::decode(payload) {
                    Ok(message) => Ok(Some(message)),
                    Err(error) => self.fail(error.to_string()),
                };
            }
            match self.response.chunk().await {
                Ok(Some(chunk)) => self.buffer.extend_from_slice(&chunk),
                Ok(None) => return self.fail("the stream ended without an end-of-stream message".into()),
                Err(error) => {
                    self.finished = true;
                    return Err(error.into());
                }
            }
        }
        Ok(None)
    }

    /// The next whole frame in the buffer, if one has arrived.
    fn frame(&mut self) -> Result<Option<(u8, Bytes)>, Error> {
        let Some(header) = self.buffer.get(..HEADER_BYTES) else { return Ok(None) };
        let flags = header[0];
        let length = usize::try_from(u32::from_be_bytes([header[1], header[2], header[3], header[4]]))
            .map_err(|error| Error::Protocol(error.to_string()))?;
        if length > MAX_MESSAGE_BYTES {
            return self.fail(format!("a {length}-byte message"));
        }
        if self.buffer.len() < HEADER_BYTES + length {
            return Ok(None);
        }
        self.buffer.advance(HEADER_BYTES);
        Ok(Some((flags, self.buffer.split_to(length).freeze())))
    }

    fn fail<U>(&mut self, problem: String) -> Result<U, Error> {
        self.finished = true;
        Err(Error::Protocol(problem))
    }
}

#[derive(Deserialize)]
struct EndOfStream {
    error: Option<WireError>,
}

fn end_of_stream(payload: &[u8]) -> Result<(), Error> {
    let end: EndOfStream =
        serde_json::from_slice(payload).map_err(|error| Error::Protocol(format!("end-of-stream message: {error}")))?;
    match end.error {
        Some(error) => Err(error.into_status().into()),
        None => Ok(()),
    }
}

/// One Connect envelope: flags, a big-endian length and the payload.
pub(crate) fn envelope(flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(HEADER_BYTES + payload.len());
    frame.put_u8(flags);
    frame.put_u32(u32::try_from(payload.len()).expect("messages are far smaller than 4 GiB"));
    frame.put_slice(payload);
    frame
}
