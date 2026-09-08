use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{DatabaseSchema, Schema};

pub const CONTRACT_VERSION: u32 = 1;
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
}

impl Deployment {
    /// # Errors
    /// Rejects unsupported versions, invalid declarations and namespace collisions.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.contract_version != CONTRACT_VERSION {
            return Err("unsupported contract version");
        }
        if self.id.is_empty()
            || self.id.len() > 128
            || self.id.contains('\0')
            || self.source.len() > 4 * 1024 * 1024
            || self.functions.len() > 256
            || serde_json::to_vec(self).map_err(|_| "invalid deployment")?.len() > 5 * 1024 * 1024
        {
            return Err("deployment size limit");
        }
        crate::validate(&self.tables)?;
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
            validate_literals(&function.arguments)?;
            validate_literals(&function.result)?;
        }
        for path in &paths {
            for (offset, _) in path.match_indices('/') {
                if paths.contains(&path[..offset]) {
                    return Err("function path collides with namespace");
                }
            }
        }
        Ok(())
    }
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn validate_literals(schema: &Schema) -> Result<(), &'static str> {
    match schema {
        Schema::Literal { value } => validate_wire_value(value),
        Schema::Array { items } => validate_literals(items),
        Schema::Object { fields } => fields.values().try_for_each(|f| validate_literals(&f.schema)),
        Schema::Union { variants } => variants.iter().try_for_each(validate_literals),
        _ => Ok(()),
    }
}

/// Wire values use JSON, finite numbers, and exactly representable integers.
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
mod tests {
    use super::*;
    use serde_json::json;

    fn deployment() -> Deployment {
        Deployment {
            contract_version: CONTRACT_VERSION,
            runtime_profile: RuntimeProfile::TransactionalV1,
            id: "v1".into(),
            source: "export function read() { return null }".into(),
            tables: BTreeMap::new(),
            functions: [(
                "shared/profile".into(),
                Function {
                    kind: FunctionKind::Query,
                    visibility: Visibility::Public,
                    export: "read".into(),
                    arguments: Schema::Object {
                        fields: BTreeMap::new(),
                    },
                    result: Schema::Null,
                },
            )]
            .into(),
        }
    }

    #[test]
    fn versioned_contract_round_trip_and_namespace_validation() {
        let original = deployment();
        let copy: Deployment = serde_json::from_value(serde_json::to_value(&original).unwrap()).unwrap();
        assert_eq!(original, copy);
        copy.validate().unwrap();
        for path in ["shared", "Shared/Profile", "shared//other", "../escape"] {
            let mut invalid = original.clone();
            let mut function = invalid.functions.values().next().unwrap().clone();
            function.export = "other".into();
            invalid.functions.insert(path.into(), function);
            assert!(invalid.validate().is_err(), "{path}");
        }
        let mut invalid = original;
        invalid.contract_version += 1;
        assert!(invalid.validate().is_err());
        let mut encoded = serde_json::to_value(deployment()).unwrap();
        encoded["runtime_profile"] = json!("unknown");
        assert!(serde_json::from_value::<Deployment>(encoded).is_err());
    }

    #[test]
    fn wire_values_preserve_optional_null_and_integer_boundaries() {
        for value in [
            json!(9_007_199_254_740_991_i64),
            json!(-9_007_199_254_740_991_i64),
            json!(0.125),
        ] {
            validate_wire_value(&value).unwrap();
        }
        for value in [
            json!(9_007_199_254_740_992_i64),
            json!(i64::MIN),
            json!({"nested": [u64::MAX]}),
        ] {
            assert!(validate_wire_value(&value).is_err());
        }
        let schema = Schema::Object {
            fields: [(
                "value".into(),
                crate::Field {
                    schema: Schema::String,
                    optional: true,
                },
            )]
            .into(),
        };
        assert!(schema.accepts(&json!({})));
        assert!(schema.accepts(&json!({"value": "text"})));
        assert!(!schema.accepts(&json!({"value": null})));
        let mut invalid = deployment();
        invalid.functions.values_mut().next().unwrap().result = Schema::Union { variants: vec![] };
        assert!(invalid.validate().is_err());
    }
}
