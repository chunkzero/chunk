mod bundle;
mod descriptors;
mod domains;
mod sources;
mod typecheck;
use std::{fs, io, path::Path};

use chunk_contract::Deployment;
use chunk_js::{Cancellation, DeploymentId, Engine, Invocation, Key, Limits, Mode, ReadHost};
use serde_json::Value;

use super::BackendMetadata;

struct Declarations;

impl ReadHost for Declarations {
    fn get(&mut self, _: &Key) -> Result<Option<Value>, String> {
        Err("declarations cannot read data".into())
    }
    fn scan(&mut self, _: &str, _: Option<&str>, _: Option<&str>) -> Result<Vec<(String, Value)>, String> {
        Err("declarations cannot read data".into())
    }
}

/// Type-checks and bundles shared/app-local TypeScript, then extracts metadata in
/// the bounded transactional engine. Produces source.mjs, source.mjs.map and
/// contract.json for immutable publication; no JVM compilation is required.
/// # Errors
/// Reports compiler diagnostics, unsupported imports, impure declarations or invalid contracts.
pub fn compile(project: &Path, output: &Path) -> io::Result<()> {
    let project = project.canonicalize()?;
    let inventory = crate::project::load(&project)?;
    crate::sdk::generate(&project, &inventory)?;
    fs::create_dir_all(output)?;
    let output = output.canonicalize()?;
    let staging = tempfile::Builder::new().prefix(".compile-").tempdir_in(&output)?;
    let files = sources::discover(&project, &inventory)?;
    let sdk = project.join(".chunk/sdk");
    typecheck::check(&files, staging.path())?;
    // This synchronous compiler entry point runs on a blocking thread in async callers.
    let executor = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    executor.block_on(bundle::build(&project, staging.path(), &sdk, &files, &inventory))?;
    drop(executor);
    let source = String::from_utf8(super::read_limited(&staging.path().join("source.mjs"), 4 * 1024 * 1024)?)
        .map_err(io::Error::other)?;
    let mut contract = extract(&source)
        .map_err(|error| io::Error::other(format!("Backend deployment at {}: {error}", project.display())))?;
    contract.contracts.env = inventory.env.clone();
    let apps = &inventory.apps;
    if let Some(methods) = &contract.contracts.session_methods {
        for method in &methods.methods {
            if !apps.iter().any(|app| app.id == method.app) {
                return Err(io::Error::other(format!("Session method references unknown app: {}", method.app)));
            }
        }
    }
    if let Some(configurations) = &contract.contracts.session_configurations
        && configurations.configurations.iter().any(|configuration| !apps.iter().any(|app| app.id == configuration.app))
    {
        return Err(io::Error::other("session configuration references an undiscovered app"));
    }
    if let Some(destinations) = &contract.contracts.destinations
        && destinations.entries.values().any(|policy| {
            let app = policy.destination.session_type.split('/').next().unwrap_or_default();
            !apps.iter().any(|candidate| candidate.id == app)
        })
    {
        return Err(io::Error::other("destination references an undiscovered app"));
    }
    fs::write(staging.path().join("contract.json"), serde_json::to_vec(&contract).map_err(io::Error::other)?)?;
    for name in ["source.mjs", "source.mjs.map", "contract.json"] {
        fs::rename(staging.path().join(name), output.join(name))?;
    }
    Ok(())
}

fn extract(source: &str) -> io::Result<BackendMetadata> {
    Engine::init_platform();
    let mut engine = Engine::new().map_err(io::Error::other)?;
    let id = DeploymentId::new("declaration-extraction").map_err(io::Error::other)?;
    engine.register(id.clone(), source.into(), Limits::default()).map_err(io::Error::other)?;
    let result = engine
        .execute(
            &id,
            Invocation {
                export: "__chunk_contract".into(),
                arguments: Value::Null.into(),
                caller: Value::Null.into(),
                mode: Mode::Query,
                timestamp: 0,
                seed: 0,
            },
            Box::new(Declarations),
            &Cancellation::default(),
        )
        .map_err(io::Error::other)?;
    let contract: BackendMetadata = serde_json::from_str(&result.value).map_err(io::Error::other)?;
    Deployment {
        contracts: contract.contracts.clone(),
        contract_version: contract.contract_version,
        runtime_profile: contract.runtime_profile,
        id: "validation".into(),
        source: source.into(),
        tables: contract.tables.clone(),
        functions: contract.functions.clone(),
    }
    .validate()
    .map_err(io::Error::other)?;
    Ok(contract)
}

fn error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests;
