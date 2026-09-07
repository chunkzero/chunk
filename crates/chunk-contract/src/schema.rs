use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    Literal {
        value: Value,
    },
    Array {
        items: Box<Schema>,
    },
    Object {
        fields: BTreeMap<String, Field>,
    },
    Union {
        variants: Vec<Schema>,
    },
}

impl Schema {
    #[must_use]
    pub fn is_scalar(&self) -> bool {
        matches!(self, Self::Boolean | Self::Number | Self::Integer | Self::String)
    }

    /// Checks the value without coercion; object properties must be declared.
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
            Self::Number => value.is_i64() || value.is_f64(),
            Self::Integer => value.is_i64(),
            Self::String => value.is_string(),
            Self::Literal { value: expected } => value == expected,
            Self::Array { items } => value
                .as_array()
                .is_some_and(|items_value| items_value.iter().all(|v| items.accepts_at(v, depth + 1))),
            Self::Object { fields } => accepts_object(fields, value, depth),
            Self::Union { variants } => variants.iter().any(|v| v.accepts_at(value, depth + 1)),
        }
    }

    fn validate(&self, depth: usize) -> Result<(), &'static str> {
        if depth > 32 {
            return Err("schema nesting limit");
        }
        match self {
            Self::Array { items } => items.validate(depth + 1)?,
            Self::Object { fields } => validate_fields(fields, depth + 1)?,
            Self::Union { variants } => {
                if variants.is_empty() || variants.len() > 16 {
                    return Err("invalid union size");
                }
                for variant in variants {
                    variant.validate(depth + 1)?;
                }
            }
            Self::Literal { value }
                if !value.is_null() && !value.is_boolean() && !value.is_string() && !value.is_number() =>
            {
                return Err("literals must be scalar");
            }
            _ => {}
        }
        Ok(())
    }
}

impl TableSchema {
    /// # Errors
    /// Rejects invalid names, excessive declarations and non-scalar index fields.
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_fields(&self.fields, 0)?;
        if self.indexes.len() > 16 {
            return Err("too many indexes");
        }
        let mut names = BTreeSet::new();
        for (name, fields) in &self.indexes {
            validate_name(name)?;
            if !names.insert(name.to_ascii_lowercase()) || fields.is_empty() || fields.len() > 8 {
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
        accepts_object(&self.fields, value, 0)
    }
}

fn accepts_object(fields: &BTreeMap<String, Field>, value: &Value, depth: usize) -> bool {
    value.as_object().is_some_and(|object| {
        object.keys().all(|key| fields.contains_key(key))
            && fields.iter().all(|(key, field)| {
                object
                    .get(key)
                    .map_or(field.optional, |v| field.schema.accepts_at(v, depth + 1))
            })
    })
}

fn validate_fields(fields: &BTreeMap<String, Field>, depth: usize) -> Result<(), &'static str> {
    if fields.len() > 64 {
        return Err("too many fields");
    }
    let mut names = BTreeSet::new();
    for (name, field) in fields {
        validate_name(name)?;
        if !names.insert(name.to_ascii_lowercase()) {
            return Err("field names differ only by case");
        }
        field.schema.validate(depth)?;
    }
    Ok(())
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
