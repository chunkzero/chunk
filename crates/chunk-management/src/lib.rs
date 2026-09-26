//! A client for the management service's `chunk.management.v1` API. It speaks the Connect protocol over HTTP/1.1
//! with binary protobuf: unary calls and server streams, which is what the Bun-hosted service serves.

mod client;
mod error;
mod methods;
mod stream;

pub use client::Client;
pub use error::{Code, Error, Status};
pub use stream::Stream;

/// The `chunk.management.v1` messages.
#[allow(clippy::all, clippy::pedantic)]
pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/chunk.management.v1.rs"));
}
