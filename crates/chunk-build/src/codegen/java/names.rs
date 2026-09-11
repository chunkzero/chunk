use std::{collections::BTreeMap, io};

const KEYWORDS: &[&str] = &[
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
    "as",
    "fun",
    "in",
    "is",
    "object",
    "typealias",
    "typeof",
    "val",
    "when",
];

const SUPPORT_NAMES: &[&str] = &[
    "BackendTypes",
    "BackendClient",
    "CoroutineBackendClient",
    "CoroutineBackend",
    "Flow",
    "Tables",
    "Documents",
    "Codecs",
    "Codec",
    "FieldValue",
    "Id",
    "PlayerId",
    "SessionId",
    "NullValue",
    "QueryRef",
    "MutationRef",
    "JsonObject",
    "Objects",
    "List",
    "Set",
    "String",
    "Boolean",
    "Double",
    "Long",
    "Object",
    "AutoCloseable",
    "CompletableFuture",
    "Consumer",
    "WatchState",
    "BackendSession",
    "OperationId",
    "IllegalArgumentException",
    "CODEC",
];

const OBJECT_METHODS: &[&str] =
    &["wait", "notify", "notifyAll", "getClass", "clone", "finalize", "equals", "hashCode", "toString"];

pub(super) fn valid_package(package: &str) -> bool {
    !package.is_empty()
        && package.split('.').all(|part| {
            part.bytes().next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && !KEYWORDS.contains(&part)
        })
}

pub(super) fn field_name(name: &str) -> String {
    if KEYWORDS.contains(&name) || SUPPORT_NAMES.contains(&name) || OBJECT_METHODS.contains(&name) {
        format!("{name}_")
    } else {
        name.into()
    }
}

pub(super) fn type_base(name: &str) -> String {
    let mut result = String::new();
    for part in name.split('_').filter(|part| !part.is_empty()) {
        result.push(char::from(part.as_bytes()[0]).to_ascii_uppercase());
        result.push_str(&part[1..]);
    }
    if result.is_empty() {
        result.push_str("Value");
    }
    if result.as_bytes()[0].is_ascii_digit() {
        result.insert_str(0, "Value");
    }
    result
}

pub(super) fn type_name(name: &str, ancestors: &[String]) -> String {
    let mut result = type_base(name);
    while SUPPORT_NAMES.contains(&result.as_str()) || ancestors.contains(&result) {
        result.push('_');
    }
    result
}

pub(super) fn member_name(name: &str) -> String {
    let mut result = type_base(name);
    result[..1].make_ascii_lowercase();
    field_name(&result)
}

#[derive(Default)]
pub(super) struct Names(BTreeMap<String, String>);

impl Names {
    pub fn insert(&mut self, scope: &str, name: &str, origin: &str) -> io::Result<()> {
        if let Some(previous) = self.0.insert(name.into(), origin.into()) {
            return Err(io::Error::other(format!("Java name {scope}.{name} collides between {previous} and {origin}")));
        }
        Ok(())
    }
}
