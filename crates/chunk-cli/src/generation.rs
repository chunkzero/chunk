use std::{
    io,
    path::{Component, Path, PathBuf},
};

use chunk_build::GenerationTarget;
use clap::{Args, ValueEnum};

#[derive(Args)]
pub(crate) struct Options {
    #[arg(default_value = ".")]
    project: PathBuf,
    /// Client source language to generate.
    #[arg(long, value_enum)]
    target: Target,
    /// Client output directory (defaults to PROJECT/.chunk/generated/TARGET).
    #[arg(long)]
    output: Option<PathBuf>,
    /// Compiler artifact/cache directory (defaults to PROJECT/.chunk/build/backend).
    #[arg(long)]
    backend_output: Option<PathBuf>,
    /// Shared JVM package (defaults to dev.chunkzero.generated for Java/Kotlin targets).
    #[arg(long)]
    java_package: Option<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Target {
    Java,
    Kotlin,
    Typescript,
}

pub(crate) fn run(options: Options) -> io::Result<()> {
    chunk_build::project::inspect(&options.project)?;
    let (target, directory) = match options.target {
        Target::Java => (
            GenerationTarget::Java { package: options.java_package.as_deref().unwrap_or("dev.chunkzero.generated") },
            "java",
        ),
        Target::Kotlin => (
            GenerationTarget::Kotlin { package: options.java_package.as_deref().unwrap_or("dev.chunkzero.generated") },
            "kotlin",
        ),
        Target::Typescript => {
            if options.java_package.is_some() {
                return Err(io::Error::other("--java-package requires --target java or kotlin"));
            }
            (GenerationTarget::TypeScript, "typescript")
        }
    };
    let output = options.output.unwrap_or_else(|| options.project.join(".chunk/generated").join(directory));
    let backend = options.backend_output.unwrap_or_else(|| options.project.join(".chunk/build/backend"));
    let output = destination(&output)?;
    let backend = destination(&backend)?;
    if output.starts_with(&backend) || backend.starts_with(&output) {
        return Err(io::Error::other("client and compiler output directories must be separate"));
    }
    cliclack::log::info("Generating backend clients…")?;
    chunk_build::compile(&options.project, &backend)?;
    chunk_build::generate(&backend.join("contract.json"), &output, target)?;
    cliclack::log::success(format!("Generated → {}", output.display()))
}

pub(super) fn destination(path: &Path) -> io::Result<PathBuf> {
    let mut resolved = PathBuf::new();
    for component in std::path::absolute(path)?.components() {
        if component == Component::ParentDir {
            resolved.pop();
        } else {
            resolved.push(component);
            match resolved.canonicalize() {
                Ok(path) => resolved = path,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests;
