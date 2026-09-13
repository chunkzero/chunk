use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::Deserialize;

pub(super) struct Toolchain {
    pub wrapper: PathBuf,
    pub source: Option<PathBuf>,
    pub versions: Versions,
    pub repository: String,
}

#[derive(Deserialize)]
pub(super) struct Versions {
    pub chunk: String,
    pub kotlin: String,
    pub foojay: String,
}

#[derive(Deserialize)]
struct Sdk {
    schema: u32,
    version: String,
    kotlin_version: String,
    foojay_version: String,
    maven_repository: String,
}

#[derive(Deserialize)]
struct Catalog {
    versions: Versions,
}

impl Toolchain {
    pub fn resolve(source: Option<&Path>, executable: &Path) -> io::Result<Self> {
        let toolchain = if let Some(source) = source {
            let source = source.canonicalize()?;
            for name in ["jvm/gradle-plugin/settings.gradle.kts", "jvm/runtime-minestom/build.gradle.kts"] {
                if !source.join(name).is_file() {
                    return Err(io::Error::new(io::ErrorKind::NotFound, format!("Chunk checkout is missing {name}")));
                }
            }
            let catalog: Catalog = toml::from_str(&fs::read_to_string(source.join("gradle/libs.versions.toml"))?)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            Self {
                wrapper: source.clone(),
                source: Some(source),
                versions: catalog.versions,
                repository: "https://maven.chunkzero.com".into(),
            }
        } else {
            let root = executable.parent().ok_or_else(|| io::Error::other("CLI has no parent directory"))?;
            let metadata = fs::read(root.join("sdk.json")).map_err(|error| {
                io::Error::new(error.kind(), format!(
                    "cannot read SDK metadata next to {}: {error}. Install a Chunk SDK, or use --chunk-source CHECKOUT for framework development",
                    executable.display(),
                ))
            })?;
            let sdk: Sdk =
                serde_json::from_slice(&metadata).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if sdk.schema != 1 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Unsupported SDK metadata schema"));
            }
            Self {
                wrapper: root.join("sdk/wrapper"),
                source: None,
                versions: Versions { chunk: sdk.version, kotlin: sdk.kotlin_version, foojay: sdk.foojay_version },
                repository: sdk.maven_repository,
            }
        };
        let cli_version = env!("CARGO_PKG_VERSION");
        if toolchain.versions.chunk != cli_version {
            let origin = if toolchain.source.is_some() { "checkout" } else { "SDK" };
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "CLI and {origin} versions must match: CLI {cli_version}, {origin} {}",
                    toolchain.versions.chunk,
                ),
            ));
        }
        for name in super::WRAPPER {
            if !toolchain.wrapper.join(name).is_file() {
                return Err(io::Error::new(io::ErrorKind::NotFound, format!("Chunk toolchain is missing {name}")));
            }
        }
        Ok(toolchain)
    }
}
