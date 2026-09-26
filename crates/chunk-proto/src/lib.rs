//! Internal service bindings. Application code uses deployment-bound clients.

#[allow(clippy::all, clippy::pedantic)]
pub mod v1 {
    tonic::include_proto!("chunk.v1");
}

pub mod sync {
    #[allow(clippy::all, clippy::pedantic)]
    pub mod v1 {
        tonic::include_proto!("chunk.sync.v1");
    }
}
