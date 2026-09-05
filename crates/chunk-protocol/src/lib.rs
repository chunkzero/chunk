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
//!
//! `chunk_protocol_codegen::protocol_version!(v26_1, "data/26.1")` generates
//! a module from a directory relative to the invoking crate's manifest.
//! Snapshots include upstream attribution and a source manifest with revision
//! and SHA-256 checksums. Generation verifies local inputs and tracks changes;
//! disabled versions skip dataset loading. Only the pinned 26.1 release has been
//! validated, so the generator retains its release guard.
//!
//! The selected login packets cover login start, encryption negotiation, profile
//! properties, compression negotiation, plugin queries, and acknowledgment.
//! Configuration packets cover client information, plugin messages, keepalives,
//! ping/pong, known packs, feature flags, and completion. Selected play packets
//! cover movement, teleports, keepalives, chunk batches, and player readiness.
//! Limbo registry frames and required tags are compiled from the pinned login snapshot; its NBT
//! encoder handles trusted build inputs only. These are wire definitions; authentication,
//! encryption, compression framing, and connection state handling belong to the
//! proxy and are not implemented by this crate.
//!
//! [`Uuid`] holds 16 network-order bytes. [`ByteArray<N>`] and
//! [`BoundedArray<T, N>`] carry `VarInt` lengths; [`RemainingBytes<N>`] consumes
//! the rest of a packet. `Option<T>` uses a boolean presence byte. Generated
//! nested containers and enum mappers have typed structs and enums. Every
//! variable-length field has an explicit limit in its packet specification;
//! collection limits are local resource bounds. Callers should also enforce
//! a phase-specific frame limit before decoding.

extern crate self as chunk_protocol;

mod codec;
mod collections;
mod frame;
pub mod versions;

pub use chunk_protocol_derive::{Decode, Encode, Packet};
pub use codec::{Decode, Encode, McString, Uuid, VarInt};
pub use collections::{BoundedArray, ByteArray, RemainingBytes};
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
    #[error("collection exceeds its wire limit")]
    CollectionTooLong,
    #[error("invalid boolean (expected 0 or 1)")]
    InvalidBoolean,
    #[error("unknown enum value")]
    InvalidEnumValue,
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
