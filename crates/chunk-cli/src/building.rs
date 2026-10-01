use std::{io, path::PathBuf, time::Instant};

use chunk_build::{JavaRuntime, Release, ReleaseInputs, project::ProjectMetadata};
use tokio_util::sync::CancellationToken;

mod gradle;
mod output;
pub(crate) mod progress;

use progress::{Event, Phase, Progress};

#[derive(Clone, clap::Args)]
#[group(id = "build")]
pub(crate) struct Options {
    #[arg(default_value = ".")]
    pub project: PathBuf,
    /// Release output directory (defaults to PROJECT/dist).
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// Fail instead of recording additive schema changes in server/migrations/, for CI.
    #[arg(long)]
    pub frozen: bool,
}

pub(crate) struct Project {
    pub root: PathBuf,
    pub metadata: ProjectMetadata,
    pub output: PathBuf,
}

/// How a build packages the project.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuildMode {
    /// Self-contained app JARs and a portable archive; Gradle's JVM stops once the build finishes.
    Release,
    /// Thin app JARs behind launcher JARs and no archive, from a Gradle daemon that exits after 15 idle minutes.
    Dev,
}

pub(crate) struct Built {
    pub release: Release,
    pub java: JavaRuntime,
}

pub(crate) fn prepare(options: &Options) -> io::Result<Project> {
    let root = options.project.canonicalize()?;
    let output = crate::generation::destination(options.output.as_deref().unwrap_or(&root.join("dist")))?;
    for source in [root.join(".chunk"), root.join("server"), root.join("apps"), root.join("assets")] {
        let source = crate::generation::destination(&source)?;
        if output.starts_with(&source) || source.starts_with(&output) {
            return Err(io::Error::other("release output must be separate from project sources and build outputs"));
        }
    }
    inspect(root, output)
}

/// Inspects the project at canonical `root`, whose releases publish into `output`.
pub(crate) fn inspect(root: PathBuf, output: PathBuf) -> io::Result<Project> {
    let metadata = chunk_build::project::inspect(&root)?;
    Ok(Project { root, metadata, output })
}

pub(crate) async fn run(options: Options) -> io::Result<()> {
    chunk_service::run(|stop| async move {
        let project = prepare(&options)?;
        if options.frozen {
            chunk_build::migrations::require_recorded(&project.root)?;
        }
        cliclack::log::info("Building application release…")?;
        let built = execute(&project, BuildMode::Release, stop, Progress::default()).await?;
        warn_irreversible(&project.root.join(".chunk/build/backend/contract.json"))?;
        let release = built.release.archive.as_ref().unwrap_or(&built.release.directory);
        cliclack::log::success(format!("Built → {}", release.display()))
    })
    .await
}

pub(crate) async fn execute(
    project: &Project,
    mode: BuildMode,
    stop: CancellationToken,
    progress: Progress,
) -> io::Result<Built> {
    cancelled(&stop)?;
    let started = Instant::now();
    progress.emit(Event::Started(Phase::Compile));
    gradle::run(&project.root, &std::env::current_exe()?, mode, &stop, &progress).await?;
    progress.emit(Event::Finished(Phase::Compile, started.elapsed()));
    cancelled(&stop)?;
    let started = Instant::now();
    progress.emit(Event::Started(Phase::Release));
    let inputs = ReleaseInputs {
        project: project.root.clone(),
        backend: project.root.join(".chunk/build/backend"),
        jvm_descriptor: project.root.join(".chunk/build/jvm/artifacts.json"),
        archive: mode == BuildMode::Release,
    };
    let output = project.output.clone();
    let built = tokio::task::spawn_blocking(move || {
        let descriptor = chunk_build::read_jvm_descriptor(&inputs.jvm_descriptor).map_err(|error| {
            io::Error::new(error.kind(), format!("Gradle JVM descriptor {}: {error}", inputs.jvm_descriptor.display()))
        })?;
        let release = chunk_build::publish_release(&inputs, &output)?;
        Ok::<_, io::Error>(Built { release, java: descriptor.java })
    })
    .await
    .map_err(io::Error::other)??;
    cancelled(&stop)?;
    progress.emit(Event::Finished(Phase::Release, started.elapsed()));
    Ok(built)
}

/// Warns about expand migrations whose removed fields have no `back`, so older deployments see them frozen.
fn warn_irreversible(contract: &std::path::Path) -> io::Result<()> {
    #[derive(serde::Deserialize)]
    struct Contract {
        #[serde(default)]
        migrations: Vec<chunk_contract::Migration>,
    }
    let contract: Contract = serde_json::from_slice(&std::fs::read(contract)?).map_err(io::Error::other)?;
    for migration in &contract.migrations {
        for (table, change) in &migration.tables {
            if migration.kind == chunk_contract::MigrationKind::Expand && !change.back && !change.removed.is_empty() {
                cliclack::log::warning(format!(
                    "Migration {} has no back for {table}; older deployments see {} frozen until it's finished",
                    migration.id,
                    change.removed.join(", ")
                ))?;
            }
        }
    }
    Ok(())
}

pub(super) fn cancelled(stop: &CancellationToken) -> io::Result<()> {
    if stop.is_cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "build cancelled"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
