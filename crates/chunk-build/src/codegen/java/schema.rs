use std::{collections::BTreeMap, fmt::Write, io};

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

    pub fn child(&mut self, name: &str, origin: &str) -> io::Result<Self> {
        self.declare(name, origin)?;
        let mut path = self.path.clone();
        path.push(name.into());
        Ok(Self::new(path))
    }
}

pub(super) struct Generator {
    pub root: String,
    pub ids: BTreeMap<String, String>,
}

impl Default for Generator {
    fn default() -> Self {
        Self { root: "BackendTypes".into(), ids: BTreeMap::new() }
    }
}

pub(super) enum Validation {
    Required,
    Null,
    Number,
    Integer,
    String,
    Literal(Value),
    Nullable(Box<Self>),
    Array(Box<Self>),
}

impl Validation {
    fn is_array(&self) -> bool {
        match self {
            Self::Array(_) => true,
            Self::Nullable(inner) => inner.is_array(),
            _ => false,
        }
    }

    pub fn kotlin_type(&self, java: &str) -> String {
        match self {
            Self::Null | Self::Literal(Value::Null) => format!("{java}?"),
            Self::Nullable(inner) => format!("{}?", inner.kotlin_type(java).trim_end_matches('?')),
            Self::Array(inner) => format!("List<{}>", inner.kotlin_type(&java[5..java.len() - 1])),
            _ => java.into(),
        }
    }

    pub fn checks(&self, value: &str, depth: usize) -> String {
        match self {
            Self::Required => format!("Objects.requireNonNull({value});"),
            Self::Null => format!("BackendValues.checkNull({value});"),
            Self::Number => format!("BackendValues.checkNumber({value});"),
            Self::Integer => format!("BackendValues.checkInteger({value});"),
            Self::String => format!("BackendValues.checkString({value});"),
            Self::Literal(expected) => {
                let expected = match expected {
                    Value::Number(n) if n.is_i64() => format!("{n}L"),
                    Value::Number(n) => format!("{n}d"),
                    other => other.to_string(),
                };
                format!("BackendValues.checkLiteral({value}, {expected});")
            }
            Self::Nullable(inner) => format!("if ({value} != null) {{ {} }}", inner.checks(value, depth)),
            Self::Array(inner) => {
                let item = format!("_item{depth}");
                format!("BackendValues.checkArray({value}, {item} -> {{ {} }});", inner.checks(&item, depth + 1))
            }
        }
    }
}

impl Generator {
    pub fn schema(&mut self, scope: &mut Scope, schema: &Schema, name: &str, origin: &str) -> io::Result<Type> {
        let (ty, validation) = match schema {
            Schema::Null => ("Void".into(), Validation::Null),
            Schema::Boolean => ("Boolean".into(), Validation::Required),
            Schema::Number => ("Double".into(), Validation::Number),
            Schema::Integer => ("Long".into(), Validation::Integer),
            Schema::String => ("String".into(), Validation::String),
            Schema::Player => ("PlayerId".into(), Validation::Required),
            Schema::Session => ("SessionId".into(), Validation::Required),
            Schema::Id { table } => {
                let name = self
                    .ids
                    .entry(table.clone())
                    .or_insert_with(|| names::type_name(table, &[self.root.clone(), "Ids".into()]));
                (format!("{}.Ids.{name}", self.root), Validation::Required)
            }
            Schema::Literal { value } => {
                let ty = match value {
                    Value::Null => "Void",
                    Value::Bool(_) => "Boolean",
                    Value::String(_) => "String",
                    Value::Number(n) if n.is_i64() => "Long",
                    Value::Number(_) => "Double",
                    _ => unreachable!("validated literal"),
                };
                (ty.into(), Validation::Literal(value.clone()))
            }
            Schema::Nullable { value } => {
                let inner = self.schema(scope, value, name, origin)?;
                (inner.ty, Validation::Nullable(Box::new(inner.validation)))
            }
            Schema::Array { items } => {
                let item = self.schema(scope, items, &format!("{name}Item"), origin)?;
                (format!("List<{}>", item.ty), Validation::Array(Box::new(item.validation)))
            }
            Schema::Enum { values } => {
                let name = names::type_name(name, &scope.path);
                let mut child = scope.child(&name, origin)?;
                let mut constants = Vec::new();
                for value in values {
                    let constant = names::field_name(value);
                    child.declare(&constant, value)?;
                    constants.push(format!("@JsonProperty({}) {constant}", quote(value)));
                }
                scope.declarations.push(format!("public enum {name} {{ {} }}", constants.join(", ")));
                (child.path.join("."), Validation::Required)
            }
            Schema::Object { fields } => return self.object(scope, name, fields, origin, None),
            Schema::Union { variants } => return self.union(scope, name, variants, origin),
        };
        Ok(Type { ty, validation })
    }

    fn object(
        &mut self,
        parent: &mut Scope,
        name: &str,
        fields: &BTreeMap<String, Field>,
        origin: &str,
        implements: Option<&str>,
    ) -> io::Result<Type> {
        let name = names::type_name(name, &parent.path);
        let mut scope = parent.child(&name, origin)?;
        let ty = scope.path.join(".");
        let mut components = Vec::new();
        let mut checks = Vec::new();
        for (field, definition) in fields {
            let origin = format!("{origin}.{field}");
            let id = names::field_name(field);
            scope.declare(&id, &origin)?;
            let field_type = self.schema(&mut scope, &definition.schema, field, &origin)?;
            let annotation = if definition.optional { "@JsonInclude(JsonInclude.Include.NON_NULL) " } else { "" };
            components.push(format!(
                "{annotation}@JsonProperty(value = {}, required = {}) {} {id}",
                quote(field),
                !definition.optional,
                field_type.ty
            ));
            let mut check = field_type.validation.checks(&id, 0);
            if field_type.validation.is_array() {
                write!(check, " if ({id} != null) {id} = BackendValues.copyArray({id});").expect("write to String");
            }
            if !definition.optional || !matches!(field_type.validation, Validation::Required) {
                checks.push(if definition.optional { format!("if ({id} != null) {{ {check} }}") } else { check });
            }
        }
        let implements = implements.map_or(String::new(), |ty| format!(" implements {ty}"));
        parent.declarations.push(format!(
            "public record {name}({}){implements} {{\npublic {name} {{ {} }}\n{}\n}}",
            components.join(", "),
            checks.join(" "),
            scope.declarations.join("\n")
        ));
        Ok(Type { validation: Validation::Required, ty })
    }

    fn union(
        &mut self,
        parent: &mut Scope,
        name: &str,
        variants: &BTreeMap<String, Schema>,
        origin: &str,
    ) -> io::Result<Type> {
        let name = names::type_name(name, &parent.path);
        let mut scope = parent.child(&name, origin)?;
        let ty = scope.path.join(".");
        let mut subtypes = Vec::new();
        for (tag, variant) in variants {
            let Schema::Object { fields } = variant else { unreachable!("validated variant") };
            let variant = self.object(&mut scope, tag, fields, &format!("{origin} variant {tag}"), Some(&ty))?;
            subtypes.push(format!("@JsonSubTypes.Type(value = {}.class, name = {})", variant.ty, quote(tag)));
        }
        parent.declarations.push(format!(
            "@JsonTypeInfo(use = JsonTypeInfo.Id.NAME, property = \"type\")\n@JsonTypeResolver(TaggedUnionResolver.class)\n@JsonSubTypes({{{}}})\npublic sealed interface {name} {{\n{}\n}}",
            subtypes.join(", "), scope.declarations.join("\n")
        ));
        Ok(Type { validation: Validation::Required, ty })
    }
}
