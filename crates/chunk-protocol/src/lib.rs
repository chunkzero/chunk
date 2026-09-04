//! The Minecraft wire protocol, without sockets.
//!
//! Framing, varints, the handshake, login and configuration phases, and the
//! short list of play packets the edge decodes (see `docs/architecture.md`).
//! Every other play packet is read as a length and an id and passed through
//! as an opaque frame. Encryption and compression codecs live here because
//! the edge terminates both on the client side and sends plain frames inward.
//!
//! One protocol version per deploy. This crate has no I/O and no policy about
//! what to do with a packet; that is `chunk-proxy`.
