//! Internal service bindings. Application code uses deployment-bound clients.

pub mod control {
    //! Control's system-table row encodings.
    #[allow(clippy::all, clippy::pedantic)]
    pub mod v1 {
        tonic::include_proto!("chunk.control.v1");
    }
}

pub mod sync {
    #[allow(clippy::all, clippy::pedantic)]
    pub mod v1 {
        tonic::include_proto!("chunk.sync.v1");
    }
}
