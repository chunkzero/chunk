use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::Deserialize;

pub(super) struct Toolchain {
    pub source: Option<PathBuf>,
    pub versions: Versions,
}

#[derive(Deserialize)]
pub(super) struct Versions {
    pub chunk: String,
    pub kotlin: String,
    pub foojay: String,
}

#[derive(Deserialize)]
struct Catalog {
    versions: Versions,
}

impl Toolchain {
    /// Maven repository serving the pinned Chunk version; nightlies are published apart from releases.
    pub fn repository(&self) -> &'static str {
        if self.versions.chunk.contains("-nightly.") {
            "https://maven.chunkzero.com/nightlies"
        } else {
            "https://maven.chunkzero.com"
        }
    }

    pub fn resolve(source: Option<&Path>) -> io::Result<Self> {
        let source = source.map(Path::canonicalize).transpose()?;
        let catalog = if let Some(source) = &source {
            for name in super::WRAPPER
                .iter()
                .map(|(name, _)| *name)
                .chain(["jvm/gradle-plugin/settings.gradle.kts", "jvm/runtime-minestom/build.gradle.kts"])
            {
                if !source.join(name).is_file() {
                    return Err(io::Error::new(io::ErrorKind::NotFound, format!("Chunk checkout is missing {name}")));
                }
            }
            fs::read_to_string(source.join("gradle/libs.versions.toml"))?
        } else {
            include_str!("../../../../gradle/libs.versions.toml").to_owned()
        };
        let catalog: Catalog =
            toml::from_str(&catalog).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let cli_version = env!("CARGO_PKG_VERSION");
        if catalog.versions.chunk != cli_version {
            let origin = if source.is_some() { "checkout" } else { "embedded SDK" };
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("CLI and {origin} versions must match: CLI {cli_version}, {origin} {}", catalog.versions.chunk),
            ));
        }
        let mut versions = catalog.versions;
        // A checkout's included builds replace the published plugin and libraries, whatever their version.
        if source.is_none() {
            crate::RELEASE_VERSION.clone_into(&mut versions.chunk);
        }
        Ok(Self { source, versions })
    }
}
