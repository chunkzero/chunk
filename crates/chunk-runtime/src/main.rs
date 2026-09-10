use chunk_service::{optional, required};
use std::io;
#[tokio::main]
async fn main() -> io::Result<()> {
    chunk_service::logging();
    let config = chunk_runtime::server::Config {
        distribution: required("CHUNK_DISTRIBUTION")?,
        java: required("CHUNK_JAVA")?,
        connection: required("CHUNK_CONNECTION")?,
        deployment: chunk_runtime::DeploymentRef {
            environment: required("CHUNK_ENVIRONMENT")?,
            deployment: required("CHUNK_DEPLOYMENT")?,
        },
        machine_profile: required("CHUNK_MACHINE_PROFILE")?,
        artifact_digest: required("CHUNK_ARTIFACT_DIGEST")?,
        memory_mib: required("CHUNK_MEMORY_MIB")?,
        backend: optional::<std::path::PathBuf>("CHUNK_BACKEND_FILE")?
            .map(|path| chunk_service::read(&path))
            .transpose()?,
    };
    chunk_service::run(|stop| chunk_runtime::server::run(config, stop)).await
}
