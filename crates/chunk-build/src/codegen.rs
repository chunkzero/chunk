use std::{collections::BTreeMap, io, path::Path};

use chunk_contract::{Deployment, Schema};

use crate::{BackendMetadata, quote};

mod java;
mod kotlin;
mod output;
mod typescript;

/// The explicitly selected developer-facing output. JVM sources share one Java model.
#[derive(Debug, Clone, Copy)]
pub enum GenerationTarget<'a> {
    Java { package: &'a str },
    Kotlin { package: &'a str },
    TypeScript,
}

/// Generates only the selected client sources from a compiled contract.
/// Tracks owned files in the destination so package/target changes remove stale generated sources
/// without deleting handwritten files. Existing owned files must be unchanged before replacement.
/// # Errors
/// Rejects invalid contracts, unsupported literals, Java name collisions, output conflicts and filesystem failures.
pub fn generate(contract: &Path, output: &Path, target: GenerationTarget<'_>) -> io::Result<()> {
    let contract = read_contract(contract)?;
    let files = match target {
        GenerationTarget::Java { package } => java::bindings(&contract)?.sources(package)?,
        GenerationTarget::Kotlin { package } => {
            let bindings = java::bindings(&contract)?;
            let mut files = bindings.sources(package)?;
            files.insert(
                format!("kotlin/{}/CoroutineBackendClient.kt", package.replace('.', "/")),
                kotlin::source(package, &bindings.root),
            );
            files
        }
        GenerationTarget::TypeScript => BTreeMap::from([("api.ts".into(), typescript::generate(&contract))]),
    };
    output::write(output, &files)
}

fn read_contract(contract: &Path) -> io::Result<BackendMetadata> {
    let contract: BackendMetadata =
        serde_json::from_slice(&super::read_limited(contract, 2 * 1024 * 1024)?).map_err(io::Error::other)?;
    Deployment {
        contracts: contract.contracts.clone(),
        contract_version: contract.contract_version,
        runtime_profile: contract.runtime_profile,
        id: "codegen".into(),
        source: "// contract validation".into(),
        tables: contract.tables.clone(),
        functions: contract.functions.clone(),
    }
    .validate()
    .map_err(io::Error::other)?;
    contract.assets.validate().map_err(io::Error::other)?;
    Ok(contract)
}

fn validate_literals(schema: &Schema) -> io::Result<()> {
    match schema {
        Schema::Literal { value } => {
            chunk_contract::validate_wire_value(value).map_err(io::Error::other)?;
            if value.as_str().is_some_and(|value| !java_constant(value)) {
                return Err(io::Error::other("literal exceeds Java string constant limit"));
            }
            Ok(())
        }
        Schema::Nullable { value } => validate_literals(value),
        Schema::Array { items } => validate_literals(items),
        Schema::Object { fields } => fields.values().try_for_each(|field| validate_literals(&field.schema)),
        Schema::Union { variants } => variants.values().try_for_each(validate_literals),
        _ => Ok(()),
    }
}

/// Whether `value` fits a class-file string constant, which uses modified UTF-8 with a u16 byte length.
fn java_constant(value: &str) -> bool {
    let bytes: usize = value
        .encode_utf16()
        .map(|unit| match unit {
            1..=0x7f => 1,
            0..=0x7ff => 2,
            _ => 3,
        })
        .sum();
    u16::try_from(bytes).is_ok()
}

#[cfg(test)]
mod tests;
