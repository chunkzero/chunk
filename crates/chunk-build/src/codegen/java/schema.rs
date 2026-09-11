use std::{collections::BTreeMap, io};

use chunk_contract::{Field, Schema};
use serde_json::Value;

use super::{Type, names};
use crate::codegen::quote;

pub(super) struct Scope {
    pub path: Vec<String>,
    pub names: names::Names,
    pub declarations: Vec<String>,
}

impl Scope {
    pub fn new(path: Vec<String>) -> Self {
        Self { path, names: names::Names::default(), declarations: Vec::new() }
    }

    pub fn declare(&mut self, name: &str, origin: &str) -> io::Result<()> {
        self.names.insert(&self.path.join("."), name, origin)
    }

    fn child(&mut self, name: &str, origin: &str) -> io::Result<Self> {
        self.declare(name, origin)?;
        let mut path = self.path.clone();
        path.push(name.into());
        Ok(Self::new(path))
    }
}

#[derive(Default)]
pub(super) struct Generator {
    pub ids: BTreeMap<String, String>,
}

impl Generator {
    pub fn schema(&mut self, scope: &mut Scope, schema: &Schema, name: &str, origin: &str) -> io::Result<Type> {
        let (ty, codec) = match schema {
            Schema::Null => ("NullValue".into(), "Codecs.NULL".into()),
            Schema::Boolean => ("Boolean".into(), "Codecs.BOOLEAN".into()),
            Schema::Number => ("Double".into(), "Codecs.NUMBER".into()),
            Schema::Integer => ("Long".into(), "Codecs.INTEGER".into()),
            Schema::String => ("String".into(), "Codecs.STRING".into()),
            Schema::Player => ("PlayerId".into(), "Codecs.PLAYER".into()),
            Schema::Session => ("SessionId".into(), "Codecs.SESSION".into()),
            Schema::Id { table } => {
                let marker = self
                    .ids
                    .entry(table.clone())
                    .or_insert_with(|| names::type_name(table, &["BackendTypes".into(), "Tables".into()]));
                (
                    format!("Id<BackendTypes.Tables.{marker}>"),
                    format!("Codecs.<BackendTypes.Tables.{marker}>id({})", quote(table)),
                )
            }
            Schema::Literal { value } => match value {
                Value::Null => ("NullValue".into(), "Codecs.NULL".into()),
                Value::Bool(value) => ("Boolean".into(), format!("Codecs.literal(Codecs.BOOLEAN, {value})")),
                Value::String(value) => ("String".into(), format!("Codecs.literal(Codecs.STRING, {})", quote(value))),
                Value::Number(value) => {
                    if let Some(integer) = value.as_i64() {
                        ("Long".into(), format!("Codecs.literal(Codecs.INTEGER, {integer}L)"))
                    } else {
                        ("Double".into(), format!("Codecs.literal(Codecs.NUMBER, {value}d)"))
                    }
                }
                _ => unreachable!("validated literal"),
            },
            Schema::Array { items } => {
                let item = self.schema(scope, items, &format!("{name}Item"), origin)?;
                (format!("List<{}>", item.ty), format!("Codecs.array({})", item.codec))
            }
            Schema::Object { fields } => return self.object(scope, name, fields, origin),
            Schema::Union { variants } => return self.union(scope, name, variants, origin),
        };
        Ok(Type { ty, codec })
    }

    fn object(
        &mut self,
        parent: &mut Scope,
        name: &str,
        fields: &BTreeMap<String, Field>,
        origin: &str,
    ) -> io::Result<Type> {
        let name = names::type_name(name, &parent.path);
        let mut scope = parent.child(&name, origin)?;
        scope.declare("CODEC", "generated codec")?;
        let ty = scope.path.join(".");
        let mut components = Vec::new();
        let mut reads = Vec::new();
        let mut writes = Vec::new();
        let mut checks = Vec::new();
        for (field, definition) in fields {
            let origin = format!("{origin}.{field}");
            let id = names::field_name(field);
            scope.declare(&id, &origin)?;
            let field_type = self.schema(&mut scope, &definition.schema, field, &origin)?;
            checks.push(format!("Objects.requireNonNull({id});"));
            if definition.optional {
                components.push(format!("FieldValue<{}> {id}", field_type.ty));
                reads.push(format!("Codecs.optional(object, {}, {})", quote(field), field_type.codec));
                writes.push(format!("Codecs.optional(object, {}, {}, value.{id}());", quote(field), field_type.codec));
            } else {
                components.push(format!("{} {id}", field_type.ty));
                reads.push(format!("Codecs.field(object, {}, {})", quote(field), field_type.codec));
                writes.push(format!("object.add({}, {}.encode(value.{id}()));", quote(field), field_type.codec));
                if matches!(definition.schema, Schema::Array { .. }) {
                    checks.push(format!("{id} = List.copyOf({id});"));
                }
            }
        }
        parent.declarations.push(format!(
            "public record {name}({}) {{\npublic {name} {{ {} }}\n{}\npublic static final Codec<{ty}> CODEC = Codecs.of(input -> {{ var object = Codecs.object(input, Set.of({})); return new {ty}({}); }}, value -> {{ var object = new JsonObject(); {} return object; }});\n}}",
            components.join(", "), checks.join(" "), scope.declarations.join("\n"),
            fields.keys().map(|name| quote(name)).collect::<Vec<_>>().join(","), reads.join(","), writes.join(" ")
        ));
        Ok(Type { codec: format!("{ty}.CODEC"), ty })
    }

    fn union(&mut self, parent: &mut Scope, name: &str, variants: &[Schema], origin: &str) -> io::Result<Type> {
        let name = names::type_name(name, &parent.path);
        let mut scope = parent.child(&name, origin)?;
        scope.declare("CODEC", "generated codec")?;
        let ty = scope.path.join(".");
        let mut reads = Vec::new();
        let mut writes = Vec::new();
        for (index, variant) in variants.iter().enumerate() {
            let origin = format!("{origin} variant {index}");
            let value_type = self.schema(&mut scope, variant, &format!("Value{index}"), &origin)?;
            let wrapper = names::type_name(&format!("V{index}"), &scope.path);
            scope.declare(&wrapper, &origin)?;
            scope.declarations.push(format!(
                "record {wrapper}({} value) implements {ty} {{ public {wrapper} {{ Objects.requireNonNull(value); }} }}",
                value_type.ty
            ));
            reads.push(format!(
                "try {{ return new {ty}.{wrapper}({}.decode(input)); }} catch (IllegalArgumentException ignored) {{}}",
                value_type.codec
            ));
            writes.push(format!(
                "if (value instanceof {ty}.{wrapper} variant) return {}.encode(variant.value());",
                value_type.codec
            ));
        }
        parent.declarations.push(format!(
            "public sealed interface {name} {{\n{}\nCodec<{ty}> CODEC = Codecs.of(input -> {{ {} throw new IllegalArgumentException(\"No union variant matched\"); }}, value -> {{ {} throw new IllegalArgumentException(\"Unknown union variant\"); }});\n}}",
            scope.declarations.join("\n"), reads.join("\n"), writes.join("\n")
        ));
        Ok(Type { codec: format!("{ty}.CODEC"), ty })
    }
}
