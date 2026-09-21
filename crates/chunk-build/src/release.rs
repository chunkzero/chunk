use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    BackendMetadata, project,
    publication::{self, Files, insert},
    read_limited,
};

mod archive;
mod descriptor;
mod jars;
mod manifest;
mod session_configurations;
mod session_methods;
pub use descriptor::{JavaRuntime, JvmDescriptor, read_jvm_descriptor};

/// Separate build outputs consumed by the complete release publisher.
pub struct ReleaseInputs {
    pub project: PathBuf,
    pub backend: PathBuf,
    pub jvm_descriptor: PathBuf,
}

pub struct Release {
    pub id: String,
    pub directory: PathBuf,
    pub archive: PathBuf,
    pub apps: Vec<chunk_contract::AppArtifact>,
}

#[derive(Serialize)]
struct Metadata<'a> {
    version: u32,
    java_version: u32,
    apps: Vec<chunk_contract::AppArtifact>,
    profiles: BTreeMap<String, &'a project::MachineProfile>,
    assets: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct Manifest<'a, 'b> {
    id: &'a str,
    #[serde(flatten)]
    metadata: &'a Metadata<'b>,
}

/// Publishes one portable release directory and its sibling gzip-compressed tar archive.
/// Identity covers normalized metadata and payloads, excluding derived IDs and the archive wrapper.
/// Repeated publication verifies existing bytes and never overwrites an immutable release.
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
        let bytes = read_limited(&input.jar, 128 * 1024 * 1024)?;
        jars::Classpath::default().add(&bytes, &format!("app {}", app.id), jvm.java.version, true)?;
        session_methods::validate(&bytes, &app.id, &input.sessions, backend.contracts.session_methods.as_ref())?;
        session_configurations::validate(
            &bytes,
            &app.id,
            &input.sessions,
            backend.contracts.session_configurations.as_ref(),
        )?;
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
                metadata.profiles.insert(machine_profile.into(), profile);
            }
            sessions.insert(
                id.clone(),
                chunk_contract::SessionDeclaration { machine_profile: machine_profile.into(), capacity },
            );
        }
        insert(&mut files, jar.clone(), bytes)?;
        let artifact =
            chunk_contract::AppArtifact { id: app.id.clone(), jar, sha256, java_version: input.java_version, sessions };
        artifact.validate().map_err(io::Error::other)?;
        metadata.apps.push(artifact);
    }
    assets(&inputs.project.join("assets"), "assets", &mut files, &mut metadata.assets)?;
    for app in &project.apps {
        let directory = format!("{}/assets", app.directory);
        assets(&inputs.project.join(&directory), &directory, &mut files, &mut metadata.assets)?;
    }
    validate_app_contracts(&backend, project.local.as_ref(), &mut metadata)?;
    if let Some(compiled) = &backend.contracts.domains {
        let bindings = project.apps.iter().map(|app| (app.id.clone(), app.domain.clone())).collect();
        if compiled.apps != bindings || compiled.scopes != project.scopes {
            return Err(io::Error::other(
                "compiled domain manifest no longer matches the project; recompile the backend",
            ));
        }
    } else if inputs.project.join("server/domains").exists() || !project.modules.is_empty() {
        return Err(io::Error::other("backend is missing the project's domain manifest; recompile the backend"));
    }
    insert(&mut files, "release.json".into(), serde_json::to_vec(&metadata).map_err(io::Error::other)?)?;
    let id = publication::digest(&files);
    backend.id.clone_from(&id);
    files.insert(
        "release.json".into(),
        serde_json::to_vec(&Manifest { id: &id, metadata: &metadata }).map_err(io::Error::other)?,
    );
    insert(&mut files, "backend.json".into(), serde_json::to_vec(&backend).map_err(io::Error::other)?)?;
    let archive = archive::prepare(dist, &files)?;
    let archive_path = dist.join(format!("{id}.tar.gz"));
    archive::verify_existing(&archive, &archive_path)?;
    let directory = publication::publish_directory(dist, &id, &files)?;
    archive::publish(archive, &archive_path)?;
    Ok(Release { id, directory, archive: archive_path.canonicalize()?, apps: metadata.apps })
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

fn validate_app_contracts<'a>(
    backend: &chunk_contract::Deployment,
    local: Option<&'a project::LocalConfig>,
    metadata: &mut Metadata<'a>,
) -> io::Result<()> {
    let apps = metadata.apps.iter().map(|app| (app.id.clone(), app.clone())).collect();
    if let Some(destinations) = &backend.contracts.destinations {
        destinations.validate_apps(&apps).map_err(io::Error::other)?;
        if let Some(local) = local {
            for policy in destinations.entries.values() {
                let name = &policy.destination.machine_profile;
                let profile = local
                    .profiles
                    .get(name)
                    .ok_or_else(|| io::Error::other(format!("unknown destination machine profile {name}")))?;
                metadata.profiles.insert(name.clone(), profile);
            }
        }
    }
    if let Some(configurations) = &backend.contracts.session_configurations {
        configurations.validate_apps(&apps).map_err(io::Error::other)?;
    }
    if let Some(methods) = &backend.contracts.session_methods
        && methods.methods.iter().any(|method| {
            !metadata.apps.iter().any(|app| app.id == method.app && app.sessions.contains_key(&method.session))
        })
    {
        return Err(io::Error::other("session method references unknown release app or session"));
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
    let backend = chunk_contract::Deployment {
        contracts: contract.contracts,
        contract_version: contract.contract_version,
        runtime_profile: contract.runtime_profile,
        id: "validation".into(),
        source,
        tables: contract.tables,
        functions: contract.functions,
    };
    backend.validate().map_err(io::Error::other)?;
    insert(files, "source.mjs".into(), backend.source.as_bytes().to_vec())?;
    insert(files, "contract.json".into(), encoded_contract)?;
    let source_map = directory.join("source.mjs.map");
    if publication::exists(&source_map)? {
        insert(files, "source.mjs.map".into(), read_limited(&source_map, 8 * 1024 * 1024)?)?;
    }
    Ok(backend)
}

fn assets(directory: &Path, prefix: &str, files: &mut Files, hashes: &mut BTreeMap<String, String>) -> io::Result<()> {
    if publication::exists(directory)? {
        let mut collected = Files::new();
        publication::collect(directory, prefix, &mut collected)?;
        for (name, bytes) in collected {
            hashes.insert(name.clone(), content_digest(&bytes));
            insert(files, name, bytes)?;
        }
    }
    Ok(())
}

fn content_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests;
