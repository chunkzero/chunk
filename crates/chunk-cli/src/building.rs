use std::{io, path::PathBuf};

use chunk_build::{JavaRuntime, Release, ReleaseInputs, project::ProjectMetadata};
use tokio_util::sync::CancellationToken;

mod gradle;

#[derive(Clone, clap::Args)]
#[group(id = "build")]
pub(crate) struct Options {
    #[arg(default_value = ".")]
    pub project: PathBuf,
    /// Release output directory (defaults to PROJECT/dist).
    #[arg(long)]
    pub output: Option<PathBuf>,
}

pub(crate) struct Project {
    pub root: PathBuf,
    pub metadata: ProjectMetadata,
    output: PathBuf,
}

pub(crate) struct Built {
    pub release: Release,
    pub java: JavaRuntime,
}

pub(crate) fn prepare(options: &Options) -> io::Result<Project> {
    let root = options.project.canonicalize()?;
    let metadata = chunk_build::project::inspect(&root)?;
    let output = crate::generation::destination(options.output.as_deref().unwrap_or(&root.join("dist")))?;
    for source in [root.join(".chunk"), root.join("server"), root.join("apps"), root.join("assets")] {
        let source = crate::generation::destination(&source)?;
        if output.starts_with(&source) || source.starts_with(&output) {
            return Err(io::Error::other("release output must be separate from project sources and build outputs"));
        }
    }
    Ok(Project { root, metadata, output })
}

pub(crate) async fn run(options: Options) -> io::Result<()> {
    chunk_service::run(|stop| async move {
        let project = prepare(&options)?;
        cliclack::log::info("Building application release…")?;
        let built = execute(&project, stop).await?;
        cliclack::log::success(format!("Built → {}", built.release.archive.display()))
    })
    .await
}

pub(crate) async fn execute(project: &Project, stop: CancellationToken) -> io::Result<Built> {
    cancelled(&stop)?;
    gradle::run(&project.root, &std::env::current_exe()?, &stop).await?;
    cancelled(&stop)?;
    let inputs = ReleaseInputs {
        project: project.root.clone(),
        backend: project.root.join(".chunk/build/backend"),
        jvm_descriptor: project.root.join(".chunk/build/jvm/artifacts.json"),
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
    Ok(built)
}

pub(super) fn cancelled(stop: &CancellationToken) -> io::Result<()> {
    if stop.is_cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "build cancelled"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
