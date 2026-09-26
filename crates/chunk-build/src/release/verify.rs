use std::{collections::BTreeMap, io, path::Path};

use chunk_contract::{AppArtifact, Deployment};
use serde::Deserialize;

use super::{Manifest, Metadata, content_digest};
use crate::{
    BackendMetadata,
    publication::{self, Files},
};

/// A release directory whose contents match its identity and contracts.
#[derive(Debug)]
pub struct VerifiedRelease {
    pub id: String,
    pub java_version: u32,
    pub apps: Vec<AppArtifact>,
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
/// from the release ID, a `backend.json` not derived from the release's backend, invalid contracts and missing or
/// modified app JARs and assets.
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

    let mut apps = BTreeMap::new();
    for app in &metadata.apps {
        app.validate().map_err(io::Error::other)?;
        if !(25..=metadata.java_version).contains(&app.java_version)
            || app.jar != format!("apps/{}/{}.jar", app.id, app.sha256)
            || files.get(&app.jar).is_none_or(|bytes| content_digest(bytes) != app.sha256)
        {
            return Err(io::Error::other(format!("app {} JAR is missing or differs from the release", app.id)));
        }
        if apps.insert(app.id.clone(), app.clone()).is_some() {
            return Err(io::Error::other(format!("release declares app {} twice", app.id)));
        }
    }
    for (name, sha256) in &metadata.assets {
        if files.get(name).is_none_or(|bytes| content_digest(bytes) != *sha256) {
            return Err(io::Error::other(format!("asset {name} is missing or differs from the release")));
        }
    }
    check_app_contracts(&backend, &apps)?;
    Ok(VerifiedRelease { id, java_version: metadata.java_version, apps: metadata.apps, backend })
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

fn check_app_contracts(backend: &Deployment, apps: &BTreeMap<String, AppArtifact>) -> io::Result<()> {
    let contracts = &backend.contracts;
    if let Some(destinations) = &contracts.destinations {
        destinations.validate_apps(apps).map_err(io::Error::other)?;
    }
    if let Some(configurations) = &contracts.session_configurations {
        configurations.validate_apps(apps).map_err(io::Error::other)?;
    }
    if let Some(methods) = &contracts.session_methods
        && methods
            .methods
            .iter()
            .any(|method| apps.get(&method.app).is_none_or(|app| !app.sessions.contains_key(&method.session)))
    {
        return Err(io::Error::other("session method references unknown release app or session"));
    }
    Ok(())
}
