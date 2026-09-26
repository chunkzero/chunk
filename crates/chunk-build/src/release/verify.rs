use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::Path,
};

use chunk_contract::{AppArtifact, Contracts, Deployment};
use serde::Deserialize;

use super::{
    Manifest, Metadata, content_digest, jars, jars::Classpath, launcher, session_configurations, session_methods,
};
use crate::{
    BackendMetadata,
    project::MachineProfile,
    publication::{self, Files},
};

/// A release directory whose contents match its identity and contracts.
#[derive(Debug)]
pub struct VerifiedRelease {
    pub id: String,
    pub java_version: u32,
    pub apps: Vec<AppArtifact>,
    /// The machine profiles its sessions and destinations run on; empty when it was built without `[local]`.
    pub profiles: BTreeMap<String, MachineProfile>,
    pub backend: Deployment,
}

#[derive(Deserialize)]
struct Signed {
    id: String,
    #[serde(flatten)]
    metadata: Metadata,
}

/// Validates an unpacked release directory the same way `chunk build` validates the releases it publishes.
/// # Errors
/// Rejects links, nonportable paths, oversized contents, an unsupported descriptor version, contents that differ
/// from the release ID, a `backend.json` not derived from the release's backend, invalid contracts, app catalogs or
/// machine profiles, contract references outside the release, missing or modified assets, and app JARs that are
/// missing, incompatible, conflicting or differ from their contracts.
pub fn verify_release(directory: &Path) -> io::Result<VerifiedRelease> {
    let mut files = Files::new();
    publication::collect(directory, "", &mut files)?;
    check(&files)
}

pub(super) fn check(files: &Files) -> io::Result<VerifiedRelease> {
    let file = |name: &str| files.get(name).ok_or_else(|| io::Error::other(format!("release is missing {name}")));
    let manifest = file("release.json")?;
    let Signed { id, metadata } = serde_json::from_slice(manifest).map_err(io::Error::other)?;
    if metadata.version != 3 || !(25..=100).contains(&metadata.java_version) {
        return Err(io::Error::other("release requires descriptor version 3 and Java 25–100"));
    }
    if serde_json::to_vec(&Manifest { id: &id, metadata: &metadata }).map_err(io::Error::other)? != *manifest {
        return Err(io::Error::other("release.json is not in its canonical form"));
    }
    let unsigned = serde_json::to_vec(&metadata).map_err(io::Error::other)?;
    let identity = files.iter().filter(|(name, _)| *name != "backend.json").map(|(name, bytes)| {
        (name.as_str(), if name == "release.json" { unsigned.as_slice() } else { bytes.as_slice() })
    });
    if publication::digest(identity) != id {
        return Err(io::Error::other("release contents differ from its ID"));
    }

    let contract: BackendMetadata = serde_json::from_slice(file("contract.json")?).map_err(io::Error::other)?;
    let source = String::from_utf8(file("source.mjs")?.clone()).map_err(io::Error::other)?;
    let mut backend = deployment(source, contract);
    backend.id.clone_from(&id);
    if serde_json::to_vec(&backend).map_err(io::Error::other)? != *file("backend.json")? {
        return Err(io::Error::other("backend.json differs from the release's backend"));
    }
    backend.validate().map_err(io::Error::other)?;

    let apps = check_catalog(&metadata, &backend)?;
    for app in apps.values() {
        check_jar(app, metadata.java_version, files, &backend.contracts)?;
    }
    for (name, sha256) in &metadata.assets {
        if files.get(name).is_none_or(|bytes| content_digest(bytes) != *sha256) {
            return Err(io::Error::other(format!("asset {name} is missing or differs from the release")));
        }
    }
    Ok(VerifiedRelease {
        id,
        java_version: metadata.java_version,
        apps: metadata.apps,
        profiles: metadata.profiles,
        backend,
    })
}

pub(super) fn deployment(source: String, contract: BackendMetadata) -> Deployment {
    Deployment {
        contracts: contract.contracts,
        contract_version: contract.contract_version,
        runtime_profile: contract.runtime_profile,
        id: "validation".into(),
        source,
        tables: contract.tables,
        functions: contract.functions,
    }
}

/// Checks the app inventory, machine profiles and every contract reference into them.
fn check_catalog(metadata: &Metadata, backend: &Deployment) -> io::Result<BTreeMap<String, AppArtifact>> {
    if metadata.apps.is_empty() || metadata.apps.len() > 128 {
        return Err(io::Error::other("release requires 1–128 apps"));
    }
    let mut apps = BTreeMap::new();
    let mut folded = BTreeSet::new();
    for app in &metadata.apps {
        app.validate().map_err(io::Error::other)?;
        if !(25..=metadata.java_version).contains(&app.java_version) {
            return Err(io::Error::other(format!("app {} requires an incompatible Java version", app.id)));
        }
        if !folded.insert(app.id.to_ascii_lowercase()) {
            return Err(io::Error::other("release app IDs must be unique and must not differ only by case"));
        }
        apps.insert(app.id.clone(), app.clone());
    }
    let contracts = &backend.contracts;
    if metadata.profiles.iter().any(|(name, profile)| !MachineProfile::valid(name, profile)) {
        return Err(io::Error::other("release declares an invalid machine profile"));
    }
    let mut referenced =
        apps.values().flat_map(|app| app.sessions.values().map(|session| &session.machine_profile)).chain(
            contracts.destinations.iter().flat_map(|destinations| {
                destinations.entries.values().map(|policy| &policy.destination.machine_profile)
            }),
        );
    if !metadata.profiles.is_empty() && referenced.any(|name| !metadata.profiles.contains_key(name)) {
        return Err(io::Error::other("release references a machine profile it does not declare"));
    }
    if let Some(domains) = &contracts.domains
        && !domains.apps.keys().eq(apps.keys())
    {
        return Err(io::Error::other("domain manifest app bindings differ from the release's apps"));
    }
    if let Some(destinations) = &contracts.destinations {
        destinations.validate_apps(&apps).map_err(io::Error::other)?;
    }
    if let Some(configurations) = &contracts.session_configurations {
        configurations.validate_apps(&apps).map_err(io::Error::other)?;
    }
    if let Some(methods) = &contracts.session_methods
        && methods
            .methods
            .iter()
            .any(|method| apps.get(&method.app).is_none_or(|app| !app.sessions.contains_key(&method.session)))
    {
        return Err(io::Error::other("session method references unknown release app or session"));
    }
    Ok(apps)
}

/// Checks the JARs one app runs: its release JAR, or the launcher's dependency closure, with a compatible and
/// conflict-free classpath, its `Main-Class` and the session schemas and providers it packages.
fn check_jar(app: &AppArtifact, java: u32, files: &Files, contracts: &Contracts) -> io::Result<()> {
    let missing = || io::Error::other(format!("app {} JAR is missing or differs from the release", app.id));
    if app.jar != format!("apps/{}/{}.jar", app.id, app.sha256) {
        return Err(missing());
    }
    let jar = files.get(&app.jar).filter(|bytes| content_digest(bytes) == app.sha256).ok_or_else(missing)?;
    let launcher = launcher::classpath(jar)?;
    let jars = match &launcher {
        None => vec![(app.jar.as_str(), jar.as_slice())],
        Some(entries) => entries
            .iter()
            .map(|entry| {
                let name = entry.strip_prefix("../../")?;
                let bytes = files.get(name)?;
                (name == format!("libs/{}.jar", content_digest(bytes))).then_some((name, bytes.as_slice()))
            })
            .collect::<Option<_>>()
            .ok_or_else(|| io::Error::other(format!("app {} launcher names a missing or modified JAR", app.id)))?,
    };
    let [(_, app_jar), ..] = jars.as_slice() else {
        return Err(io::Error::other(format!("app {} launcher has an empty classpath", app.id)));
    };
    let main = jars::main_class(app_jar)?;
    if let Some(entries) = &launcher
        && launcher::write(&main, entries)? != *jar
    {
        return Err(io::Error::other(format!("app {} launcher differs from its classpath", app.id)));
    }
    let mut classpath = Classpath::default();
    for (label, bytes) in &jars {
        classpath.add(bytes, label, java)?;
    }
    if !classpath.contains(&main) {
        return Err(io::Error::other(format!("app {} Main-Class {main} is not on its classpath", app.id)));
    }
    let sessions: Vec<_> = app.sessions.keys().cloned().collect();
    session_methods::validate(app_jar, &classpath, &app.id, &sessions, contracts.session_methods.as_ref())?;
    session_configurations::validate(app_jar, &classpath, &app.id, &sessions, contracts.session_configurations.as_ref())
}
