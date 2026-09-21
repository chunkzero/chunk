use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use chunk_contract::{DomainScope, domain_path};
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, ExportDefaultDeclarationKind, Expression, ObjectExpression, ObjectPropertyKind, PropertyKey,
    PropertyKind, Statement,
};
use oxc_parser::Parser;
use oxc_span::SourceType;

use super::{AppMetadata, Inventory, RuntimeRequirements, invalid, require_file, valid_id};

pub(crate) struct Module {
    pub path: PathBuf,
    pub namespace: String,
    pub scope: String,
    pub app: bool,
    pub hooks: Vec<String>,
    pub commands: Vec<String>,
    pub destinations: BTreeMap<String, Destination>,
}

#[derive(serde::Serialize)]
pub(crate) struct Destination {
    pub implementation: String,
    pub key: String,
    pub machine_profile: Option<String>,
}

/// Parses every `app.ts` and `scope.ts` under `apps/`; the result only carries authored entries.
pub(crate) fn discover(root: &Path) -> io::Result<Inventory> {
    let mut inventory = Inventory::default();
    collect(&root.join("apps"), "", &mut inventory, 0)?;
    Ok(inventory)
}

fn collect(directory: &Path, relative: &str, inventory: &mut Inventory, depth: usize) -> io::Result<()> {
    if depth > 32 {
        return Err(invalid(directory, "app nesting limit"));
    }
    match fs::symlink_metadata(directory) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(invalid(directory, "app directories cannot be files or symlinks"));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(invalid(directory, error)),
        _ => {}
    }
    let scope_path = directory.join("scope.ts");
    let app_path = directory.join("app.ts");
    let has_scope = fs::symlink_metadata(&scope_path).is_ok();
    let has_app = fs::symlink_metadata(&app_path).is_ok();
    if has_app && (relative.is_empty() || directory.join("app.toml").exists()) {
        return Err(invalid(&app_path, "app.ts requires an app subdirectory and cannot coexist with app.toml"));
    }
    if has_scope || has_app {
        register_scope(directory, relative, inventory)?;
    }
    if has_scope {
        let declaration = parse(&scope_path, false)?;
        inventory.modules.push(Module {
            path: scope_path,
            namespace: if relative.is_empty() { "scopes".into() } else { format!("scopes/{relative}") },
            scope: relative.into(),
            app: false,
            hooks: declaration.hooks,
            commands: declaration.commands,
            destinations: declaration.destinations,
        });
    }
    if has_app {
        let declaration = parse(&app_path, true)?;
        let id = declaration.id.ok_or_else(|| invalid(&app_path, "app.ts requires an explicit literal id"))?;
        require_file(&directory.join("build.gradle.kts"))?;
        inventory.modules.push(Module {
            path: app_path,
            namespace: format!("apps/{id}/app"),
            scope: relative.into(),
            app: true,
            hooks: declaration.hooks,
            commands: declaration.commands,
            destinations: declaration.destinations,
        });
        inventory.apps.push(AppMetadata {
            id,
            directory: format!("apps/{relative}"),
            gradle_project: format!(":apps:{}", relative.replace('/', ":")),
            domain: relative.into(),
            runtime: declaration.runtime,
            sessions: declaration.sessions,
        });
    }
    // A legacy app is a leaf; its source/build directories are not new app roots.
    if directory.join("app.toml").exists() {
        return Ok(());
    }
    let mut entries = fs::read_dir(directory)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name().into_string().map_err(|_| invalid(&entry.path(), "app paths must be UTF-8"))?;
        if ["node_modules", "_generated", ".chunk", ".gradle", "build", "src", "server"].contains(&name.as_str())
            || name.starts_with('.')
        {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(invalid(&entry.path(), "app symlinks are unsupported"));
        }
        if !kind.is_dir() {
            continue;
        }
        let child = if relative.is_empty() { name } else { format!("{relative}/{name}") };
        collect(&entry.path(), &child, inventory, depth + 1)?;
    }
    Ok(())
}

fn register_scope(directory: &Path, relative: &str, inventory: &mut Inventory) -> io::Result<()> {
    if !domain_path(relative) {
        return Err(invalid(directory, "invalid static scope path"));
    }
    let mut current = String::new();
    for segment in relative.split('/').filter(|segment| !segment.is_empty()) {
        let ancestor = current.clone();
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(segment);
        if inventory.scopes.keys().any(|path| path != &current && path.eq_ignore_ascii_case(&current)) {
            return Err(invalid(directory, "case-colliding static scope path"));
        }
        inventory.scopes.entry(current.clone()).or_insert_with(|| DomainScope { parent: Some(ancestor) });
    }
    if relative.is_empty() {
        inventory.scopes.insert(String::new(), DomainScope { parent: None });
    }
    if inventory.scopes.len() > 256 {
        return Err(invalid(directory, "too many scopes"));
    }
    Ok(())
}

struct Declaration {
    id: Option<String>,
    runtime: RuntimeRequirements,
    sessions: BTreeMap<String, RuntimeRequirements>,
    hooks: Vec<String>,
    commands: Vec<String>,
    destinations: BTreeMap<String, Destination>,
}

fn parse(path: &Path, app: bool) -> io::Result<Declaration> {
    require_file(path)?;
    let bytes = crate::read_limited(path, 65_536).map_err(|error| invalid(path, error))?;
    let source = std::str::from_utf8(&bytes).map_err(|error| invalid(path, error))?;
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::ts()).parse();
    if let Some(error) = parsed.diagnostics.first() {
        return Err(invalid(path, error));
    }
    let expected = if app { "defineApp" } else { "defineScope" };
    let call = parsed
        .program
        .body
        .iter()
        .find_map(|statement| match statement {
            Statement::ExportDefaultDeclaration(declaration) => match &declaration.declaration {
                ExportDefaultDeclarationKind::CallExpression(call) => Some(call),
                _ => None,
            },
            _ => None,
        })
        .ok_or_else(|| invalid(path, format!("requires export default {expected}({{...}})")))?;
    if !matches!(&call.callee, Expression::Identifier(identifier) if identifier.name == expected)
        || call.arguments.len() != 1
    {
        return Err(invalid(path, format!("requires export default {expected}({{...}})")));
    }
    let Argument::ObjectExpression(object) = &call.arguments[0] else {
        return Err(invalid(path, "declaration must be an object literal"));
    };
    let fields = properties(path, object)?;
    let allowed = if app {
        &["id", "runtime", "implementations", "destinations", "hooks", "commands"][..]
    } else {
        &["hooks", "commands"][..]
    };
    if let Some(name) = fields.keys().find(|name| !allowed.contains(name)) {
        return Err(invalid(path, format!("unsupported {expected} field {name:?}")));
    }
    let id = fields.get("id").map(|value| literal_string(path, value)).transpose()?;
    if id.as_ref().is_some_and(|id| !valid_id(id)) {
        return Err(invalid(path, "app id must be an ASCII identifier of at most 128 characters"));
    }
    let runtime = fields.get("runtime").map(|value| runtime_requirements(path, value)).transpose()?.unwrap_or_default();
    let mut sessions = BTreeMap::new();
    if let Some(value) = fields.get("implementations") {
        for (name, value) in object_fields(path, value)? {
            if !valid_id(name)
                || sessions.len() >= 128
                || sessions.keys().any(|existing: &String| existing.eq_ignore_ascii_case(name))
            {
                return Err(invalid(path, "implementations requires at most 128 valid implementation IDs"));
            }
            let fields = object_fields(path, value)?;
            if fields.keys().any(|name| !["runtime", "config"].contains(name)) {
                return Err(invalid(path, "unsupported implementation field"));
            }
            sessions.insert(
                name.into(),
                fields.get("runtime").map(|value| runtime_requirements(path, value)).transpose()?.unwrap_or_default(),
            );
        }
    }
    if fields.contains_key("implementations") && sessions.is_empty() {
        return Err(invalid(path, "implementations must declare at least one implementation"));
    }
    let destinations = destinations(path, fields.get("destinations").copied(), &sessions)?;
    Ok(Declaration {
        id,
        runtime,
        sessions,
        destinations,
        hooks: descriptor_names(path, fields.get("hooks").copied())?,
        commands: descriptor_names(path, fields.get("commands").copied())?,
    })
}

fn destinations(
    path: &Path,
    expression: Option<&Expression<'_>>,
    sessions: &BTreeMap<String, RuntimeRequirements>,
) -> io::Result<BTreeMap<String, Destination>> {
    let mut destinations = BTreeMap::new();
    if let Some(value) = expression {
        for (name, value) in object_fields(path, value)? {
            if !valid_id(name) {
                return Err(invalid(path, "destination keys must be ASCII identifiers"));
            }
            let options = object_fields(path, value)?;
            if options.keys().any(|key| {
                !["implementation", "key", "machineProfile", "maxPlayers", "config", "overflow", "emptyTimeoutSeconds"]
                    .contains(key)
            }) {
                return Err(invalid(path, "unsupported destination option"));
            }
            let implementation = options
                .get("implementation")
                .map(|value| literal_string(path, value))
                .transpose()?
                .ok_or_else(|| invalid(path, "destination requires a literal implementation"))?;
            if !(sessions.contains_key(&implementation) || sessions.is_empty() && implementation == "default") {
                return Err(invalid(path, "destination references an undeclared implementation"));
            }
            let key = options
                .get("key")
                .map(|value| literal_string(path, value))
                .transpose()?
                .ok_or_else(|| invalid(path, "destination requires a literal key"))?;
            let machine_profile = options.get("machineProfile").map(|value| literal_string(path, value)).transpose()?;
            destinations.insert(name.into(), Destination { implementation, key, machine_profile });
        }
    }
    Ok(destinations)
}

fn descriptor_names(path: &Path, expression: Option<&Expression<'_>>) -> io::Result<Vec<String>> {
    let Some(expression) = expression else {
        return Ok(Vec::new());
    };
    object_fields(path, expression)?
        .into_keys()
        .map(|name| {
            if !valid_id(name) {
                return Err(invalid(path, "hook and command keys must be ASCII identifiers"));
            }
            Ok(name.into())
        })
        .collect()
}

fn runtime_requirements(path: &Path, expression: &Expression<'_>) -> io::Result<RuntimeRequirements> {
    let mut runtime = RuntimeRequirements::default();
    for (name, value) in object_fields(path, expression)? {
        match name {
            "machineProfile" => runtime.machine_profile = Some(literal_string(path, value)?),
            "maxPlayers" => {
                let Expression::NumericLiteral(number) = value else {
                    return Err(invalid(path, "maxPlayers must be a literal integer"));
                };
                if number.value.fract() != 0.0 || !(1.0..=128.0).contains(&number.value) {
                    return Err(invalid(path, "maxPlayers must be between 1 and 128"));
                }
                runtime.capacity = Some(
                    number
                        .raw
                        .as_ref()
                        .and_then(|raw| raw.parse::<u32>().ok())
                        .ok_or_else(|| invalid(path, "maxPlayers must be a decimal integer"))?,
                );
            }
            _ => return Err(invalid(path, format!("unsupported runtime field {name:?}"))),
        }
    }
    Ok(runtime)
}

fn literal_string(path: &Path, expression: &Expression<'_>) -> io::Result<String> {
    match expression {
        Expression::StringLiteral(value) => Ok(value.value.to_string()),
        _ => Err(invalid(path, "metadata must use literal strings")),
    }
}

fn object_fields<'a>(path: &Path, expression: &'a Expression<'a>) -> io::Result<BTreeMap<&'a str, &'a Expression<'a>>> {
    let Expression::ObjectExpression(object) = expression else {
        return Err(invalid(path, "metadata and descriptor maps must be object literals"));
    };
    properties(path, object)
}

fn properties<'a>(path: &Path, object: &'a ObjectExpression<'a>) -> io::Result<BTreeMap<&'a str, &'a Expression<'a>>> {
    let mut fields = BTreeMap::new();
    for property in &object.properties {
        let ObjectPropertyKind::ObjectProperty(property) = property else {
            return Err(invalid(path, "spreads are unsupported in statically discovered declarations"));
        };
        if property.computed || property.method || property.kind != PropertyKind::Init {
            return Err(invalid(path, "declaration keys must be static properties"));
        }
        let name = match &property.key {
            PropertyKey::StaticIdentifier(identifier) => identifier.name.as_str(),
            PropertyKey::StringLiteral(value) => value.value.as_str(),
            _ => return Err(invalid(path, "declaration keys must be literal names")),
        };
        if fields.insert(name, &property.value).is_some() {
            return Err(invalid(path, format!("duplicate declaration key {name:?}")));
        }
    }
    Ok(fields)
}
