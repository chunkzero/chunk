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
}

#[derive(Serialize)]
struct Metadata<'a> {
    version: u32,
    java_version: u32,
    apps: Vec<App<'a>>,
    classpath: Vec<Dependency>,
    profiles: BTreeMap<&'a str, &'a project::MachineProfile>,
    assets: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct App<'a> {
    id: &'a str,
    jar: String,
    sha256: String,
    java_version: u32,
    runtime: &'a project::RuntimeRequirements,
}

#[derive(Serialize)]
struct Dependency {
    file: String,
    sha256: String,
    artifact: String,
    component: descriptor::Component,
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
    let project = project::inspect(&inputs.project)?;
    let jvm = read_jvm_descriptor(&inputs.jvm_descriptor)?;
    let descriptor_apps: BTreeMap<_, _> = jvm.apps.iter().map(|app| (app.id.as_str(), app)).collect();
    let discovered: BTreeSet<_> = project.apps.iter().map(|app| app.id.as_str()).collect();
    if descriptor_apps.len() != jvm.apps.len() || descriptor_apps.keys().copied().collect::<BTreeSet<_>>() != discovered
    {
        return Err(io::Error::other(
            "JVM descriptor apps must exactly match the discovered app inventory",
        ));
    }
    let mut files = Files::new();
    let mut classes = jars::Classpath::default();
    let mut metadata = Metadata {
        version: 1,
        java_version: jvm.java.version,
        apps: Vec::new(),
        classpath: Vec::new(),
        profiles: BTreeMap::new(),
        assets: BTreeMap::new(),
    };
    for app in &project.apps {
        let input = descriptor_apps[app.id.as_str()];
        let bytes = read_limited(&input.jar, 128 * 1024 * 1024)?;
        classes.add(&bytes, &format!("app {}", app.id), jvm.java.version, Some(&app.id))?;
        let sha256 = content_digest(&bytes);
        let jar = format!("gameplay/lib/{sha256}.jar");
        insert(&mut files, jar.clone(), bytes)?;
        metadata.apps.push(App {
            id: &app.id,
            jar,
            sha256,
            java_version: input.java_version,
            runtime: &app.runtime,
        });
        if let Some(profile) = &app.runtime.machine_profile {
            let local = project
                .local
                .as_ref()
                .ok_or_else(|| io::Error::other("missing runtime profile definitions"))?;
            metadata.profiles.insert(profile, &local.profiles[profile]);
        }
    }
    metadata.classpath = classpath(&jvm, &mut classes, &mut files)?;
    assets(
        &inputs.project.join("assets"),
        "assets",
        &mut files,
        &mut metadata.assets,
    )?;
    for app in &project.apps {
        let directory = format!("{}/assets", app.directory);
        assets(
            &inputs.project.join(&directory),
            &directory,
            &mut files,
            &mut metadata.assets,
        )?;
    }
    let mut backend = assemble_backend(&inputs.backend, &mut files)?;
    insert(
        &mut files,
        "release.json".into(),
        serde_json::to_vec(&metadata).map_err(io::Error::other)?,
    )?;
    let id = publication::digest(&files);
    backend.id.clone_from(&id);
    files.insert(
        "release.json".into(),
        serde_json::to_vec(&Manifest {
            id: &id,
            metadata: &metadata,
        })
        .map_err(io::Error::other)?,
    );
    insert(
        &mut files,
        "backend.json".into(),
        serde_json::to_vec(&backend).map_err(io::Error::other)?,
    )?;
    let archive = archive::prepare(dist, &files)?;
    let archive_path = dist.join(format!("{id}.tar.gz"));
    archive::verify_existing(&archive, &archive_path)?;
    let directory = publication::publish_directory(dist, &id, &files)?;
    archive::publish(archive, &archive_path)?;
    Ok(Release {
        id,
        directory,
        archive: archive_path.canonicalize()?,
    })
}

fn assemble_backend(directory: &Path, files: &mut Files) -> io::Result<chunk_contract::Deployment> {
    let source =
        String::from_utf8(read_limited(&directory.join("source.mjs"), 4 * 1024 * 1024)?).map_err(io::Error::other)?;
    let contract: BackendMetadata =
        serde_json::from_slice(&read_limited(&directory.join("contract.json"), 2 * 1024 * 1024)?)
            .map_err(io::Error::other)?;
    let encoded_contract = serde_json::to_vec(&contract).map_err(io::Error::other)?;
    let backend = chunk_contract::Deployment {
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
        insert(
            files,
            "source.mjs.map".into(),
            read_limited(&source_map, 8 * 1024 * 1024)?,
        )?;
    }
    Ok(backend)
}

fn classpath(jvm: &JvmDescriptor, classes: &mut jars::Classpath, files: &mut Files) -> io::Result<Vec<Dependency>> {
    let mut dependencies = BTreeMap::<_, Dependency>::new();
    let mut versions = BTreeMap::new();
    let mut inspected = BTreeSet::new();
    for dependency in &jvm.classpath {
        if let descriptor::Component::Module { group, name, version } = &dependency.component
            && versions
                .insert((group, name), version)
                .is_some_and(|previous| previous != version)
        {
            return Err(io::Error::other(format!(
                "conflicting versions of JVM module {group}:{name}"
            )));
        }
        let bytes = read_limited(&dependency.file, 128 * 1024 * 1024)?;
        let sha256 = content_digest(&bytes);
        let key = (dependency.component.clone(), dependency.artifact.clone());
        if let Some(previous) = dependencies.get(&key) {
            if previous.sha256 != sha256 {
                return Err(io::Error::other(format!(
                    "conflicting bytes for JVM artifact {:?}",
                    dependency.artifact
                )));
            }
            continue;
        }
        if inspected.insert(sha256.clone()) {
            classes.add(&bytes, &dependency.artifact, jvm.java.version, None)?;
        }
        let file = format!("gameplay/lib/{sha256}.jar");
        insert(files, file.clone(), bytes)?;
        dependencies.insert(
            key,
            Dependency {
                file,
                sha256,
                artifact: dependency.artifact.clone(),
                component: dependency.component.clone(),
            },
        );
    }
    Ok(dependencies.into_values().collect())
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
