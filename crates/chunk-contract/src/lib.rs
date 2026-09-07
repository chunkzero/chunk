//! Explicit contracts for immutable bundled application deployments.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Schema {
    Null,
    Boolean,
    Number,
    Integer,
    String,
    Literal { value: Value },
    Array { items: Box<Schema> },
    Object { fields: BTreeMap<String, Field> },
    Union { variants: Vec<Schema> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub schema: Schema,
    #[serde(default)]
    pub optional: bool,
}

impl Schema {
    /// Optional object fields differ from nullable values. Extra fields are rejected.
    #[must_use]
    pub fn accepts(&self, value: &Value) -> bool {
        self.accepts_at(value, 0)
    }

    fn accepts_at(&self, value: &Value, depth: usize) -> bool {
        if depth > 32 {
            return false;
        }
        match self {
            Self::Null => value.is_null(),
            Self::Boolean => value.is_boolean(),
            Self::Number => value.is_number(),
            Self::Integer => value.is_i64() || value.is_u64(),
            Self::String => value.is_string(),
            Self::Literal { value: expected } => value == expected,
            Self::Array { items } => value
                .as_array()
                .is_some_and(|a| a.iter().all(|v| items.accepts_at(v, depth + 1))),
            Self::Object { fields } => value.as_object().is_some_and(|object| {
                object.keys().all(|key| fields.contains_key(key))
                    && fields.iter().all(|(key, field)| {
                        object
                            .get(key)
                            .map_or(field.optional, |v| field.schema.accepts_at(v, depth + 1))
                    })
            }),
            Self::Union { variants } => variants.iter().any(|v| v.accepts_at(value, depth + 1)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionKind {
    Query,
    Mutation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Function {
    pub kind: FunctionKind,
    pub export: String,
    pub arguments: Schema,
    pub result: Schema,
}

/// Source is a bundled ES module; function paths select explicit exported handlers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    pub id: String,
    pub source: String,
    pub tables: BTreeMap<String, Schema>,
    pub functions: BTreeMap<String, Function>,
}
