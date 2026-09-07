use std::{collections::BTreeMap, fs, io, net::SocketAddr, path::PathBuf, time::Duration};

use chunk_build::{Artifact, Inputs};
use chunk_control::{MachineProfile, SessionType};
use serde::{Deserialize, Serialize};

mod processes;

#[derive(clap::Args)]
pub(crate) struct Options {
    #[arg(long, default_value = "examples/local/project.json")]
    project: PathBuf,
    #[arg(long, default_value = ".chunk/local")]
    state: PathBuf,
    /// Java 25 executable. `just local` resolves this from the Gradle toolchain.
    #[arg(long)]
    java: PathBuf,
    #[arg(long, default_value = "127.0.0.1:25565")]
    bind: SocketAddr,
    #[arg(long, default_value = "127.0.0.1:25568")]
    backend_bind: SocketAddr,
    #[arg(long, default_value = "127.0.0.1:25567")]
    control_bind: SocketAddr,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Project {
    environment: String,
    backend_source: PathBuf,
    backend_contract: PathBuf,
    gameplay_distribution: PathBuf,
    profiles: BTreeMap<String, MachineProfile>,
    session_types: BTreeMap<String, SessionType>,
    max_processes: u16,
}

pub(crate) async fn run(mut options: Options) -> io::Result<()> {
    let shutdown = crate::shutdown_signal()?;
    tokio::pin!(shutdown);
    fs::create_dir_all(&options.state)?;
    options.state = options.state.canonicalize()?;
    let _lock = runner_lock(&options.state.join("runner.lock"))?;
    for address in [options.bind, options.backend_bind, options.control_bind] {
        if !address.ip().is_loopback() || address.port() == 0 {
            return Err(io::Error::other("local runner requires fixed loopback ports"));
        }
        std::net::TcpListener::bind(address).map_err(|error| {
            io::Error::other(format!(
                "{address} is unavailable; stop the existing server first: {error}"
            ))
        })?;
    }
    if options.bind == options.backend_bind
        || options.bind == options.control_bind
        || options.backend_bind == options.control_bind
    {
        return Err(io::Error::other("local service addresses must differ"));
    }
    java_version(&options.java).await?;
    let program = chunk_build::pin_program(&std::env::current_exe()?, &options.state.join("platform"))?;
    let (project, artifact) = build(&options)?;
    tracing::info!(deployment = %artifact.id, artifact = %artifact.directory.display(), "local project packaged");
    let mut processes = processes::Services::new(&options, &project, &artifact, program);
    let result = async {
        processes.start().await?;
        tracing::info!(address = %options.bind, state = %options.state.display(), "local project ready; Ctrl-C stops all services");
        loop {
            tokio::select! {
                result = &mut shutdown => return result,
                () = tokio::time::sleep(Duration::from_millis(250)) => processes.poll().await?,
            }
        }
    }.await;
    let stopped = processes.stop().await;
    result.and(stopped)
}

fn build(options: &Options) -> io::Result<(Project, Artifact)> {
    let file = options.project.canonicalize()?;
    let project: Project = serde_json::from_slice(&fs::read(&file)?).map_err(io::Error::other)?;
    let directory = file
        .parent()
        .ok_or_else(|| io::Error::other("project directory missing"))?;
    let artifact = chunk_build::publish(
        &Inputs {
            source: directory.join(&project.backend_source),
            contract: directory.join(&project.backend_contract),
            distribution: directory.join(&project.gameplay_distribution),
        },
        &options.state.join("artifacts"),
        &serde_json::to_vec(&project).map_err(io::Error::other)?,
    )?;
    let config = chunk_control::Config {
        deployment: chunk_proto::v1::DeploymentRef {
            environment: project.environment.clone(),
            deployment: artifact.id.clone(),
        },
        artifact_digest: artifact.id.clone(),
        profiles: project.profiles.clone(),
        session_types: project.session_types.clone(),
        max_processes: project.max_processes,
    };
    fs::write(
        options.state.join("control-config.json"),
        serde_json::to_vec(&config).map_err(io::Error::other)?,
    )?;
    Ok((project, artifact))
}

fn runner_lock(path: &std::path::Path) -> io::Result<fs::File> {
    let mut options = fs::File::options();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    file.try_lock()
        .map_err(|_| io::Error::other("another local runner owns this state directory"))?;
    Ok(file)
}

async fn java_version(java: &std::path::Path) -> io::Result<()> {
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(java).arg("-version").output(),
    )
    .await
    .map_err(io::Error::other)??;
    let version = String::from_utf8_lossy(&output.stderr);
    let major = version
        .split('"')
        .nth(1)
        .and_then(|v| v.split('.').next())
        .and_then(|v| v.parse::<u32>().ok());
    if !output.status.success() || major.is_none_or(|v| v < 25) {
        return Err(io::Error::other(
            "gameplay requires Java 25; use `just local` or supply --java",
        ));
    }
    Ok(())
}
