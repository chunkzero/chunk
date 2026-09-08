use std::collections::{BTreeMap, BTreeSet};

use chunk_contract::{Field, Schema};
use serde_json::Value;

use super::quote;

pub(super) struct Type {
    pub ty: String,
    pub codec: String,
}

pub(super) struct Generator {
    pub declarations: Vec<String>,
    pub ids: BTreeSet<String>,
    pub codecs: BTreeSet<String>,
}

impl Generator {
    pub fn schema(&mut self, schema: &Schema, name: &str) -> Type {
        let (ty, codec) = match schema {
            Schema::Null => ("NullValue".into(), "Codecs.NULL".into()),
            Schema::Boolean => ("Boolean".into(), "Codecs.BOOLEAN".into()),
            Schema::Number => ("Double".into(), "Codecs.NUMBER".into()),
            Schema::Integer => ("Long".into(), "Codecs.INTEGER".into()),
            Schema::String => ("String".into(), "Codecs.STRING".into()),
            Schema::Player => ("PlayerId".into(), "Codecs.PLAYER".into()),
            Schema::Session => ("SessionId".into(), "Codecs.SESSION".into()),
            Schema::Id { table } => {
                self.ids.insert(table.clone());
                (
                    format!("Id<Table${table}>"),
                    format!("Codecs.<Table${table}>id({})", quote(table)),
                )
            }
            Schema::Literal { value } => match value {
                Value::Null => ("NullValue".into(), "Codecs.NULL".into()),
                Value::Bool(value) => ("Boolean".into(), format!("Codecs.literal(Codecs.BOOLEAN, {value})")),
                Value::String(value) => (
                    "String".into(),
                    format!("Codecs.literal(Codecs.STRING, {})", quote(value)),
                ),
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
                let item = self.schema(items, &format!("{name}$Item"));
                (format!("List<{}>", item.ty), format!("Codecs.array({})", item.codec))
            }
            Schema::Object { fields } => return self.object(name, fields),
            Schema::Union { variants } => return self.union(name, variants),
        };
        Type { ty, codec }
    }

    fn object(&mut self, name: &str, fields: &BTreeMap<String, Field>) -> Type {
        self.codecs.insert(format!("{name}$Codec"));
        let mut components = Vec::new();
        let mut reads = Vec::new();
        let mut writes = Vec::new();
        let mut checks = Vec::new();
        for (field, definition) in fields {
            let ty = self.schema(&definition.schema, &format!("{name}${field}"));
            let id = identifier(field);
            checks.push(format!("Objects.requireNonNull({id});"));
            if definition.optional {
                components.push(format!("FieldValue<{}> {id}", ty.ty));
                reads.push(format!("Codecs.optional(object, {}, {})", quote(field), ty.codec));
                writes.push(format!(
                    "Codecs.optional(object, {}, {}, value.{id}());",
                    quote(field),
                    ty.codec
                ));
            } else {
                components.push(format!("{} {id}", ty.ty));
                reads.push(format!("Codecs.field(object, {}, {})", quote(field), ty.codec));
                writes.push(format!(
                    "object.add({}, {}.encode(value.{id}()));",
                    quote(field),
                    ty.codec
                ));
                if matches!(definition.schema, Schema::Array { .. }) {
                    checks.push(format!("{id} = List.copyOf({id});"));
                }
            }
        }
        self.declarations.push(format!("public record {name}({}) {{ public {name} {{ {} }} }}\npublic static final Codec<{name}> {name}$Codec = Codecs.of(input -> {{ var object = Codecs.object(input, Set.of({})); return new {name}({}); }}, value -> {{ var object = new JsonObject(); {} return object; }});", components.join(", "), checks.join(" "), fields.keys().map(|name| quote(name)).collect::<Vec<_>>().join(","), reads.join(","), writes.join(" ")));
        Type {
            ty: name.into(),
            codec: format!("{name}$Codec"),
        }
    }

    fn union(&mut self, name: &str, variants: &[Schema]) -> Type {
        self.codecs.insert(format!("{name}$Codec"));
        let mut types = Vec::new();
        let mut reads = Vec::new();
        let mut writes = Vec::new();
        for (index, variant) in variants.iter().enumerate() {
            let ty = self.schema(variant, &format!("{name}$Value{index}"));
            types.push(format!("record V{index}({} value) implements {name} {{ public V{index} {{ Objects.requireNonNull(value); }} }}", ty.ty));
            reads.push(format!(
                "try {{ return new {name}.V{index}({}.decode(input)); }} catch (IllegalArgumentException ignored) {{}}",
                ty.codec
            ));
            writes.push(format!(
                "if (value instanceof {name}.V{index} variant) return {}.encode(variant.value());",
                ty.codec
            ));
        }
        self.declarations.push(format!("public sealed interface {name} {{ {} }}\npublic static final Codec<{name}> {name}$Codec = Codecs.of(input -> {{ {} throw new IllegalArgumentException(\"No union variant matched\"); }}, value -> {{ {} throw new IllegalArgumentException(\"Unknown union variant\"); }});", types.join("\n"), reads.join("\n"), writes.join("\n")));
        Type {
            ty: name.into(),
            codec: format!("{name}$Codec"),
        }
    }
}

pub(super) fn identifier(name: &str) -> String {
    const RESERVED: &[&str] = &[
        "Codecs",
        "Objects",
        "List",
        "Set",
        "abstract",
        "assert",
        "boolean",
        "break",
        "byte",
        "case",
        "catch",
        "char",
        "class",
        "const",
        "continue",
        "default",
        "do",
        "double",
        "else",
        "enum",
        "extends",
        "final",
        "finally",
        "float",
        "for",
        "goto",
        "if",
        "implements",
        "import",
        "instanceof",
        "int",
        "interface",
        "long",
        "native",
        "new",
        "package",
        "private",
        "protected",
        "public",
        "return",
        "short",
        "static",
        "strictfp",
        "super",
        "switch",
        "synchronized",
        "this",
        "throw",
        "throws",
        "transient",
        "try",
        "void",
        "volatile",
        "while",
        "true",
        "false",
        "null",
        "record",
        "sealed",
        "permits",
        "yield",
        "var",
        "_",
        "wait",
        "notify",
        "notifyAll",
        "getClass",
        "clone",
        "finalize",
        "hashCode",
        "toString",
    ];
    if name.is_empty()
        || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        || name.as_bytes()[0].is_ascii_digit()
        || RESERVED.contains(&name)
    {
        format!("{name}$")
    } else {
        name.into()
    }
}

pub(super) fn client_method(name: &str, args: &str, result: &str, kind: chunk_contract::FunctionKind) -> String {
    match kind {
        chunk_contract::FunctionKind::Mutation => format!(
            "public CompletableFuture<{result}> call${name}({args} args, OperationId operation) {{ return session.mutate(BackendTypes.{name}, args, operation); }}"
        ),
        chunk_contract::FunctionKind::Query => format!(
            "public CompletableFuture<{result}> call${name}({args} args) {{ return session.query(BackendTypes.{name}, args); }}\npublic AutoCloseable watch${name}({args} args, Consumer<WatchState<{result}>> observer) {{ return session.watch(BackendTypes.{name}, args, observer); }}"
        ),
    }
}

pub(super) fn write_client(output: &std::path::Path, package: &str, methods: &[String]) -> std::io::Result<()> {
    let directory = output.join("java-client").join(package.replace('.', "/"));
    std::fs::create_dir_all(&directory)?;
    std::fs::write(
        directory.join("BackendClient.java"),
        format!(
            "// Generated by chunk-codegen.\npackage {package};\nimport {package}.BackendTypes.*;\nimport dev.chunkzero.backend.api.*;\nimport dev.chunkzero.backend.client.*;\nimport java.util.*;\nimport java.util.concurrent.CompletableFuture;\nimport java.util.function.Consumer;\npublic final class BackendClient {{\nprivate final BackendSession session;\npublic BackendClient(BackendSession session) {{ this.session = Objects.requireNonNull(session); }}\n{}\n}}\n",
            methods.join("\n")
        ),
    )
}
