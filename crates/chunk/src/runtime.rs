use chunk_runtime::RuntimeConnection;
use std::{
    io,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::shutdown_signal;

#[derive(clap::Args)]
pub(crate) struct Options {
    #[arg(long, default_value = "jvm/runtime/build/install/runtime")]
    distribution: PathBuf,
    #[arg(long, default_value = "java")]
    java: PathBuf,
    #[arg(long, default_value = ".chunk/runtime.json")]
    connection: PathBuf,
    #[arg(long, default_value = "local")]
    environment: String,
    #[arg(long, default_value = "local")]
    deployment: String,
    #[arg(long, default_value = "local")]
    machine_profile: String,
    #[arg(long, default_value = "local-bridge-fixture")]
    artifact_digest: String,
    #[arg(long, default_value = "512")]
    memory_mib: u32,
    /// Start empty; the control plane provisions gameplay sessions.
    #[arg(long)]
    managed: bool,
}

pub(crate) fn read_target(path: &Path) -> io::Result<chunk_edge::GameplayTarget> {
    let record: RuntimeConnection = serde_json::from_slice(&std::fs::read(path)?).map_err(io::Error::other)?;
    let deployment = record
        .identity
        .deployment
        .ok_or_else(|| io::Error::other("missing deployment"))?;
    Ok(chunk_edge::GameplayTarget {
        endpoint: record.endpoint,
        token: record.token,
        environment: deployment.environment,
        deployment: deployment.deployment,
    })
}

pub(crate) async fn run(options: Options) -> io::Result<()> {
    let connection = &options.connection;
    if options.managed && (connection.exists() || connection.with_extension("exit").exists()) {
        return Err(io::Error::other("managed runtime identity already used"));
    }
    let result = supervise(&options).await;
    // A missing connection file alone cannot prove that a runtime has stopped.
    if options.managed {
        std::fs::write(connection.with_extension("exit"), b"stopped")?;
    }
    result
}

async fn supervise(options: &Options) -> io::Result<()> {
    let connection = &options.connection;
    if !(128..=8192).contains(&options.memory_mib) {
        return Err(io::Error::other("invalid JVM memory"));
    }
    if let Some(parent) = connection.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let process = chunk_runtime::ManagedJvm::launch(chunk_runtime::Launch {
        program: options.java.clone(),
        arguments: vec![
            format!("-Xmx{}M", options.memory_mib),
            "-cp".into(),
            options.distribution.join("lib/*").to_string_lossy().into_owned(),
            "dev.chunkzero.runtime.BridgeMainKt".into(),
        ],
        deployment: chunk_runtime::DeploymentRef {
            environment: options.environment.clone(),
            deployment: options.deployment.clone(),
        },
        machine_profile: options.machine_profile.clone(),
        artifact_digest: options.artifact_digest.clone(),
        log_path: connection.with_extension("log"),
        startup_timeout: Duration::from_secs(30),
        bootstrap_session: !options.managed,
    })
    .await?;
    let record = serde_json::to_vec(&RuntimeConnection {
        endpoint: process.endpoint().into(),
        token: process.credential().into(),
        identity: process.identity().clone(),
    })
    .map_err(io::Error::other)?;
    let result = async {
                let mut options = std::fs::OpenOptions::new();
                options.create_new(true).write(true);
                #[cfg(unix)] {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                options.open(connection)?.write_all(&record)?;
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
    if std::fs::read(connection).ok().as_deref() == Some(&record) {
        std::fs::remove_file(connection)?;
    }
    result
}
