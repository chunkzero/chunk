use serde::{Deserialize, Serialize};
use std::{
    io,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::shutdown_signal;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnectionRecord {
    endpoint: String,
    token: String,
    environment: String,
    deployment: String,
}

pub(crate) fn read_target(path: &Path) -> io::Result<chunk_edge::GameplayTarget> {
    let record: ConnectionRecord = serde_json::from_slice(&std::fs::read(path)?).map_err(io::Error::other)?;
    Ok(chunk_edge::GameplayTarget {
        endpoint: record.endpoint,
        token: record.token,
        environment: record.environment,
        deployment: record.deployment,
    })
}

pub(crate) async fn run(
    distribution: PathBuf,
    java: PathBuf,
    connection: PathBuf,
    environment: String,
    deployment: String,
) -> io::Result<()> {
    if let Some(parent) = connection.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let process = chunk_runtime::ManagedJvm::launch(chunk_runtime::Launch {
        program: java,
        arguments: vec![
            "-Xmx512M".into(),
            "-cp".into(),
            distribution.join("lib/*").to_string_lossy().into_owned(),
            "dev.chunkzero.runtime.BridgeMainKt".into(),
        ],
        deployment: chunk_runtime::DeploymentRef {
            environment: environment.clone(),
            deployment: deployment.clone(),
        },
        machine_profile: "local".into(),
        artifact_digest: "local-bridge-fixture".into(),
        log_path: connection.with_extension("log"),
        startup_timeout: Duration::from_secs(30),
    })
    .await?;
    let record = serde_json::to_vec(&ConnectionRecord {
        endpoint: process.endpoint().into(),
        token: process.credential().into(),
        environment,
        deployment,
    })
    .map_err(io::Error::other)?;
    let result = async {
                let mut options = std::fs::OpenOptions::new();
                options.create_new(true).write(true);
                #[cfg(unix)] {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                options.open(&connection)?.write_all(&record)?;
                tracing::info!(endpoint = process.endpoint(), connection = %connection.display(), "supervised gameplay ready");
                let mut status = process.watch();
                tokio::select! {
                    result = shutdown_signal()? => result,
                    result = status.wait_for(|status| matches!(status.phase, chunk_runtime::Phase::Failed | chunk_runtime::Phase::Stopped)) => {
                        Err(io::Error::other(result.ok().and_then(|status| status.diagnostic.clone()).unwrap_or_else(|| "JVM stopped".into())))
                    }
                }
            }.await;
    process.stop().await;
    if std::fs::read(&connection).ok().as_deref() == Some(&record) {
        std::fs::remove_file(connection)?;
    }
    result
}
