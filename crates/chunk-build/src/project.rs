//! Project manifests and the shared app inventory used by compiler and build tools.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

use chunk_contract::DomainScope;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{MachineProfile, valid_id};

pub(crate) mod assets;
pub(crate) mod authoring;

pub use assets::{ChunkRange, Pack, World, WorldFormat};

/// Apps, domain scopes and authored modules found in one pass over the project tree.
#[derive(Default)]
pub(crate) struct Inventory {
    pub apps: Vec<AppMetadata>,
    pub scopes: BTreeMap<String, DomainScope>,
    pub modules: Vec<authoring::Module>,
    /// Each scope's packs, by scope path.
    pub packs: BTreeMap<String, BTreeMap<String, Pack>>,
    pub local: Option<LocalConfig>,
    pub env: chunk_contract::EnvManifest,
}

#[derive(Debug, Serialize)]
pub struct ProjectMetadata {
    pub version: u32,
    pub apps: Vec<AppMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local: Option<LocalConfig>,
    /// Each scope's packs, by scope path.
    #[serde(skip)]
    pub scope_packs: BTreeMap<String, BTreeMap<String, Pack>>,
}

#[derive(Debug, Serialize)]
pub struct AppMetadata {
    pub id: String,
    /// Project-relative path with forward slashes.
    pub directory: String,
    pub gradle_project: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub domain: String,
    pub runtime: RuntimeRequirements,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub sessions: BTreeMap<String, RuntimeRequirements>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub worlds: BTreeMap<String, World>,
    /// The app's own packs; its scopes' packs apply too.
    #[serde(skip)]
    pub packs: BTreeMap<String, Pack>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeRequirements {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u32>,
    /// An implementation's `reconnect` option; unset for an app's runtime.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconnect: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalConfig {
    pub environment: String,
    pub machine_profile: String,
    pub capacity: u32,
    pub max_processes: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_node_timeout_seconds: Option<u32>,
    pub profiles: BTreeMap<String, MachineProfile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectManifest {
    local: Option<LocalConfig>,
    #[serde(default)]
    vars: BTreeMap<String, String>,
    #[serde(default)]
    env: BTreeMap<String, EnvironmentSection>,
    #[serde(default)]
    secrets: SecretsSection,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentSection {
    vars: BTreeMap<String, String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretsSection {
    required: BTreeSet<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppManifest {
    #[serde(default)]
    runtime: RuntimeRequirements,
    #[serde(default)]
    sessions: BTreeMap<String, RuntimeRequirements>,
}

/// Reads `chunk.toml` and discovered app manifests without executing project code or build tools.
/// Local defaults are resolved into each app's runtime requirements. Serialized paths are relative
/// to the project directory; JVM toolchain requirements are supplied separately by Gradle.
/// # Errors
/// Rejects invalid manifests, missing app build files, unknown profiles and unsupported local limits.
pub fn inspect(root: &Path) -> io::Result<ProjectMetadata> {
    Ok(metadata(inspect_inventory(root)?))
}

/// `inspect` for declarations whose world and pack sources may be missing, as in a checkout that has yet to fetch
/// them. Sources must still be relative paths inside `assets/`.
/// # Errors
/// Rejects what `inspect` does, except for missing or malformed sources.
pub fn inspect_declarations(root: &Path) -> io::Result<ProjectMetadata> {
    Ok(metadata(inventory(root, false)?))
}

fn metadata(inventory: Inventory) -> ProjectMetadata {
    ProjectMetadata { version: 1, apps: inventory.apps, local: inventory.local, scope_packs: inventory.packs }
}

/// `inspect`, keeping the scopes and authored modules discovered along the way.
pub(crate) fn inspect_inventory(root: &Path) -> io::Result<Inventory> {
    inventory(root, true)
}

fn inventory(root: &Path, require_sources: bool) -> io::Result<Inventory> {
    let manifest_path = root.join("chunk.toml");
    // Room for the variables `[vars]` and `[env.<name>.vars]` may hold.
    let manifest: ProjectManifest = read_manifest(&manifest_path, 1024 * 1024)?;
    let mut inventory = discover_with(root, require_sources)?;
    if let Some(local) = &manifest.local {
        local.validate(&manifest_path)?;
        for app in &mut inventory.apps {
            app.runtime.machine_profile.get_or_insert_with(|| local.machine_profile.clone());
            app.runtime.capacity.get_or_insert(local.capacity);
            for requirements in std::iter::once(&app.runtime).chain(app.sessions.values()) {
                if let Some(profile) = &requirements.machine_profile
                    && !local.profiles.contains_key(profile)
                {
                    return Err(invalid(
                        &app_manifest_path(root, app),
                        format!("machine_profile references unknown profile {profile:?}"),
                    ));
                }
            }
        }
    } else if let Some(app) = inventory.apps.iter().find(|app| {
        std::iter::once(&app.runtime)
            .chain(app.sessions.values())
            .any(|requirements| requirements.machine_profile.is_some())
    }) {
        return Err(invalid(&app_manifest_path(root, app), "machine_profile requires profiles in chunk.toml [local]"));
    }
    inventory.env = chunk_contract::EnvManifest {
        vars: manifest.vars,
        environments: manifest.env.into_iter().map(|(name, section)| (name, section.vars)).collect(),
        secrets: manifest.secrets.required,
    };
    inventory.env.validate().map_err(|error| invalid(&manifest_path, error))?;
    inventory.local = manifest.local;
    Ok(inventory)
}

/// Discovers the project, resolving local defaults only when `chunk.toml` exists.
pub(crate) fn load(root: &Path) -> io::Result<Inventory> {
    if root.join("chunk.toml").exists() { inspect_inventory(root) } else { discover(root) }
}

/// Discovers recursive `apps/**/app.ts` declarations and legacy immediate `apps/*/app.toml` children, plus the
/// scopes and modules authored under `apps/`. Unmanifested directories are ignored.
pub(crate) fn discover(root: &Path) -> io::Result<Inventory> {
    discover_with(root, true)
}

fn discover_with(root: &Path, require_sources: bool) -> io::Result<Inventory> {
    let domains = root.join("server/domains");
    if fs::symlink_metadata(&domains).is_ok() {
        return Err(invalid(
            &domains,
            "server/domains is no longer supported; declare scopes in apps/**/scope.ts and bind hooks and commands in defineScope or defineApp",
        ));
    }
    let mut inventory = authoring::discover(root, require_sources)?;
    legacy_apps(root, &mut inventory)?;
    assets::check_unique_packs(root, &inventory)?;
    Ok(inventory)
}

fn legacy_apps(root: &Path, inventory: &mut Inventory) -> io::Result<()> {
    let directory = root.join("apps");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(invalid(&directory, "expected a directory, not a symlink or file"));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(invalid(&directory, error)),
        _ => {}
    }
    let mut entries = fs::read_dir(&directory)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
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
        let id = entry.file_name().into_string().map_err(|_| invalid(&entry.path(), "app IDs must be UTF-8"))?;
        if !valid_id(&id) {
            return Err(invalid(
                &manifest_path,
                "app directory ID must be an ASCII identifier of at most 128 characters",
            ));
        }
        if !names.insert(id.to_ascii_lowercase()) {
            return Err(invalid(&manifest_path, "app IDs must not differ only by case"));
        }
        let manifest: AppManifest = read_manifest(&manifest_path, 65_536)?;
        if manifest.sessions.len() > 128 || manifest.sessions.keys().any(|id| !valid_id(id)) {
            return Err(invalid(&manifest_path, "sessions requires at most 128 valid session type IDs"));
        }
        for requirements in std::iter::once(&manifest.runtime).chain(manifest.sessions.values()) {
            if requirements.capacity.is_some_and(|capacity| !(1..=128).contains(&capacity)) {
                return Err(invalid(&manifest_path, "capacity must be between 1 and 128"));
            }
        }
        require_file(&entry.path().join("build.gradle.kts"))?;
        inventory.apps.push(AppMetadata {
            directory: format!("apps/{id}"),
            gradle_project: format!(":apps:{id}"),
            id,
            domain: String::new(),
            runtime: manifest.runtime,
            sessions: manifest.sessions,
            worlds: BTreeMap::new(),
            packs: BTreeMap::new(),
        });
    }
    inventory.apps.sort_by(|left, right| left.id.cmp(&right.id));
    let mut names = BTreeSet::new();
    for app in &inventory.apps {
        if !names.insert(app.id.to_ascii_lowercase()) {
            return Err(invalid(&root.join(&app.directory), "app IDs must be unique and must not differ only by case"));
        }
    }
    Ok(())
}

fn app_manifest_path(root: &Path, app: &AppMetadata) -> PathBuf {
    let directory = root.join(&app.directory);
    if directory.join("app.ts").exists() { directory.join("app.ts") } else { directory.join("app.toml") }
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
        if self.idle_node_timeout_seconds.is_some_and(|seconds| seconds > 3600) {
            return Err(invalid(path, "local.idle_node_timeout_seconds must be between 0 and 3600"));
        }
        if !self.profiles.contains_key(&self.machine_profile) {
            return Err(invalid(
                path,
                format!("local.machine_profile references unknown profile {:?}", self.machine_profile),
            ));
        }
        for (name, profile) in &self.profiles {
            if !MachineProfile::valid(name, profile) {
                return Err(invalid(
                    path,
                    format!(
                        "local.profiles.{name} requires a 1–128 byte name, memory_mib between 128 and 8192 and max_sessions between 1 and 16"
                    ),
                ));
            }
        }
        Ok(())
    }
}

fn require_file(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(invalid(path, "expected a regular file, not a symlink or directory")),
        Err(error) => Err(invalid(path, error)),
    }
}

fn read_manifest<T: DeserializeOwned>(path: &Path, limit: u64) -> io::Result<T> {
    require_file(path)?;
    let bytes = super::read_limited(path, limit).map_err(|error| invalid(path, error))?;
    let source = std::str::from_utf8(&bytes).map_err(|error| invalid(path, error))?;
    toml::from_str(source).map_err(|error| invalid(path, error))
}

/// Tool-owned directory names that never hold authored sources.
pub(crate) const GENERATED: [&str; 3] = ["node_modules", "_generated", ".chunk"];

pub(crate) struct Child {
    pub name: String,
    pub path: PathBuf,
    pub kind: fs::FileType,
}

/// Name-sorted children of a real directory, without skipped names; a missing directory has none.
/// `what` names the inputs in errors. Symlinks are rejected rather than followed.
pub(crate) fn children(directory: &Path, what: &str, skip: impl Fn(&str) -> bool) -> io::Result<Vec<Child>> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(invalid(directory, format!("{what} directories cannot be files or symlinks")));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(invalid(directory, error)),
        _ => {}
    }
    let mut entries = fs::read_dir(directory)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    let mut children = Vec::new();
    for entry in entries {
        let path = entry.path();
        let name =
            entry.file_name().into_string().map_err(|_| invalid(&path, format!("{what} paths must be UTF-8")))?;
        if skip(&name) {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(invalid(&path, format!("{what} symlinks are unsupported")));
        }
        children.push(Child { name, path, kind });
    }
    Ok(children)
}

fn invalid(path: &Path, error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests;
