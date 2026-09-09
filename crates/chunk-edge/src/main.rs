use chunk_service::{optional, required};
use std::{io, path::PathBuf};
#[tokio::main]
async fn main() -> io::Result<()> {
    chunk_service::logging();
    let backend: PathBuf = required("CHUNK_BACKEND_FILE")?;
    let control: PathBuf = required("CHUNK_CONTROL_FILE")?;
    let config = chunk_edge::ProxyConfig {
        platform: Some(chunk_edge::PlatformTarget {
            backend: chunk_service::read(&backend)?,
            control: chunk_service::read(&control)?,
        }),
        motd: optional("CHUNK_MOTD")?.unwrap_or_else(|| "chunk".into()),
        max_connections: optional("CHUNK_MAX_CONNECTIONS")?
            .unwrap_or(std::num::NonZeroUsize::new(1024).expect("nonzero")),
        ..Default::default()
    };
    let address = optional("CHUNK_BIND")?.unwrap_or(([0, 0, 0, 0], 25565).into());
    chunk_service::run(|stop| {
        chunk_edge::run(address, config, async move {
            stop.cancelled().await;
            Ok(())
        })
    })
    .await
}
