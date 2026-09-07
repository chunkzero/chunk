//! Internal service bindings. Application code uses deployment-bound clients.

#[allow(clippy::all, clippy::pedantic)]
pub mod v1 {
    tonic::include_proto!("chunk.v1");
}
