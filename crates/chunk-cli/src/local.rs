use std::{
    fs, io,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use chunk_build::project::ProjectMetadata;
use tokio_util::sync::CancellationToken;

use crate::building;

mod services;

#[derive(clap::Args)]
pub(crate) struct Options {
    #[command(flatten)]
    build: building::Options,
    /// Local service state directory (defaults to PROJECT/.chunk/local).
    #[arg(long)]
    state: Option<PathBuf>,
    /// Override the Java executable selected by Gradle.
    #[arg(long)]
    java: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1:25565")]
    bind: SocketAddr,
    #[arg(long, default_value = "127.0.0.1:25568")]
    backend_bind: SocketAddr,
    #[arg(long, default_value = "127.0.0.1:25567")]
    control_bind: SocketAddr,
}

struct Settings {
    state: PathBuf,
    java: PathBuf,
    bind: SocketAddr,
    backend_bind: SocketAddr,
    control_bind: SocketAddr,
}

pub(crate) async fn run(options: Options) -> io::Result<()> {
    chunk_service::run(|stop| serve(options, stop)).await
}

async fn serve(options: Options, stop: CancellationToken) -> io::Result<()> {
    let project = building::prepare(&options.build)?;

    let state = options.state.unwrap_or_else(|| project.root.join(".chunk/local"));
    fs::create_dir_all(&state)?;
    let state = state.canonicalize()?;
    let _lock = runner_lock(&state.join("runner.lock"))?;
    available_addresses(options.bind, options.backend_bind, options.control_bind)?;
    let built = building::execute(&project, stop.clone()).await?;
    let java = options.java.map_or_else(|| Ok(built.java.executable), std::path::absolute)?;
    tokio::select! {
        () = stop.cancelled() => return building::cancelled(&stop),
        result = java_version(&java, built.java.version) => result?,
    }
    let mut control = control_config(&project.metadata, &built.release.id, &built.release.apps)?;
    let backend: chunk_contract::Deployment = chunk_service::read(&built.release.directory.join("backend.json"))?;
    backend.validate().map_err(io::Error::other)?;
    if backend.id != built.release.id {
        return Err(io::Error::other("published backend deployment differs from release"));
    }
    control.session_methods = backend.session_methods;
    control.session_configurations = backend.session_configurations;
    control.destinations = backend.destinations;
    fs::write(state.join("control-config.json"), serde_json::to_vec(&control).map_err(io::Error::other)?)?;
    tracing::info!(deployment = %built.release.id, "local project packaged");
    let settings = Settings {
        state,
        java,
        bind: options.bind,
        backend_bind: options.backend_bind,
        control_bind: options.control_bind,
    };
    services::run(&settings, &control, &built.release, stop).await
}

fn control_config(
    project: &ProjectMetadata,
    deployment: &str,
    apps: &[chunk_contract::AppArtifact],
) -> io::Result<chunk_control::Config> {
    let local =
        project.local.as_ref().ok_or_else(|| io::Error::other("chunk dev requires [local] settings in chunk.toml"))?;
    if project.apps.is_empty() {
        return Err(io::Error::other("chunk dev requires at least one discovered app"));
    }
    let session_types = apps
        .iter()
        .flat_map(|app| {
            app.sessions.iter().map(|(id, session)| {
                (
                    format!("{}/{id}", app.id),
                    chunk_control::SessionType {
                        app: app.id.clone(),
                        machine_profile: session.machine_profile.clone(),
                        capacity: session.capacity,
                    },
                )
            })
        })
        .collect();
    Ok(chunk_control::Config {
        destinations: None,
        session_methods: None,
        session_configurations: None,
        apps: apps.iter().map(|app| (app.id.clone(), app.clone())).collect(),
        deployment: chunk_proto::v1::DeploymentRef {
            environment: local.environment.clone(),
            deployment: deployment.into(),
        },
        artifact_digest: deployment.into(),
        profiles: local
            .profiles
            .iter()
            .map(|(name, profile)| {
                (
                    name.clone(),
                    chunk_control::MachineProfile {
                        memory_mib: profile.memory_mib,
                        max_sessions: profile.max_sessions,
                    },
                )
            })
            .collect(),
        session_types,
        max_processes: local.max_processes,
    })
}

fn available_addresses(bind: SocketAddr, backend: SocketAddr, control: SocketAddr) -> io::Result<()> {
    if bind == backend || bind == control || backend == control {
        return Err(io::Error::other("local service addresses must differ"));
    }
    for address in [bind, backend, control] {
        if !address.ip().is_loopback() || address.port() == 0 {
            return Err(io::Error::other("local runner requires fixed loopback ports"));
        }
        std::net::TcpListener::bind(address).map_err(|error| {
            io::Error::other(format!("{address} is unavailable; stop the existing server first: {error}"))
        })?;
    }
    Ok(())
}

fn runner_lock(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::File::options();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    file.try_lock().map_err(|_| io::Error::other("another local runner owns this state directory"))?;
    Ok(file)
}

async fn java_version(java: &Path, required: u32) -> io::Result<()> {
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(java).arg("-version").kill_on_drop(true).output(),
    )
    .await
    .map_err(io::Error::other)??;
    let version = format!("{}\n{}", String::from_utf8_lossy(&output.stderr), String::from_utf8_lossy(&output.stdout));
    let major = version.lines().find_map(|line| {
        let version = line
            .strip_prefix("openjdk version ")
            .or_else(|| line.strip_prefix("java version "))
            .or_else(|| line.strip_prefix("openjdk "))
            .or_else(|| line.strip_prefix("java "))?;
        version.trim_start_matches('"').split(|ch: char| !ch.is_ascii_digit()).next()?.parse::<u32>().ok()
    });
    if !output.status.success() || major.is_none_or(|version| version < required) {
        return Err(io::Error::other(format!(
            "{} does not provide the Java {required}+ required by this release; select a compatible Gradle toolchain or --java executable",
            java.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
