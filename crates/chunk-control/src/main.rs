use chunk_service::{optional, required};
use std::{io, path::PathBuf, sync::Arc};
#[tokio::main]
async fn main() -> io::Result<()> {
    chunk_service::logging();
    let state: PathBuf = required("CHUNK_STATE")?;
    let control: chunk_control::Config = chunk_service::read(&required::<PathBuf>("CHUNK_CONFIG")?)?;
    let backend = chunk_service::read(&required::<PathBuf>("CHUNK_BACKEND_FILE")?)?;
    let host = Arc::new(chunk_control::ProcessHost::new(chunk_control::ProcessHostConfig {
        distribution: required("CHUNK_DISTRIBUTION")?,
        java: required("CHUNK_JAVA")?,
        directory: state.join("nodes"),
        deployment: control.deployment.clone(),
        apps: control.apps.clone(),
        profiles: control.profiles.clone(),
        backend,
    }));
    let config = chunk_control::server::Config {
        state,
        connection: required("CHUNK_CONNECTION")?,
        bind: optional("CHUNK_BIND")?.unwrap_or(([127, 0, 0, 1], 25567).into()),
        control,
        host,
    };
    chunk_service::run(|stop| chunk_control::server::run(config, tokio::sync::oneshot::channel().0, stop)).await
}
