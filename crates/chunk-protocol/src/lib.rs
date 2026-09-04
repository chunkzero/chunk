//! Minecraft wire types and uncompressed framing, without sockets or policy.
//!
//! Codec derives serialize struct fields in declaration order. Wire types are
//! explicit: `VarInt` differs from a fixed-width `i32`, and `McString<N>` limits
//! strings to N UTF-16 code units, matching Java's string length.
//!
//! ```
//! use chunk_protocol::{Decode, Encode, Packet};
//!
//! #[derive(Encode, Decode, Packet)]
//! #[packet(id = 0x01, state = Status, direction = Serverbound)]
//! struct Ping { payload: i64 }
//! ```
//!
//! Derives support named, tuple and unit structs, including generics. Packet
//! IDs belong to a state and direction. Packet APIs live in [`versions`],
//! generated from local datasets and gated by explicit Cargo features.
//! The default feature is `mc-26-1`. With no version features, wire primitives
//! remain usable but no Minecraft version is enabled. There is no gameplay
//! version translation.

extern crate self as chunk_protocol;

mod codec;
mod frame;
pub mod versions;

pub use chunk_protocol_derive::{Decode, Encode, Packet};
pub use codec::{Decode, Encode, McString, VarInt};
pub use frame::{MAX_FRAME_SIZE, decode_frame, decode_packet, encode_packet};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("incomplete packet")]
    Incomplete,
    #[error("invalid or overflowing VarInt")]
    InvalidVarInt,
    #[error("invalid frame length")]
    InvalidFrameLength,
    #[error("string exceeds its wire limit")]
    StringTooLong,
    #[error("invalid UTF-8 string")]
    InvalidUtf8,
    #[error("unexpected packet id")]
    UnexpectedPacket,
    #[error("trailing bytes in packet")]
    TrailingBytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Handshake,
    Status,
    Login,
    Configuration,
    Play,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Serverbound,
    Clientbound,
}

pub trait Packet {
    const ID: i32;
    const STATE: State;
    const DIRECTION: Direction;
}
