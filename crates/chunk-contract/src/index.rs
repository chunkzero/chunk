use std::cmp::Ordering;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Ascending equality-prefix/half-open range query, with document ID breaking ties.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexQuery {
    pub table: String,
    pub index: String,
    pub prefix: Vec<Value>,
    pub start: Option<Value>,
    pub end: Option<Value>,
    pub limit: usize,
}

impl IndexQuery {
    /// Caller validates the query against its declaration before matching documents.
    #[must_use]
    pub fn matches(&self, fields: &[String], document: &Value) -> bool {
        if self.prefix.len() > fields.len() {
            return false;
        }
        if !fields
            .iter()
            .zip(&self.prefix)
            .all(|(field, expected)| compare_index_values(&document[field], expected).is_eq())
        {
            return false;
        }
        let next = fields
            .get(self.prefix.len())
            .map_or(&Value::Null, |field| &document[field]);
        self.start
            .as_ref()
            .is_none_or(|start| !compare_index_values(next, start).is_lt())
            && self
                .end
                .as_ref()
                .is_none_or(|end| compare_index_values(next, end).is_lt())
    }

    #[must_use]
    pub fn compare(fields: &[String], left: &(String, Value), right: &(String, Value)) -> Ordering {
        fields
            .iter()
            .map(|name| compare_index_values(&left.1[name], &right.1[name]))
            .find(|order| !order.is_eq())
            .unwrap_or_else(|| left.0.cmp(&right.0))
    }
}

/// Scalar order used by SQLite indexes. Missing optional fields are represented
/// by null. Non-scalar values sort last and will fail document validation.
#[must_use]
pub fn compare_index_values(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Number(a), Value::Number(b)) => compare_numbers(a, b),
        _ => rank(left).cmp(&rank(right)),
    }
}

fn compare_numbers(a: &serde_json::Number, b: &serde_json::Number) -> Ordering {
    match (a.as_i64(), b.as_i64()) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(a), None) => integer_float(a, b.as_f64().expect("JSON number")),
        (None, Some(b)) => integer_float(b, a.as_f64().expect("JSON number")).reverse(),
        (None, None) => a.as_f64().partial_cmp(&b.as_f64()).expect("finite JSON numbers"),
    }
}

fn rank(value: &Value) -> u8 {
    match value {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Number(_) => 2,
        Value::String(_) => 3,
        _ => 4,
    }
}

#[allow(clippy::cast_possible_truncation)]
fn integer_float(integer: i64, float: f64) -> Ordering {
    i128::from(integer)
        .cmp(&(float as i128))
        .then_with(|| 0.0_f64.partial_cmp(&float.fract()).expect("finite JSON number"))
}
