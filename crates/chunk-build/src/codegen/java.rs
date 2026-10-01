use std::{collections::BTreeMap, io};

use chunk_contract::{Field, FunctionKind, Schema, Visibility};

use super::{BackendMetadata, quote, validate_literals};

mod client;
mod configurations;
mod destinations;
mod names;
mod schema;
mod sessions;
mod support;

pub(super) struct Type {
    pub ty: String,
    validation: schema::Validation,
}

impl Type {
    pub fn kotlin_type(&self) -> String {
        self.validation.kotlin_type(&self.ty)
    }

    fn json_type(&self) -> String {
        format!(
            "JsonType.of(new TypeReference<{}>() {{}}, value -> {{ {} }})",
            self.ty,
            self.validation.checks("value", 0)
        )
    }
}

pub(super) struct Function {
    pub path: String,
    pub name: String,
    pub watch_name: Option<String>,
    pub reference: String,
    pub arguments: Type,
    pub result: Type,
    pub kind: FunctionKind,
    pub empty_arguments: bool,
}

pub(super) struct Namespace {
    pub type_name: String,
    pub member_name: String,
    pub children: Vec<Namespace>,
    pub functions: Vec<Function>,
    declarations: Vec<String>,
}

#[derive(Default)]
struct Node<'a> {
    children: BTreeMap<&'a str, Self>,
    functions: BTreeMap<&'a str, (&'a str, &'a chunk_contract::Function)>,
}

pub(super) struct Bindings {
    pub root: Namespace,
    declarations: String,
    sessions: Option<chunk_contract::SessionMethods>,
    configurations: Option<chunk_contract::SessionConfigurations>,
    destinations: Option<chunk_contract::DestinationManifest>,
}

impl Bindings {
    pub fn sources(&self, package: &str) -> io::Result<BTreeMap<String, String>> {
        if !names::valid_package(package) {
            return Err(io::Error::other("invalid Java package"));
        }
        let source = support::class_source(package, "BackendTypes", &self.declarations);
        let package_path = package.replace('.', "/");
        let mut files = BTreeMap::from([
            (format!("java/{package_path}/BackendTypes.java"), source),
            (format!("java-client/{package_path}/BackendClient.java"), client::source(package, &self.root)),
        ]);
        files.extend(sessions::sources(self.sessions.as_ref(), package)?);
        files.extend(configurations::sources(self.configurations.as_ref(), package)?);
        files.extend([destinations::sources(self.destinations.as_ref(), package)?]);
        Ok(files)
    }
}

pub(super) fn bindings(contract: &BackendMetadata) -> io::Result<Bindings> {
    let mut generator = schema::Generator::default();
    let mut documents = schema::Scope::new(vec!["BackendTypes".into(), "Documents".into()]);
    for (name, table) in &contract.tables {
        for field in table.fields.values() {
            validate_literals(&field.schema)?;
        }
        let mut fields = table.fields.clone();
        fields.insert("_id".into(), Field { schema: Schema::Id { table: name.clone() }, optional: false });
        generator.schema(&mut documents, &Schema::Object { fields }, name, &format!("table {name}"))?;
    }
    let mut root = Node::default();
    for (path, function) in &contract.functions {
        if function.visibility != Visibility::Public || function.kind == FunctionKind::Action {
            continue;
        }
        validate_literals(&function.arguments)?;
        validate_literals(&function.result)?;
        let mut parts = path.split('/').peekable();
        let mut node = &mut root;
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                node.functions.insert(part, (path, function));
            } else {
                node = node.children.entry(part).or_default();
            }
        }
    }
    let mut scope = schema::Scope::new(vec!["BackendTypes".into()]);
    scope.declare("Ids", "table IDs")?;
    scope.declare("Documents", "table documents")?;
    let root = describe(&mut generator, root, scope, "")?;
    let mut table_names = names::Names::default();
    let mut markers = Vec::new();
    for (table, name) in generator.ids {
        table_names.insert("BackendTypes.Ids", &name, &format!("table {table}"))?;
        markers.push(format!("public record {name}(@JsonValue String value) {{ @JsonCreator(mode = JsonCreator.Mode.DELEGATING) public {name} {{ BackendValues.tableId({}, value); }} }}", quote(&table)));
    }
    let declarations = [
        container("Ids", &markers.join("\n")),
        container("Documents", &documents.declarations.join("\n")),
        model_body(&root),
    ]
    .join("\n");
    Ok(Bindings {
        root,
        declarations,
        sessions: contract.contracts.session_methods.clone(),
        configurations: contract.contracts.session_configurations.clone(),
        destinations: contract.contracts.destinations.clone(),
    })
}

fn describe(
    generator: &mut schema::Generator,
    node: Node<'_>,
    mut scope: schema::Scope,
    member: &str,
) -> io::Result<Namespace> {
    let type_name = scope.path.last().expect("root type").clone();
    let path = scope.path.join(".");
    let mut methods = names::Names::default();
    let mut children = Vec::new();
    for (raw, child) in node.children {
        let name = names::type_name(raw, &scope.path);
        let member = names::member_name(raw);
        scope.declare(&name, &format!("namespace {raw}"))?;
        methods.insert(&path, &member, &format!("namespace {raw}"))?;
        let mut child_path = scope.path.clone();
        child_path.push(name);
        children.push(describe(generator, child, schema::Scope::new(child_path), &member)?);
    }
    let mut functions = Vec::new();
    for (raw, (wire_path, function)) in node.functions {
        let name = names::member_name(raw);
        scope.declare(&name, wire_path)?;
        methods.insert(&path, &name, wire_path)?;
        let watch_name = if function.kind == FunctionKind::Query {
            let name = format!("watch{}", names::type_base(raw));
            methods.insert(&path, &name, &format!("watch {wire_path}"))?;
            Some(name)
        } else {
            None
        };
        let base = names::type_base(raw);
        let arguments = generator.schema(
            &mut scope,
            &function.arguments,
            &format!("{base}Args"),
            &format!("{wire_path} arguments"),
        )?;
        let result =
            generator.schema(&mut scope, &function.result, &format!("{base}Result"), &format!("{wire_path} result"))?;
        functions.push(Function {
            path: wire_path.into(),
            name: name.clone(),
            watch_name,
            reference: format!("{path}.{name}"),
            arguments,
            result,
            kind: function.kind,
            empty_arguments: matches!(&function.arguments, Schema::Object { fields } if fields.is_empty()),
        });
    }
    Ok(Namespace { type_name, member_name: member.into(), children, functions, declarations: scope.declarations })
}

fn model_body(namespace: &Namespace) -> String {
    let mut declarations = namespace.declarations.clone();
    for child in &namespace.children {
        declarations.push(container(&child.type_name, &model_body(child)));
    }
    for function in &namespace.functions {
        let kind = match function.kind {
            FunctionKind::Query => "Query",
            FunctionKind::Mutation => "Mutation",
            FunctionKind::Action => unreachable!("actions are not JVM transaction bindings"),
        };
        // The reference's Java namespace is independent of its persisted backend path.
        declarations.push(format!(
            "public static final {kind}Ref<{}, {}> {} = new {kind}Ref<>({}, {}, {});",
            function.arguments.ty,
            function.result.ty,
            function.name,
            quote(&function.path),
            function.arguments.json_type(),
            function.result.json_type()
        ));
    }
    declarations.join("\n")
}

fn container(name: &str, body: &str) -> String {
    format!("public static final class {name} {{\nprivate {name}() {{}}\n{body}\n}}")
}
