use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
};

use chunk_contract::SessionDeclaration;

use super::{
    Manifest, Metadata, archive, content_digest, descriptor, directory, jars, launcher, read_jvm_descriptor, verify,
};
use crate::{
    BackendMetadata, project,
    publication::{self, Files, insert},
    read_limited,
};

/// Separate build outputs consumed by the complete release publisher.
pub struct ReleaseInputs {
    pub project: PathBuf,
    pub backend: PathBuf,
    pub jvm_descriptor: PathBuf,
    /// Also publish the portable `.tar.gz`; `chunk dev` runs releases from their directory alone.
    pub archive: bool,
}

pub struct Release {
    pub id: String,
    pub directory: PathBuf,
    pub archive: Option<PathBuf>,
    pub apps: Vec<chunk_contract::AppArtifact>,
}

/// Publishes one portable release directory and, when requested, its sibling gzip-compressed tar archive.
/// Identity covers normalized metadata and payloads, excluding derived IDs and the archive wrapper.
/// Apps with a descriptor classpath run from a launcher JAR over their thin JARs; others run their bundled JAR.
/// Repeated publication verifies existing bytes and never overwrites an immutable release.
/// Nothing is published unless the release passes [`verify_release`](super::verify_release)'s checks.
/// # Errors
/// Rejects inconsistent app/JAR identities, incompatible classpaths, invalid contracts, symlinks,
/// oversized inputs, nonportable paths and modified published content.
pub fn publish_release(inputs: &ReleaseInputs, dist: &Path) -> io::Result<Release> {
    let project = project::inspect_inventory(&inputs.project)?;
    let jvm = read_jvm_descriptor(&inputs.jvm_descriptor)?;
    let descriptor_apps: BTreeMap<_, _> = jvm.apps.iter().map(|app| (app.id.as_str(), app)).collect();
    let discovered: BTreeSet<_> = project.apps.iter().map(|app| app.id.as_str()).collect();
    if descriptor_apps.len() != jvm.apps.len() || descriptor_apps.keys().copied().collect::<BTreeSet<_>>() != discovered
    {
        return Err(io::Error::other("JVM descriptor apps must exactly match the discovered app inventory"));
    }
    let mut files = Files::new();
    let mut backend = assemble_backend(&inputs.backend, &mut files)?;
    let mut metadata = Metadata {
        version: 3,
        java_version: jvm.java.version,
        apps: Vec::new(),
        profiles: BTreeMap::new(),
        assets: BTreeMap::new(),
    };
    for app in &project.apps {
        let input = descriptor_apps[app.id.as_str()];
        let bytes = executable_jar(input, &mut files)?;
        let sha256 = content_digest(&bytes);
        let jar = format!("apps/{}/{}.jar", app.id, sha256);
        validate_implementations(&inputs.project, app, &input.sessions)?;
        let mut sessions = BTreeMap::new();
        for id in &input.sessions {
            let requirements = app.sessions.get(id).unwrap_or(&app.runtime);
            let machine_profile = requirements
                .machine_profile
                .as_ref()
                .or(app.runtime.machine_profile.as_ref())
                .map_or("default", String::as_str);
            let capacity = requirements.capacity.or(app.runtime.capacity).unwrap_or(16);
            if let Some(local) = &project.local {
                let profile = local
                    .profiles
                    .get(machine_profile)
                    .ok_or_else(|| io::Error::other(format!("unknown machine profile {machine_profile}")))?;
                metadata.profiles.insert(machine_profile.into(), profile.clone());
            }
            let reconnect = requirements.reconnect.unwrap_or(true);
            sessions.insert(
                id.clone(),
                SessionDeclaration { machine_profile: machine_profile.into(), capacity, reconnect },
            );
        }
        insert(&mut files, jar.clone(), bytes)?;
        metadata.apps.push(chunk_contract::AppArtifact {
            id: app.id.clone(),
            jar,
            sha256,
            java_version: input.java_version,
            sessions,
        });
    }
    assets(&inputs.project.join("assets"), "assets", &mut files, &mut metadata.assets)?;
    for app in &project.apps {
        let directory = format!("{}/assets", app.directory);
        assets(&inputs.project.join(&directory), &directory, &mut files, &mut metadata.assets)?;
    }
    destination_profiles(&backend, project.local.as_ref(), &mut metadata)?;
    if let Some(compiled) = &backend.contracts.domains {
        let bindings = project.apps.iter().map(|app| (app.id.clone(), app.domain.clone())).collect();
        if compiled.apps != bindings || compiled.scopes != project.scopes {
            return Err(io::Error::other(
                "compiled domain manifest no longer matches the project; recompile the backend",
            ));
        }
    } else if !project.modules.is_empty() {
        return Err(io::Error::other("backend is missing the project's domain manifest; recompile the backend"));
    }
    if backend.contracts.env != project.env {
        return Err(io::Error::other("compiled variables no longer match chunk.toml; recompile the backend"));
    }
    insert(&mut files, "release.json".into(), serde_json::to_vec(&metadata).map_err(io::Error::other)?)?;
    let id = publication::digest(files.iter().map(|(name, bytes)| (name.as_str(), bytes.as_slice())));
    backend.id.clone_from(&id);
    files.insert(
        "release.json".into(),
        serde_json::to_vec(&Manifest { id: &id, metadata: &metadata }).map_err(io::Error::other)?,
    );
    insert(&mut files, "backend.json".into(), serde_json::to_vec(&backend).map_err(io::Error::other)?)?;
    let verified = verify::check(&files)?;
    let archive = if inputs.archive {
        let prepared = archive::prepare(dist, &files)?;
        let path = dist.join(format!("{id}.tar.gz"));
        archive::verify_existing(&prepared, &path)?;
        Some((prepared, path))
    } else {
        None
    };
    let directory = directory::publish(dist, &id, &files)?;
    let archive = match archive {
        Some((prepared, path)) => {
            archive::publish(prepared, &path)?;
            Some(path.canonicalize()?)
        }
        None => None,
    };
    Ok(Release { id, directory, archive, apps: verified.apps })
}

/// Returns the JAR an app's JVM runs: the bundled JAR itself, or a launcher over the thin JAR and the classpath JARs
/// it adds to `files`.
fn executable_jar(app: &descriptor::App, files: &mut Files) -> io::Result<Vec<u8>> {
    let bytes = read_limited(&app.jar, 128 * 1024 * 1024)?;
    if app.classpath.is_empty() {
        return Ok(bytes);
    }
    let main = jars::main_class(&bytes)?;
    let mut jars = vec![bytes];
    for path in &app.classpath {
        jars.push(read_limited(path, 128 * 1024 * 1024)?);
    }
    assemble_launcher(&main, jars, files)
}

/// Publishes a thin app JAR and its runtime classpath under `libs/` and returns a launcher JAR that runs `main` with
/// them. The launcher manifest names every JAR by digest, so its own digest changes whenever any of them does.
fn assemble_launcher(main: &str, jars: impl IntoIterator<Item = Vec<u8>>, files: &mut Files) -> io::Result<Vec<u8>> {
    let mut classpath = Vec::new();
    for bytes in jars {
        let name = format!("libs/{}.jar", content_digest(&bytes));
        insert(files, name.clone(), bytes)?;
        let entry = format!("../../{name}");
        if !classpath.contains(&entry) {
            classpath.push(entry);
        }
    }
    launcher::write(main, &classpath)
}

fn validate_implementations(root: &Path, app: &project::AppMetadata, actual: &[String]) -> io::Result<()> {
    if root.join(&app.directory).join("app.ts").is_file() {
        let expected: BTreeSet<_> = if app.sessions.is_empty() {
            ["default"].into()
        } else {
            app.sessions.keys().map(String::as_str).collect()
        };
        if expected != actual.iter().map(String::as_str).collect() {
            return Err(io::Error::other(format!(
                "app {} implementations must exactly match its packaged session providers",
                app.id
            )));
        }
    } else if app.sessions.keys().any(|id| !actual.contains(id)) {
        return Err(io::Error::other(format!("app {} configures an unknown session type", app.id)));
    }
    Ok(())
}

fn destination_profiles(
    backend: &chunk_contract::Deployment,
    local: Option<&project::LocalConfig>,
    metadata: &mut Metadata,
) -> io::Result<()> {
    if let (Some(destinations), Some(local)) = (&backend.contracts.destinations, local) {
        for policy in destinations.entries.values() {
            let name = &policy.destination.machine_profile;
            let profile = local
                .profiles
                .get(name)
                .ok_or_else(|| io::Error::other(format!("unknown destination machine profile {name}")))?;
            metadata.profiles.insert(name.clone(), profile.clone());
        }
    }
    Ok(())
}

fn assemble_backend(directory: &Path, files: &mut Files) -> io::Result<chunk_contract::Deployment> {
    let source =
        String::from_utf8(read_limited(&directory.join("source.mjs"), 4 * 1024 * 1024)?).map_err(io::Error::other)?;
    let contract: BackendMetadata =
        serde_json::from_slice(&read_limited(&directory.join("contract.json"), 2 * 1024 * 1024)?)
            .map_err(io::Error::other)?;
    let encoded_contract = serde_json::to_vec(&contract).map_err(io::Error::other)?;
    let backend = verify::deployment(source, contract);
    insert(files, "source.mjs".into(), backend.source.as_bytes().to_vec())?;
    insert(files, "contract.json".into(), encoded_contract)?;
    let source_map = directory.join("source.mjs.map");
    if directory::exists(&source_map)? {
        insert(files, "source.mjs.map".into(), read_limited(&source_map, 8 * 1024 * 1024)?)?;
    }
    Ok(backend)
}

fn assets(directory: &Path, prefix: &str, files: &mut Files, hashes: &mut BTreeMap<String, String>) -> io::Result<()> {
    if directory::exists(directory)? {
        let mut collected = Files::new();
        publication::collect(directory, prefix, &mut collected)?;
        for (name, bytes) in collected {
            hashes.insert(name.clone(), content_digest(&bytes));
            insert(files, name, bytes)?;
        }
    }
    Ok(())
}
