use std::{
    fs,
    io::{self, Write},
    path::Path,
};

use serde_json::{Value, json};

const SOURCES: &[(&str, &str)] = &[
    ("apps.ts", include_str!("../sdk/src/apps.ts")),
    ("index.ts", include_str!("../sdk/src/index.ts")),
    ("commands.ts", include_str!("../sdk/src/commands.ts")),
    ("destinations.ts", include_str!("../sdk/src/destinations.ts")),
    ("command-effects.ts", include_str!("../sdk/src/command-effects.ts")),
    ("hooks.ts", include_str!("../sdk/src/hooks.ts")),
    ("jobs.ts", include_str!("../sdk/src/jobs.ts")),
    ("functions.ts", include_str!("../sdk/src/functions.ts")),
    ("sessions.ts", include_str!("../sdk/src/sessions.ts")),
    ("validators.ts", include_str!("../sdk/src/validators.ts")),
    ("documents.ts", include_str!("../sdk/src/documents.ts")),
    ("schema.ts", include_str!("../sdk/src/schema.ts")),
    ("web.d.ts", include_str!("../sdk/src/web.d.ts")),
];

/// Materializes the embedded SDK and schema-bound declarations for editor use.
/// Does not execute project code, type-check, or start services.
/// # Errors
/// Reports a missing schema, invalid package configuration, or filesystem errors.
pub fn generate_sdk(project: &Path) -> io::Result<()> {
    let project = project.canonicalize()?;
    if !project.join("server/schema/index.ts").is_file() {
        return Err(io::Error::other("missing explicitly composed server/schema/index.ts"));
    }
    let package_path = project.join("package.json");
    let mut package = match fs::read(&package_path) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
            .map_err(|error| io::Error::other(format!("{}: {error}", package_path.display())))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => json!({ "private": true }),
        Err(error) => return Err(error),
    };
    let original = package.clone();
    let object = package.as_object_mut().ok_or_else(|| io::Error::other("package.json must be an object"))?;
    let imports = object.entry("imports").or_insert_with(|| json!({}));
    let imports = imports.as_object_mut().ok_or_else(|| io::Error::other("package.json imports must be an object"))?;
    imports.insert("#chunk".into(), json!("./.chunk/generated/index.ts"));
    imports.insert("#chunk/schema".into(), json!("./.chunk/sdk/schema.ts"));
    imports.insert("#chunk/apps".into(), json!("./.chunk/generated/apps.ts"));

    for (name, source) in SOURCES {
        write_changed(&project.join(".chunk/sdk").join(name), source.as_bytes())?;
    }
    write_changed(&project.join(".chunk/generated/index.ts"), include_bytes!("sdk/index.ts"))?;
    write_changed(&project.join(".chunk/generated/apps.ts"), app_references(&project)?.as_bytes())?;
    if original != package {
        let mut bytes = serde_json::to_vec_pretty(&package).map_err(io::Error::other)?;
        bytes.push(b'\n');
        write_changed(&package_path, &bytes)?;
    }
    let config = project.join("tsconfig.json");
    if !config.exists() {
        write_changed(&config, include_bytes!("sdk/tsconfig.json"))?;
    }
    Ok(())
}

fn app_references(project: &Path) -> io::Result<String> {
    let apps = if project.join("chunk.toml").exists() {
        crate::project::inspect(project)?.apps
    } else {
        crate::project::discover_apps(project)?
    };
    let inventory = crate::project::authoring::discover(project)?;
    let mut references = serde_json::Map::new();
    for app in apps {
        let mut implementations = Vec::new();
        let mut destinations = serde_json::Map::new();
        if let Some(module) =
            inventory.modules.iter().find(|module| module.app && module.namespace == format!("apps/{}/app", app.id))
        {
            implementations = if app.sessions.is_empty() {
                vec!["default".to_string()]
            } else {
                app.sessions.keys().cloned().collect()
            };
            for (name, destination) in &module.destinations {
                let profile = destination
                    .machine_profile
                    .as_ref()
                    .or_else(|| {
                        app.sessions
                            .get(&destination.implementation)
                            .and_then(|runtime| runtime.machine_profile.as_ref())
                    })
                    .or(app.runtime.machine_profile.as_ref())
                    .ok_or_else(|| {
                        io::Error::other(format!(
                            "{}: destination {name} requires a machineProfile",
                            module.path.display()
                        ))
                    })?;
                destinations.insert(name.clone(), json!({"key":destination.key,"session_type":format!("{}/{}", app.id, destination.implementation),"machine_profile":profile}));
            }
        }
        references.insert(app.id.clone(), json!({"id": app.id,"implementations":implementations.into_iter().map(|session| (session.clone(),json!({"app":app.id,"session":session}))).collect::<serde_json::Map<String,Value>>(),"destinations":destinations}));
    }
    Ok(format!(
        "// Generated by chunk. Contains references only; no app modules are imported.\nexport const apps = {} as const;\n",
        reference_tree(&Value::Object(references))
    ))
}

fn reference_tree(value: &Value) -> String {
    match value {
        Value::Object(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(name, value)| format!(
                    "[{}]:{}",
                    serde_json::to_string(name).expect("string serialization"),
                    reference_tree(value)
                ))
                .collect::<Vec<_>>()
                .join(",\n")
        ),
        _ => serde_json::to_string(value).expect("reference serialization"),
    }
}

fn write_changed(path: &Path, content: &[u8]) -> io::Result<()> {
    match fs::read(path) {
        Ok(existing) if existing == content => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let parent = path.parent().ok_or_else(|| io::Error::other("output directory missing"))?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(content)?;
    file.persist(path).map_err(io::Error::other)?;
    Ok(())
}

#[cfg(test)]
mod tests;
