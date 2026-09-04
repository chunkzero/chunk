//! The connection owner.
//!
//! Accepts a client's TCP connection and keeps it for the whole session with
//! the network. Drives handshake and login (encryption, Mojang
//! authentication, compression, transfer cookie verification), owns the
//! configuration phase, relays play frames to a session process over the
//! player stream, and parks the client in configuration while nobody owns
//! them. A move, a session ending and a process restart are all the same
//! withdraw, park, deliver sequence; a move to a session behind another edge
//! is a transfer packet with a signed cookie.
//!
//! The proxy asks the edge for decisions (the ping response, where to place a
//! player, whether a chat message may pass) through a narrow interface and
//! knows nothing about JavaScript, the database or the app.
