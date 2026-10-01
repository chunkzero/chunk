use std::{
    fs,
    io::{self, IsTerminal},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use chunk_build::project::ProjectMetadata;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::building;
use report::Reporter;

mod dev_vars;
mod logs;
mod plain;
mod reload;
mod report;
mod services;
mod session;
mod tui;

#[derive(clap::Args)]
pub(crate) struct Options {
    #[arg(default_value = ".")]
    project: PathBuf,
    /// Local service state directory (defaults to PROJECT/.chunk/local).
    #[arg(long)]
    state: Option<PathBuf>,
    /// Override the Java executable selected by Gradle.
    #[arg(long)]
    java: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1:25565")]
    bind: SocketAddr,
    /// Control address, shared by every release.
    #[arg(long, default_value = "127.0.0.1:25567")]
    control_bind: SocketAddr,
    /// Print plain progress lines instead of the terminal UI (automatic when stdout is not a terminal).
    #[arg(long)]
    plain: bool,
    /// Rebuild only on an explicit restart instead of watching project sources.
    #[arg(long)]
    no_watch: bool,
    /// After a JVM change, disconnect players still on the old release after this many seconds.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(0..=120))]
    drain_seconds: u64,
    /// Accept logins without Mojang authentication, using offline-mode UUIDs. For local testing only.
    #[arg(long)]
    offline_logins: bool,
    /// Read the variables chunk.toml's `[env.NAME.vars]` overrides, rather than only its top-level `[vars]`.
    #[arg(long = "env", value_name = "NAME")]
    environment: Option<String>,
}

struct Settings {
    state: PathBuf,
    /// The Java executable every release's JVMs run with.
    java: PathBuf,
    bind: SocketAddr,
    control_bind: SocketAddr,
    offline_logins: bool,
    /// Selects the `[env.<name>.vars]` deployments read.
    environment_name: Option<String>,
}

/// A packaged release checked against its Java runtime and projected into a control release.
pub(super) struct Staged {
    release: chunk_build::Release,
    java: PathBuf,
    control: chunk_control::Release,
    bundle: chunk_contract::Deployment,
    /// The project's `.dev.vars`, read as it was built.
    secrets: chunk_backend::Secrets,
}

/// Requests from the terminal UI or plain-mode input.
pub(super) enum Command {
    /// Rebuild and replace every running release immediately.
    Restart,
    /// Move a player served by the `deployment` release to another session.
    MovePlayer { deployment: String, player: String, name: String, demand: chunk_proto::sync::v1::SessionDemand },
}

pub(crate) async fn run(options: Options) -> io::Result<()> {
    let interactive = !options.plain && io::stdout().is_terminal();
    let (reporter, events) = Reporter::new();
    let (commands, requests) = mpsc::unbounded_channel();
    if interactive {
        logs::tui(reporter.clone());
    } else {
        logs::plain();
        plain::read_commands(commands.clone());
        plain::exit_on_second_interrupt();
    }
    chunk_service::run(|stop| async move {
        let finished = CancellationToken::new();
        let ui = if interactive {
            let (stop, finished) = (stop.clone(), finished.clone());
            let info = tui::Info {
                title: format!("chunk dev · {}", options.project.display()),
                address: options.bind,
                watching: !options.no_watch,
            };
            tokio::task::spawn_blocking(move || tui::run(events, &commands, &info, &stop, &finished))
        } else {
            let finished = finished.clone();
            tokio::spawn(async move {
                plain::render(events, finished).await;
                Ok(())
            })
        };
        let result = serve(options, interactive, reporter, requests, stop).await;
        finished.cancel();
        let shown = ui.await.map_err(io::Error::other)?;
        result.and(shown)
    })
    .await
}

async fn serve(
    options: Options,
    interactive: bool,
    reporter: Reporter,
    commands: mpsc::UnboundedReceiver<Command>,
    stop: CancellationToken,
) -> io::Result<()> {
    let root = options.project.canonicalize()?;
    fs::create_dir_all(root.join(".chunk"))?;
    let _project_lock = runner_lock(&root.join(crate::cleaning::PROJECT_LOCK))
        .map_err(|_| io::Error::other("chunk dev is already running for this project"))?;
    let state = options.state.clone().unwrap_or_else(|| root.join(".chunk/local"));
    fs::create_dir_all(&state)?;
    let state = state.canonicalize()?;
    let project = building::inspect(root, state.join("releases"))?;
    reporter.done("Project", project_summary(&project));
    let _lock = runner_lock(&state.join("runner.lock"))?;
    if interactive {
        tokio::spawn(logs::follow_jvms(state.join("control").join("nodes"), reporter.clone(), stop.clone()));
    }
    available_addresses(options.bind, options.control_bind)?;
    reporter.running("Build", "Gradle chunkArtifacts");
    let started = Instant::now();
    let built = building::execute(&project, building::BuildMode::Dev, stop.clone(), reporter.build_progress()).await?;
    reporter.done("Build", format!("{} · release {}", report::seconds(started.elapsed()), short(&built.release.id)));
    let required = built.java.version;
    reporter.running("Java", format!("Checking Java {required}+"));
    let staged = stage(&project, built, options.java.as_deref(), &stop).await?;
    reporter.done("Java", format!("{required}+ · {}", staged.java.display()));
    let settings = Settings {
        state,
        java: staged.java.clone(),
        bind: options.bind,
        control_bind: options.control_bind,
        offline_logins: options.offline_logins,
        environment_name: options.environment.clone(),
    };
    let watched = if options.no_watch {
        None
    } else {
        let ignored = vec![project.output.clone(), settings.state.clone()];
        Some(reload::watch(&project.root, &ignored).map_err(io::Error::other)?)
    };
    let environment = staged.control.deployment.environment.clone();
    let (shared, version) = services::start(&settings, staged, &reporter).await?;
    let session = session::Session::new(&settings, &options, &reporter, environment, shared, version);
    session.run(watched, commands, stop).await
}

/// Exits without waiting for every JVM to confirm it stopped. The next `chunk dev` stops the JVMs left running.
fn force_exit() -> ! {
    eprintln!("chunk dev exited; JVMs may still be stopping");
    std::process::exit(130);
}

/// Checks the release's Java requirement and projects it into a control release.
async fn stage(
    project: &building::Project,
    built: building::Built,
    java: Option<&Path>,
    stop: &CancellationToken,
) -> io::Result<Staged> {
    let java = java.map_or_else(|| Ok(built.java.executable), std::path::absolute)?;
    tokio::select! {
        () = stop.cancelled() => building::cancelled(stop)?,
        result = java_version(&java, built.java.version) => result?,
    }
    let secrets = dev_vars::read(&project.root)?;
    let mut bundle: chunk_contract::Deployment = chunk_service::read(&built.release.directory.join("backend.json"))?;
    let missing: Vec<_> = bundle.contracts.env.secrets.iter().filter(|name| secrets.get(name).is_none()).collect();
    if !missing.is_empty() {
        let missing = missing.iter().map(|name| name.as_str()).collect::<Vec<_>>().join(", ");
        tracing::warn!("required secrets missing from {}: {missing}", dev_vars::FILE);
    }
    let deployment = version(&built.release.id);
    let mut control = control_config(&project.metadata, &built.release.id, &deployment, &built.release.apps)?;
    bundle.validate().map_err(io::Error::other)?;
    if bundle.id != built.release.id {
        return Err(io::Error::other("published backend deployment differs from release"));
    }
    bundle.id = deployment;
    let contracts = bundle.contracts.clone();
    control.contracts = chunk_control::Contracts {
        session_methods: contracts.session_methods,
        session_configurations: contracts.session_configurations,
        destinations: contracts.destinations,
    };
    Ok(Staged { release: built.release, java, control, bundle, secrets })
}

fn project_summary(project: &building::Project) -> String {
    let name =
        project.root.file_name().map_or_else(|| project.root.display().to_string(), |n| n.to_string_lossy().into());
    match project.metadata.apps.len() {
        1 => format!("{name} · 1 app"),
        apps => format!("{name} · {apps} apps"),
    }
}

/// A local deployment version of `release`; the backend never reuses a released version ID.
fn version(release: &str) -> String {
    let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
    format!("{release}-{millis}")
}

/// Abbreviates a content-addressed release ID for display.
fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

fn control_config(
    project: &ProjectMetadata,
    release: &str,
    deployment: &str,
    apps: &[chunk_contract::AppArtifact],
) -> io::Result<chunk_control::Release> {
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
    Ok(chunk_control::Release {
        contracts: chunk_control::Contracts::default(),
        apps: apps.iter().map(|app| (app.id.clone(), app.clone())).collect(),
        deployment: chunk_proto::control::v1::DeploymentRef {
            environment: local.environment.clone(),
            deployment: deployment.into(),
        },
        release_id: release.into(),
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
        idle_node_timeout_seconds: local
            .idle_node_timeout_seconds
            .unwrap_or(chunk_control::DEFAULT_IDLE_NODE_TIMEOUT_SECONDS),
    })
}

fn available_addresses(bind: SocketAddr, control: SocketAddr) -> io::Result<()> {
    if bind == control {
        return Err(io::Error::other("local service addresses must differ"));
    }
    for address in [bind, control] {
        if !address.ip().is_loopback() || address.port() == 0 {
            return Err(io::Error::other("local runner requires fixed loopback ports"));
        }
        std::net::TcpListener::bind(address).map_err(|error| {
            io::Error::other(format!("{address} is unavailable; stop the existing server first: {error}"))
        })?;
    }
    Ok(())
}

pub(crate) fn runner_lock(path: &Path) -> io::Result<fs::File> {
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
    let mut command = tokio::process::Command::new(java);
    chunk_service::withhold_platform_env(command.as_std_mut());
    let output = tokio::time::timeout(Duration::from_secs(5), command.arg("-version").kill_on_drop(true).output())
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
