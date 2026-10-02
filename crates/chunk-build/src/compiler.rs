mod bundle;
mod descriptors;
mod domains;
mod sources;
mod stage;
mod typecheck;
use std::{collections::BTreeMap, fs, io, path::Path};

use chunk_contract::{DatabaseSchema, Deployment, MigrationKind};
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
    compile_with(project, output, false)
}

/// Like [`compile`]; when `frozen`, fails instead of writing an `Additive` journal entry.
/// # Errors
/// As [`compile`], and when `frozen`, for schema changes not yet recorded in the journal.
pub fn compile_with(project: &Path, output: &Path, frozen: bool) -> io::Result<()> {
    let project = project.canonicalize()?;
    let journal = crate::migrations::verified(&project, false)?;
    compile_journal(&project, output, &journal, frozen)
}

/// Compiles from `journal`, which was verified when it was read: migrations are type-checked and bundled from
/// copies of its sources in a private directory under `.chunk/`, never from `server/migrations/`.
pub(crate) fn compile_journal(
    project: &Path,
    output: &Path,
    journal: &crate::migrations::Journal,
    frozen: bool,
) -> io::Result<()> {
    let inventory = crate::project::load(project)?;
    crate::sdk::generate(project, &inventory, journal)?;
    fs::create_dir_all(output)?;
    let output = output.canonicalize()?;
    let staging = tempfile::Builder::new().prefix(".compile-").tempdir_in(&output)?;
    let stage = stage::stage(project, &inventory, journal)?;
    let migrations = &stage.migrations;
    let files = sources::discover(project, &inventory)?;
    let paths: Vec<_> = files
        .iter()
        .map(|file| file.path.as_path())
        .chain(migrations.iter().map(|migration| migration.path.as_path()))
        .collect();
    typecheck::check(&paths, &stage.chunk, staging.path())?;
    let (mut contract, backs) = bundle_and_extract(project, staging.path(), &files, migrations, &inventory)?;
    let recorded = crate::migrations::record_additive(project, journal, &contract.tables, frozen)?;
    let journal = recorded.as_ref().unwrap_or(journal);
    contract.contracts.migrations = journal.contract(&backs);
    for migration in contract.contracts.migrations.iter().filter(|migration| migration.kind == MigrationKind::Expand) {
        if !backs.get(&migration.id).is_some_and(|tables| tables.keys().eq(migration.tables.keys())) {
            return Err(io::Error::other(format!(
                "server/migrations/{}.ts must transform exactly the tables its snapshot changes: {}",
                migration.id,
                migration.tables.keys().cloned().collect::<Vec<_>>().join(", ")
            )));
        }
    }
    contract.contracts.env = inventory.env.clone();
    contract.assets = crate::project::assets::contract(&inventory);
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

/// Evaluates only `server/schema/`, without type-checking or migration checks, for diffing it.
pub(crate) fn schema(project: &Path) -> io::Result<DatabaseSchema> {
    let inventory = crate::project::load(project)?;
    crate::sdk::generate(project, &inventory, &crate::migrations::Journal::read(project)?)?;
    let staging = tempfile::Builder::new().prefix(".schema-").tempdir_in(project.join(".chunk"))?;
    let files = [sources::Source {
        path: project.join("server/schema/index.ts"),
        namespace: "shared/schema/index".into(),
        authoring: None,
    }];
    let (contract, _) =
        bundle_and_extract(project, staging.path(), &files, &[], &crate::project::Inventory::default())?;
    Ok(contract.tables)
}

type Backs = BTreeMap<String, BTreeMap<String, bool>>;

fn bundle_and_extract(
    project: &Path,
    staging: &Path,
    files: &[sources::Source<'_>],
    migrations: &[bundle::MigrationSource],
    inventory: &crate::project::Inventory,
) -> io::Result<(BackendMetadata, Backs)> {
    let sdk = project.join(".chunk/sdk");
    // This synchronous compiler entry point runs on a blocking thread in async callers.
    let executor = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    executor.block_on(bundle::build(project, staging, &sdk, files, migrations, inventory))?;
    drop(executor);
    let source = String::from_utf8(super::read_limited(&staging.join("source.mjs"), 4 * 1024 * 1024)?)
        .map_err(io::Error::other)?;
    extract(&source).map_err(|error| io::Error::other(format!("Backend deployment at {}: {error}", project.display())))
}

fn extract(source: &str) -> io::Result<(BackendMetadata, Backs)> {
    Engine::init_platform();
    let mut engine = Engine::new().map_err(io::Error::other)?;
    let id = DeploymentId::new("declaration-extraction").map_err(io::Error::other)?;
    engine.register(id.clone(), source.into(), Limits::default()).map_err(io::Error::other)?;
    let mut call = |export: &str| {
        let invocation = Invocation {
            export: export.into(),
            arguments: Value::Null.into(),
            caller: Value::Null.into(),
            mode: Mode::Query,
            timestamp: 0,
            seed: 0,
        };
        engine
            .execute(&id, invocation, Box::new(Declarations), &Cancellation::default())
            .map(|result| result.value)
            .map_err(io::Error::other)
    };
    let contract: BackendMetadata = serde_json::from_str(&call("__chunk_contract")?).map_err(io::Error::other)?;
    let backs: Backs = serde_json::from_str(&call("__chunk_migrations")?).map_err(io::Error::other)?;
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
    Ok((contract, backs))
}

fn error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests;
