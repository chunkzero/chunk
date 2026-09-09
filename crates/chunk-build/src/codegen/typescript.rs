use std::collections::BTreeMap;

use chunk_contract::{Field, Schema};
use serde_json::Value;

use super::quote;

pub(super) fn fields(fields: &BTreeMap<String, Field>) -> String {
    format!(
        "shape({{{}}})",
        fields
            .iter()
            .map(|(name, field)| {
                let validator = schema(&field.schema);
                format!(
                    "{}: {}",
                    quote(name),
                    if field.optional {
                        format!("v.optional({validator})")
                    } else {
                        validator
                    }
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    )
}

pub(super) fn schema(value: &Schema) -> String {
    match value {
        Schema::Null => "v.null()".into(),
        Schema::Boolean => "v.boolean()".into(),
        Schema::Number => "v.number()".into(),
        Schema::Integer => "v.integer()".into(),
        Schema::String => "v.string()".into(),
        Schema::Id { table } => format!("v.id({})", quote(table)),
        Schema::Player => "v.player()".into(),
        Schema::Session => "v.session()".into(),
        Schema::Literal { value } => format!("v.literal({value})"),
        Schema::Array { items } => format!("v.array({})", schema(items)),
        Schema::Object { fields: declarations } => {
            if let Some(Field {
                schema: Schema::Id { table },
                ..
            }) = declarations.get("_id")
            {
                let mut rest = declarations.clone();
                rest.remove("_id");
                format!("v.document({}, {})", quote(table), fields(&rest))
            } else {
                format!("v.object({})", fields(declarations))
            }
        }
        Schema::Union { variants } => format!("v.union({})", variants.iter().map(schema).collect::<Vec<_>>().join(",")),
    }
}

pub(super) fn tree(node: &Value) -> String {
    match node {
        Value::String(value) => value.clone(),
        Value::Object(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(name, value)| format!("[{}]:{}", quote(name), tree(value)))
                .collect::<Vec<_>>()
                .join(",\n")
        ),
        _ => unreachable!("reference tree"),
    }
}

pub(super) fn insert(api: &mut Value, path: &str, entry: String) {
    let mut node = api;
    let segments: Vec<_> = path.split('/').collect();
    for segment in &segments[..segments.len() - 1] {
        node = node
            .as_object_mut()
            .expect("validated namespace")
            .entry(*segment)
            .or_insert_with(|| serde_json::json!({}));
    }
    node.as_object_mut().expect("validated namespace").insert(
        segments.last().expect("validated function path").to_string(),
        Value::String(entry),
    );
}
