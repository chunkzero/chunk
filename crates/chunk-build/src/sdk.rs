use std::{
    fs,
    io::{self, Write},
    path::Path,
};

use serde_json::{Value, json};

const SOURCES: &[(&str, &str)] = &[
    ("index.ts", include_str!("../sdk/src/index.ts")),
    ("destinations.ts", include_str!("../sdk/src/destinations.ts")),
    ("hooks.ts", include_str!("../sdk/src/hooks.ts")),
    ("functions.ts", include_str!("../sdk/src/functions.ts")),
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

    for (name, source) in SOURCES {
        write_changed(&project.join(".chunk/sdk").join(name), source.as_bytes())?;
    }
    write_changed(&project.join(".chunk/generated/index.ts"), include_bytes!("sdk/index.ts"))?;
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
