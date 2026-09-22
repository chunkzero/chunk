use std::{collections::BTreeMap, io, path::Path};

use chunk_contract::{DomainScope, domain_path};

use super::{GENERATED, children, invalid};

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
    for entry in
        children(directory, "domain", |name| GENERATED.contains(&name))?.into_iter().filter(|c| c.kind.is_dir())
    {
        let child = if path.is_empty() { entry.name } else { format!("{path}/{}", entry.name) };
        if !domain_path(&child) || scopes.keys().any(|existing| existing.eq_ignore_ascii_case(&child)) {
            return Err(invalid(&entry.path, "invalid or case-colliding static domain path"));
        }
        scopes.insert(child.clone(), DomainScope { parent: Some(path.into()) });
        if scopes.len() > 256 {
            return Err(invalid(directory, "too many domain scopes"));
        }
        collect(&entry.path, &child, scopes)?;
    }
    Ok(())
}
