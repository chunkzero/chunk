use std::{collections::BTreeMap, fs, io, path::Path};

use chunk_contract::{DomainScope, domain_path};

use super::invalid;

/// Merges static `server/domains` scopes with the scopes authored under `apps/`.
pub(crate) fn discover(
    root: &Path,
    authored: BTreeMap<String, DomainScope>,
) -> io::Result<BTreeMap<String, DomainScope>> {
    let mut scopes = BTreeMap::from([(String::new(), DomainScope { parent: None })]);
    collect(&root.join("server/domains"), "", &mut scopes)?;
    for (path, scope) in authored {
        if !path.is_empty() && scopes.contains_key(&path) {
            return Err(invalid(&root.join("apps").join(&path), "scope conflicts with legacy server/domains scope"));
        }
        scopes.insert(path, scope);
    }
    Ok(scopes)
}

fn collect(directory: &Path, path: &str, scopes: &mut BTreeMap<String, DomainScope>) -> io::Result<()> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(invalid(directory, "domain scopes require directories, not symlinks or files"));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(invalid(directory, error)),
        _ => {}
    }
    let mut entries = fs::read_dir(directory)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name().into_string().map_err(|_| invalid(&entry.path(), "domain paths must be UTF-8"))?;
        if ["node_modules", "_generated", ".chunk"].contains(&name.as_str()) {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(invalid(&entry.path(), "domain symlinks are unsupported"));
        }
        if !kind.is_dir() {
            continue;
        }
        let child = if path.is_empty() { name } else { format!("{path}/{name}") };
        if !domain_path(&child) || scopes.keys().any(|existing| existing.eq_ignore_ascii_case(&child)) {
            return Err(invalid(&entry.path(), "invalid or case-colliding static domain path"));
        }
        scopes.insert(child.clone(), DomainScope { parent: Some(path.into()) });
        if scopes.len() > 256 {
            return Err(invalid(directory, "too many domain scopes"));
        }
        collect(&entry.path(), &child, scopes)?;
    }
    Ok(())
}
