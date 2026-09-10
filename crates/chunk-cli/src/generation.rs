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
    /// Java package (defaults to dev.chunkzero.generated for the Java target).
    #[arg(long)]
    java_package: Option<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Target {
    Java,
    Typescript,
}

pub(crate) fn run(options: Options) -> io::Result<()> {
    chunk_build::project::inspect(&options.project)?;
    let (target, directory) = match options.target {
        Target::Java => (
            GenerationTarget::Java {
                package: options.java_package.as_deref().unwrap_or("dev.chunkzero.generated"),
            },
            "java",
        ),
        Target::Typescript => {
            if options.java_package.is_some() {
                return Err(io::Error::other("--java-package requires --target java"));
            }
            (GenerationTarget::TypeScript, "typescript")
        }
    };
    let output = options
        .output
        .unwrap_or_else(|| options.project.join(".chunk/generated").join(directory));
    let backend = options
        .backend_output
        .unwrap_or_else(|| options.project.join(".chunk/build/backend"));
    let output = destination(&output)?;
    let backend = destination(&backend)?;
    if output.starts_with(&backend) || backend.starts_with(&output) {
        return Err(io::Error::other(
            "client and compiler output directories must be separate",
        ));
    }
    cliclack::log::info("Generating backend clients…")?;
    chunk_build::compile(&options.project, &backend)?;
    chunk_build::generate(&backend.join("contract.json"), &output, target)?;
    cliclack::log::success(format!("Generated → {}", output.display()))
}

fn destination(path: &Path) -> io::Result<PathBuf> {
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
mod tests {
    use super::*;

    fn options(project: &Path) -> Options {
        Options {
            project: project.into(),
            target: Target::Java,
            output: None,
            backend_output: None,
            java_package: None,
        }
    }

    #[test]
    fn generation_validates_shared_metadata_and_output_separation_before_compiling() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("chunk.toml"), "domains = []").unwrap();
        let error = run(options(project.path())).unwrap_err();
        assert!(error.to_string().contains("unknown field"), "{error}");
        assert!(!project.path().join(".chunk").exists());
        std::fs::write(project.path().join("chunk.toml"), "").unwrap();
        let mut overlapping = options(project.path());
        overlapping.output = Some(project.path().join(".chunk/build/backend/../backend"));
        let error = run(overlapping).unwrap_err();
        assert!(error.to_string().contains("must be separate"), "{error}");
        assert!(!project.path().join(".chunk").exists());
    }
}
