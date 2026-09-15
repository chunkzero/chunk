use std::{collections::BTreeMap, io, path::Path};

use chunk_contract::{DOMAIN_MANIFEST_VERSION, DomainManifest};

use super::sources::Source;

pub(super) fn manifest(root: &Path) -> io::Result<Option<DomainManifest>> {
    let scopes = crate::project::domains::discover(root)?;
    let apps = crate::project::discover_apps(root)?.into_iter().map(|app| (app.id, app.domain)).collect();
    if !root.join("server/domains").exists() {
        return Ok(None);
    }
    Ok(Some(DomainManifest { version: DOMAIN_MANIFEST_VERSION, scopes, apps, hooks: BTreeMap::new() }))
}

pub(super) fn hook_scope(source: &Source) -> Option<&str> {
    let path = source.namespace.strip_prefix("shared/domains/")?;
    if path == "hooks" { Some("") } else { path.strip_suffix("/hooks") }
}

#[cfg(test)]
mod tests;
