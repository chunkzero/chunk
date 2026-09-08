use chunk_contract::{Field, Schema};
use rusqlite::types::Value as SqlValue;
use serde_json::Value;

use crate::{Error, Result};

pub(super) fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub(super) fn column(name: &str, field: &Field) -> String {
    let name = quote(name);
    let (kind, check) = match &field.schema {
        Schema::Boolean => ("INTEGER", format!(" CHECK ({name} IN (0, 1))")),
        Schema::Integer => ("INTEGER", String::new()),
        // STRICT ANY preserves the integer/double distinction without REAL affinity
        // rounding large integers. The CHECK still restricts the column to numbers.
        Schema::Number => (
            "ANY",
            format!(" CHECK ({name} IS NULL OR typeof({name}) IN ('integer', 'real'))"),
        ),
        Schema::String => ("TEXT", String::new()),
        _ => ("TEXT", format!(" CHECK (json_valid({name}))")),
    };
    let required = if field.optional { "" } else { " NOT NULL" };
    format!("{name} {kind}{required}{check}")
}

pub(super) fn encode(field: &Field, value: Option<&Value>) -> Result<SqlValue> {
    let Some(value) = value else {
        return if field.optional {
            Ok(SqlValue::Null)
        } else {
            Err(Error::Invalid("missing required field"))
        };
    };
    let encoded = match (&field.schema, value) {
        (Schema::Boolean, Value::Bool(value)) => Some(SqlValue::Integer(i64::from(*value))),
        (Schema::Integer | Schema::Number, Value::Number(value)) if value.is_i64() => {
            value.as_i64().map(SqlValue::Integer)
        }
        (Schema::Number, Value::Number(value)) if value.is_f64() => value.as_f64().map(SqlValue::Real),
        (Schema::String, Value::String(value)) => Some(SqlValue::Text(value.clone())),
        (schema, value) if !schema.is_scalar() && schema.accepts(value) => {
            Some(SqlValue::Text(serde_json::to_string(value)?))
        }
        _ => None,
    };
    encoded.ok_or(Error::Invalid("field does not match schema"))
}

pub(super) fn decode(field: &Field, value: SqlValue) -> Result<Option<Value>> {
    if value == SqlValue::Null && field.optional {
        return Ok(None);
    }
    let decoded = match (&field.schema, value) {
        (Schema::Boolean, SqlValue::Integer(value @ (0 | 1))) => Value::Bool(value == 1),
        (Schema::Integer | Schema::Number, SqlValue::Integer(value)) => value.into(),
        (Schema::Number, SqlValue::Real(value)) => serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or(Error::Corrupt("non-finite number"))?,
        (Schema::String, SqlValue::Text(value)) => Value::String(value),
        (schema, SqlValue::Text(value)) if !schema.is_scalar() => serde_json::from_str(&value)?,
        _ => return Err(Error::Corrupt("field has an unexpected storage type")),
    };
    if !field.schema.accepts(&decoded) {
        return Err(Error::Corrupt("stored field violates its schema"));
    }
    Ok(Some(decoded))
}
