use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{DatabaseSchema, DomainManifest, Schema};

pub const CONTRACT_VERSION: u32 = 3;
const SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeProfile {
    TransactionalV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionKind {
    Query,
    Mutation,
    Action,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Public,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Function {
    pub kind: FunctionKind,
    pub visibility: Visibility,
    pub export: String,
    pub arguments: Schema,
    pub result: Schema,
}

/// Immutable bundle and language-independent metadata. Paths identify functions;
/// schema composition keys identify environment tables across deployments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    pub contract_version: u32,
    pub runtime_profile: RuntimeProfile,
    pub id: String,
    pub source: String,
    pub tables: DatabaseSchema,
    pub functions: BTreeMap<String, Function>,
    #[serde(flatten)]
    pub contracts: Contracts,
}

/// Optional manifests a deployment may declare beyond its functions and tables.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contracts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domains: Option<DomainManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_methods: Option<crate::SessionMethods>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_configurations: Option<crate::SessionConfigurations>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destinations: Option<crate::DestinationManifest>,
    #[serde(default, skip_serializing_if = "crate::EnvManifest::is_empty")]
    pub env: crate::EnvManifest,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub migrations: Vec<crate::Migration>,
}

impl Deployment {
    /// # Errors
    /// Rejects unsupported versions, invalid declarations and namespace collisions.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.contract_version != CONTRACT_VERSION {
            return Err("unsupported contract version");
        }
        if self.id.is_empty() || self.id.len() > 128 || self.id.contains('\0') {
            return Err("invalid deployment id");
        }
        if self.source.len() > 4 * 1024 * 1024
            || self.functions.len() > 256
            || serde_json::to_vec(self).map_err(|_| "invalid deployment")?.len() > 5 * 1024 * 1024
        {
            return Err("deployment size limit");
        }
        crate::validate(&self.tables)?;
        let contracts = &self.contracts;
        contracts.env.validate().map_err(|_| "invalid variables or secret names")?;
        crate::validate_migrations(&contracts.migrations)?;
        if let Some(methods) = &contracts.session_methods {
            methods.validate()?;
        }
        if let Some(configurations) = &contracts.session_configurations {
            configurations.validate()?;
        }
        let mut paths = BTreeSet::new();
        let mut exports = BTreeSet::new();
        for (path, function) in &self.functions {
            if path.len() > 256 || !path.split('/').all(identifier) {
                return Err("invalid function path");
            }
            let normalized = path.to_ascii_lowercase();
            if !paths.insert(normalized) || !exports.insert(&function.export) || !identifier(&function.export) {
                return Err("function namespace collision or invalid export");
            }
            function.arguments.validate(0)?;
            function.result.validate(0)?;
        }
        for path in &paths {
            for (offset, _) in path.match_indices('/') {
                if paths.contains(&path[..offset]) {
                    return Err("function path collides with namespace");
                }
            }
        }
        if let Some(destinations) = &contracts.destinations {
            destinations.validate()?;
            destinations.validate_configurations(contracts.session_configurations.as_ref())?;
        }
        if let Some(domains) = &contracts.domains {
            domains.validate()?;
            for command in domains.commands.values() {
                command.validate(&self.functions)?;
            }
            if domains
                .hooks
                .values()
                .map(|hook| &hook.export)
                .chain(domains.commands.values().map(|command| &command.export))
                .any(|export| !exports.insert(export))
            {
                return Err("domain handler export collides with function export");
            }
        }
        Ok(())
    }
}

pub(crate) fn identifier(value: &str) -> bool {
    ascii_identifier(value, 128)
}

/// ASCII identifier: starts with a letter or underscore, then letters, digits or underscores.
pub(crate) fn ascii_identifier(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value.bytes().next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Wire values use JSON and finite numbers. All integral numbers, including
/// floating-point values, must be within ±(2^53 - 1); larger values use strings.
/// IDs are opaque strings. Missing optional properties and explicit null remain
/// distinct; deletion is an operation, never an undefined JSON sentinel.
/// # Errors
/// Rejects excessive nesting and numbers that cannot safely cross JavaScript.
pub fn validate_wire_value(value: &Value) -> Result<(), &'static str> {
    fn visit(value: &Value, depth: usize) -> Result<(), &'static str> {
        if depth > 32 {
            return Err("wire value nesting limit");
        }
        match value {
            Value::Number(number) => {
                let n = number.as_f64().ok_or("invalid wire number")?;
                if !n.is_finite() || (n.fract() == 0.0 && n.abs() > SAFE_INTEGER) {
                    return Err("wire integer exceeds JavaScript safe range");
                }
                Ok(())
            }
            Value::Array(values) => values.iter().try_for_each(|v| visit(v, depth + 1)),
            Value::Object(values) => values.values().try_for_each(|v| visit(v, depth + 1)),
            _ => Ok(()),
        }
    }
    visit(value, 0)
}

#[cfg(test)]
mod tests;
