use std::{fs, io, path::Path, process::Command};

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
    fs::create_dir_all(output)?;
    let output = output.canonicalize()?;
    let staging = tempfile::Builder::new().prefix(".compile-").tempdir_in(&output)?;
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/compiler/bundle.mjs");
    let result = Command::new("node")
        .env("NO_COLOR", "1")
        .arg(script)
        .arg(&project)
        .arg(staging.path())
        .output()?;
    if !result.status.success() {
        return Err(io::Error::other(String::from_utf8_lossy(&result.stderr).into_owned()));
    }
    let source = String::from_utf8(super::read_limited(
        &staging.path().join("source.mjs"),
        4 * 1024 * 1024,
    )?)
    .map_err(io::Error::other)?;
    let contract = extract(&source)
        .map_err(|error| io::Error::other(format!("Backend deployment at {}: {error}", project.display())))?;
    fs::write(
        staging.path().join("contract.json"),
        serde_json::to_vec(&contract).map_err(io::Error::other)?,
    )?;
    for name in ["source.mjs", "source.mjs.map", "contract.json"] {
        fs::rename(staging.path().join(name), output.join(name))?;
    }
    Ok(())
}

fn extract(source: &str) -> io::Result<BackendMetadata> {
    Engine::init_platform();
    let mut engine = Engine::new().map_err(io::Error::other)?;
    let id = DeploymentId::new("declaration-extraction").map_err(io::Error::other)?;
    engine
        .register(id.clone(), source.into(), Limits::default())
        .map_err(io::Error::other)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compilation_diagnostics_identify_invalid_schema_and_deployment() {
        let project = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("server/schema")).unwrap();
        let schema = project.path().join("server/schema/index.ts");
        fs::write(
            &schema,
            "import {defineTable,v} from '@chunk/server'; export default defineTable({name:v.string()});",
        )
        .unwrap();
        let error = compile(project.path(), output.path()).unwrap_err().to_string();
        assert!(
            error.contains("server/schema/index.ts must default-export a schema created with defineSchema()"),
            "{error}"
        );
        fs::write(
            &schema,
            "import {defineSchema} from '@chunk/server'; export default defineSchema({});",
        )
        .unwrap();
        fs::write(
            project.path().join("server/invalid-name.ts"),
            "import {query,v} from '@chunk/server'; export const value=query({args:{},returns:v.null(),handler:()=>null});",
        )
        .unwrap();
        let error = compile(project.path(), output.path()).unwrap_err().to_string();
        assert!(error.contains("invalid function path"), "{error}");
        assert!(
            error.contains(&project.path().canonicalize().unwrap().display().to_string()),
            "{error}"
        );
        assert!(!error.contains('\u{1b}'), "{error}");
    }

    #[test]
    fn clean_typescript_compilation_produces_deterministic_executable_contracts() {
        let project = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("server/schema")).unwrap();
        fs::create_dir_all(project.path().join("apps/duels/server")).unwrap();
        fs::write(project.path().join("server/schema/index.ts"), "import {defineSchema,defineTable,v} from '@chunk/server'; export default defineSchema({profiles:defineTable({player:v.player()}).index('by_player',['player'])});").unwrap();
        fs::write(project.path().join("apps/duels/server/match.ts"), "import {query,internalMutation,v} from '@chunk/server'; export function helper(n:number){return n+1} export const score=query({args:{value:v.integer()},returns:v.integer(),handler:(_,a)=>helper(a.value)}); export const hidden=internalMutation({args:{},returns:v.null(),handler:()=>null});").unwrap();
        compile(project.path(), output.path()).unwrap();
        let contract = fs::read(output.path().join("contract.json")).unwrap();
        let decoded: BackendMetadata = serde_json::from_slice(&contract).unwrap();
        assert_eq!(decoded.functions.len(), 2);
        assert_eq!(decoded.tables["profiles"].indexes["by_player"], ["player"]);
        let source = fs::read_to_string(output.path().join("source.mjs")).unwrap();
        let mut engine = Engine::new().unwrap();
        let id = DeploymentId::new("test").unwrap();
        engine.register(id.clone(), source.clone(), Limits::default()).unwrap();
        let result = engine
            .execute(
                &id,
                Invocation {
                    export: decoded.functions["apps/duels/match/score"].export.clone(),
                    arguments: json!({"value":2}).into(),
                    caller: Value::Null.into(),
                    mode: Mode::Query,
                    timestamp: 0,
                    seed: 0,
                },
                Box::new(Declarations),
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(result.value, "3");
        drop(engine);
        compile(project.path(), output.path()).unwrap();
        assert_eq!(contract, fs::read(output.path().join("contract.json")).unwrap());
        assert_eq!(source, fs::read_to_string(output.path().join("source.mjs")).unwrap());
        fs::write(
            project.path().join("server/bad.ts"),
            "import 'node:fs'; export const value=1;",
        )
        .unwrap();
        assert!(compile(project.path(), output.path()).is_err());
        fs::write(project.path().join("server/bad.ts"), "export const value=Date.now();").unwrap();
        assert!(compile(project.path(), output.path()).is_err());
    }
}
