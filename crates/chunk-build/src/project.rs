//! Project manifests and the shared app inventory used by compiler and build tools.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::Path,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

#[derive(Debug, Serialize)]
pub struct ProjectMetadata {
    pub version: u32,
    pub apps: Vec<AppMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local: Option<LocalConfig>,
}

#[derive(Debug, Serialize)]
pub struct AppMetadata {
    pub id: String,
    /// Project-relative path with forward slashes.
    pub directory: String,
    pub gradle_project: String,
    pub runtime: RuntimeRequirements,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeRequirements {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalConfig {
    pub environment: String,
    pub machine_profile: String,
    pub capacity: u32,
    pub max_processes: u16,
    pub profiles: BTreeMap<String, MachineProfile>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MachineProfile {
    pub memory_mib: u32,
    pub max_sessions: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectManifest {
    local: Option<LocalConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppManifest {
    #[serde(default)]
    runtime: RuntimeRequirements,
}

/// Reads `chunk.toml` and discovered app manifests without executing project code or build tools.
/// Local defaults are resolved into each app's runtime requirements. Serialized paths are relative
/// to the project directory; JVM toolchain requirements are supplied separately by Gradle.
/// # Errors
/// Rejects invalid manifests, missing app build files, unknown profiles and unsupported local limits.
pub fn inspect(root: &Path) -> io::Result<ProjectMetadata> {
    let manifest_path = root.join("chunk.toml");
    let manifest: ProjectManifest = read_manifest(&manifest_path)?;
    let mut apps = discover_apps(root)?;
    if let Some(local) = &manifest.local {
        local.validate(&manifest_path)?;
        for app in &mut apps {
            let profile = app
                .runtime
                .machine_profile
                .get_or_insert_with(|| local.machine_profile.clone());
            if !local.profiles.contains_key(profile) {
                return Err(invalid(
                    &root.join(&app.directory).join("app.toml"),
                    format!("runtime.machine_profile references unknown profile {profile:?}"),
                ));
            }
            app.runtime.capacity.get_or_insert(local.capacity);
        }
    } else if let Some(app) = apps.iter().find(|app| app.runtime.machine_profile.is_some()) {
        return Err(invalid(
            &root.join(&app.directory).join("app.toml"),
            "runtime.machine_profile requires profiles in chunk.toml [local]",
        ));
    }
    Ok(ProjectMetadata {
        version: 1,
        apps,
        local: manifest.local,
    })
}

/// Discovers sorted immediate `apps/*/app.toml` children using their directory names as app IDs.
/// Unmanifested directories are ignored. This inventory does not require a root project manifest.
/// # Errors
/// Rejects symlinks, malformed manifests, invalid or case-colliding IDs and missing app build files.
pub fn discover_apps(root: &Path) -> io::Result<Vec<AppMetadata>> {
    let directory = root.join("apps");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(invalid(&directory, "expected a directory, not a symlink or file"));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(invalid(&directory, error)),
        _ => {}
    }
    let mut entries = fs::read_dir(&directory)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    let mut apps = Vec::new();
    let mut names = BTreeSet::new();
    for entry in entries {
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(invalid(&entry.path(), "app symlinks are unsupported"));
        }
        if !kind.is_dir() {
            continue;
        }
        let manifest_path = entry.path().join("app.toml");
        match fs::symlink_metadata(&manifest_path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(invalid(&manifest_path, error)),
            Ok(_) => {}
        }
        let id = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid(&entry.path(), "app IDs must be UTF-8"))?;
        if !valid_id(&id) {
            return Err(invalid(
                &manifest_path,
                "app directory ID must be an ASCII identifier of at most 128 characters",
            ));
        }
        if !names.insert(id.to_ascii_lowercase()) {
            return Err(invalid(&manifest_path, "app IDs must not differ only by case"));
        }
        let manifest: AppManifest = read_manifest(&manifest_path)?;
        if manifest
            .runtime
            .capacity
            .is_some_and(|capacity| !(1..=128).contains(&capacity))
        {
            return Err(invalid(&manifest_path, "runtime.capacity must be between 1 and 128"));
        }
        require_file(&entry.path().join("build.gradle.kts"))?;
        apps.push(AppMetadata {
            directory: format!("apps/{id}"),
            gradle_project: format!(":apps:{id}"),
            id,
            runtime: manifest.runtime,
        });
    }
    Ok(apps)
}

impl LocalConfig {
    fn validate(&self, path: &Path) -> io::Result<()> {
        if self.environment.is_empty() || self.environment.len() > 100 {
            return Err(invalid(path, "local.environment must contain between 1 and 100 bytes"));
        }
        if !(1..=128).contains(&self.capacity) {
            return Err(invalid(path, "local.capacity must be between 1 and 128"));
        }
        if !(1..=32).contains(&self.max_processes) {
            return Err(invalid(path, "local.max_processes must be between 1 and 32"));
        }
        if !self.profiles.contains_key(&self.machine_profile) {
            return Err(invalid(
                path,
                format!(
                    "local.machine_profile references unknown profile {:?}",
                    self.machine_profile
                ),
            ));
        }
        for (name, profile) in &self.profiles {
            if name.is_empty() || name.len() > 128 {
                return Err(invalid(
                    path,
                    "local profile names must contain between 1 and 128 bytes",
                ));
            }
            if !(128..=8192).contains(&profile.memory_mib) || !(1..=16).contains(&profile.max_sessions) {
                return Err(invalid(
                    path,
                    format!(
                        "local.profiles.{name} requires memory_mib between 128 and 8192 and max_sessions between 1 and 16"
                    ),
                ));
            }
        }
        Ok(())
    }
}

fn valid_id(id: &str) -> bool {
    id.len() <= 128
        && id
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn require_file(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(invalid(path, "expected a regular file, not a symlink or directory")),
        Err(error) => Err(invalid(path, error)),
    }
}

fn read_manifest<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    require_file(path)?;
    let bytes = super::read_limited(path, 65_536).map_err(|error| invalid(path, error))?;
    let source = std::str::from_utf8(&bytes).map_err(|error| invalid(path, error))?;
    toml::from_str(source).map_err(|error| invalid(path, error))
}

fn invalid(path: &Path, error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests;
