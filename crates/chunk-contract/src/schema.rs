use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_DEPTH: usize = 32;
const MAX_UNION_VARIANTS: usize = 16;
const MAX_FIELDS: usize = 64;
const MAX_INDEXES: usize = 16;
const MAX_INDEX_FIELDS: usize = 8;
const MAX_TABLES: usize = 128;
const MAX_SCHEMA_BYTES: usize = 1024 * 1024;

/// Composition keys are persistent table identities, independent of source paths.
pub type DatabaseSchema = BTreeMap<String, TableSchema>;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableSchema {
    pub fields: BTreeMap<String, Field>,
    /// Ascending scalar fields. Document ID is the final ordering tiebreaker.
    #[serde(default)]
    pub indexes: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub schema: Schema,
    /// An absent property differs from a present property whose value is null.
    #[serde(default)]
    pub optional: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Schema {
    Null,
    Boolean,
    /// Signed 64-bit integers or finite IEEE-754 doubles.
    Number,
    Integer,
    String,
    Id {
        table: String,
    },
    Player,
    Session,
    Literal {
        value: Value,
    },
    Enum {
        values: Vec<String>,
    },
    Nullable {
        value: Box<Schema>,
    },
    Array {
        items: Box<Schema>,
    },
    Object {
        fields: BTreeMap<String, Field>,
    },
    Union {
        variants: BTreeMap<String, Schema>,
    },
}

impl Schema {
    #[must_use]
    pub fn is_scalar(&self) -> bool {
        matches!(
            self,
            Self::Boolean
                | Self::Number
                | Self::Integer
                | Self::String
                | Self::Enum { .. }
                | Self::Id { .. }
                | Self::Player
                | Self::Session
        )
    }

    /// Checks the value without coercion; object properties must be declared.
    #[must_use]
    pub fn accepts(&self, value: &Value) -> bool {
        self.accepts_at(value, 0)
    }

    fn accepts_at(&self, value: &Value, depth: usize) -> bool {
        if depth > MAX_DEPTH {
            return false;
        }
        match self {
            Self::Null => value.is_null(),
            Self::Boolean => value.is_boolean(),
            Self::Number => value.is_i64() || value.is_f64(),
            Self::Integer => value.is_i64(),
            Self::String => value.is_string(),
            Self::Id { table } => value.as_str().is_some_and(|id| {
                id.strip_prefix(table).and_then(|suffix| suffix.strip_prefix(':')).is_some_and(valid_id)
            }),
            Self::Player | Self::Session => value.as_str().is_some_and(valid_id),
            Self::Literal { value: expected } => value == expected,
            Self::Enum { values } => value.as_str().is_some_and(|v| values.iter().any(|item| item == v)),
            Self::Nullable { value: inner } => value.is_null() || inner.accepts_at(value, depth + 1),
            Self::Array { items } => {
                value.as_array().is_some_and(|items_value| items_value.iter().all(|v| items.accepts_at(v, depth + 1)))
            }
            Self::Object { fields } => accepts_object(fields, value, depth, None),
            Self::Union { variants } => value.as_object().is_some_and(|object| {
                object.get("type").and_then(Value::as_str).and_then(|tag| variants.get(tag)).is_some_and(|variant| {
                    matches!(variant, Self::Object { fields }
                        if depth < MAX_DEPTH && accepts_object(fields, value, depth + 1, Some("type")))
                })
            }),
        }
    }

    /// Normalizes optional API properties. Database values retain explicit presence semantics.
    pub fn normalize_api(&self, value: &mut Value) {
        match (self, value) {
            (Self::Object { fields }, Value::Object(object)) => {
                for (name, field) in fields {
                    if field.optional && object.get(name).is_some_and(Value::is_null) {
                        object.remove(name);
                    } else if let Some(value) = object.get_mut(name) {
                        field.schema.normalize_api(value);
                    }
                }
            }
            (Self::Array { items }, Value::Array(values)) => {
                for value in values {
                    items.normalize_api(value);
                }
            }
            (Self::Nullable { value: inner }, value) if !value.is_null() => inner.normalize_api(value),
            (Self::Union { variants }, value) => {
                if let Some(variant) = value.get("type").and_then(Value::as_str).and_then(|tag| variants.get(tag)) {
                    variant.normalize_api(value);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn validate(&self, depth: usize) -> Result<(), &'static str> {
        if depth > MAX_DEPTH {
            return Err("schema nesting limit");
        }
        match self {
            Self::Id { table } => validate_name(table)?,
            Self::Enum { values } => {
                if values.is_empty()
                    || values.len() > MAX_FIELDS
                    || values.iter().collect::<BTreeSet<_>>().len() != values.len()
                {
                    return Err("invalid enum values");
                }
                for value in values {
                    validate_name(value)?;
                }
            }
            Self::Nullable { value } => value.validate(depth + 1)?,
            Self::Array { items } => items.validate(depth + 1)?,
            Self::Object { fields } => validate_fields(fields, depth + 1, true)?,
            Self::Union { variants } => {
                if variants.is_empty() || variants.len() > MAX_UNION_VARIANTS {
                    return Err("invalid union size");
                }
                for (name, variant) in variants {
                    validate_name(name)?;
                    if !matches!(variant, Self::Object { fields } if !fields.contains_key("type")) {
                        return Err("union variants must be objects without a type field");
                    }
                    variant.validate(depth + 1)?;
                }
            }
            Self::Literal { value } => {
                if value.is_array() || value.is_object() {
                    return Err("literals must be scalar");
                }
                crate::validate_wire_value(value)?;
            }
            _ => {}
        }
        Ok(())
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl TableSchema {
    /// # Errors
    /// Rejects invalid names, excessive declarations and non-scalar index fields.
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_fields(&self.fields, 0, false)?;
        if self.indexes.len() > MAX_INDEXES {
            return Err("too many indexes");
        }
        let mut names = BTreeSet::new();
        for (name, fields) in &self.indexes {
            validate_name(name)?;
            if !names.insert(name.to_ascii_lowercase()) || fields.is_empty() || fields.len() > MAX_INDEX_FIELDS {
                return Err("invalid index declaration");
            }
            let mut seen = BTreeSet::new();
            for field in fields {
                if !seen.insert(field) || !self.fields.get(field).is_some_and(|f| f.schema.is_scalar()) {
                    return Err("indexes require distinct declared scalar fields");
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn accepts(&self, value: &Value) -> bool {
        accepts_object(&self.fields, value, 0, None)
    }
}

fn accepts_object(fields: &BTreeMap<String, Field>, value: &Value, depth: usize, ignored: Option<&str>) -> bool {
    value.as_object().is_some_and(|object| {
        object.keys().all(|key| Some(key.as_str()) == ignored || fields.contains_key(key))
            && fields
                .iter()
                .all(|(key, field)| object.get(key).map_or(field.optional, |v| field.schema.accepts_at(v, depth + 1)))
    })
}

fn validate_fields(fields: &BTreeMap<String, Field>, depth: usize, metadata: bool) -> Result<(), &'static str> {
    if fields.len() > MAX_FIELDS + usize::from(metadata && fields.contains_key("_id")) {
        return Err("too many fields");
    }
    let mut names = BTreeSet::new();
    for (name, field) in fields {
        if !(metadata && name == "_id" && matches!(field.schema, Schema::Id { .. }) && !field.optional) {
            validate_name(name)?;
        }
        if !names.insert(name.to_ascii_lowercase()) {
            return Err("field names differ only by case");
        }
        field.schema.validate(depth)?;
    }
    Ok(())
}

/// Tables whose names start with this prefix, in any letter case, belong to the
/// environment itself. App deployments and migrations may not declare them.
pub const SYSTEM_TABLE_PREFIX: &str = "chunk_";

/// Whether `table` is reserved for the environment by [`SYSTEM_TABLE_PREFIX`].
#[must_use]
pub fn is_system_table(table: &str) -> bool {
    table.get(..SYSTEM_TABLE_PREFIX.len()).is_some_and(|prefix| prefix.eq_ignore_ascii_case(SYSTEM_TABLE_PREFIX))
}

/// Portable SQL identifiers; a leading underscore is reserved for storage metadata.
/// # Errors
/// Rejects empty, oversized, reserved or non-ASCII identifiers.
pub fn validate_name(name: &str) -> Result<(), &'static str> {
    if name.len() > 64
        || !name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        || name.to_ascii_lowercase().starts_with("sqlite_")
    {
        return Err("invalid schema identifier");
    }
    Ok(())
}

/// Validates a complete database declaration.
/// # Errors
/// Rejects invalid declarations, case collisions and excessive schema sizes.
pub fn validate(schema: &DatabaseSchema) -> Result<(), &'static str> {
    if schema.len() > MAX_TABLES
        || serde_json::to_vec(schema).map_err(|_| "invalid schema serialization")?.len() > MAX_SCHEMA_BYTES
    {
        return Err("schema size limit");
    }
    let mut names = BTreeSet::new();
    for (name, table) in schema {
        validate_name(name)?;
        if !names.insert(name.to_ascii_lowercase()) {
            return Err("table names differ only by case");
        }
        table.validate()?;
    }
    Ok(())
}
