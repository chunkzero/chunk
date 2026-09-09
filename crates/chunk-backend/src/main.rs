use chunk_service::{optional, required};
use std::io;

#[tokio::main]
async fn main() -> io::Result<()> {
    chunk_service::logging();
    let config = chunk_backend::server::Config {
        bundle: required("CHUNK_BUNDLE")?,
        environment: required("CHUNK_ENVIRONMENT")?,
        state: required("CHUNK_STATE")?,
        connection: required("CHUNK_CONNECTION")?,
        bind: optional("CHUNK_BIND")?.unwrap_or(([127, 0, 0, 1], 25568).into()),
    };
    chunk_service::run(|stop| chunk_backend::server::run(config, tokio::sync::oneshot::channel().0, stop)).await
}
