use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
    io,
    path::Path,
};

use chunk_contract::{DOMAIN_MANIFEST_VERSION, DomainManifest};

use super::{
    bundle::{error, quote},
    sources::Source,
};
use crate::project::Inventory;

struct Descriptors {
    kind: &'static str,
    module: &'static str,
    metadata: Vec<String>,
    scopes: BTreeSet<String>,
}

pub(super) struct DomainEntries {
    manifest: Option<DomainManifest>,
    authored: bool,
    descriptors: [Descriptors; 2],
    bound: Vec<String>,
    unbound: Vec<String>,
}

impl DomainEntries {
    pub(super) fn new(root: &Path, inventory: &Inventory) -> Self {
        let authored = !inventory.modules.is_empty();
        let manifest = (authored || root.join("server/domains").exists()).then(|| DomainManifest {
            version: DOMAIN_MANIFEST_VERSION,
            scopes: inventory.scopes.clone(),
            apps: inventory.apps.iter().map(|app| (app.id.clone(), app.domain.clone())).collect(),
            hooks: BTreeMap::new(),
            commands: BTreeMap::new(),
        });
        Self {
            manifest,
            authored,
            descriptors: [("Hook", "hooks"), ("Command", "commands")].map(|(kind, module)| Descriptors {
                kind,
                module,
                metadata: Vec::new(),
                scopes: BTreeSet::new(),
            }),
            bound: Vec::new(),
            unbound: Vec::new(),
        }
    }

    pub(super) fn add_module(&mut self, entry: &Source<'_>) -> io::Result<()> {
        for descriptors in &mut self.descriptors {
            if let Some(scope) = descriptor_scope(entry, descriptors.module)
                && !descriptors.scopes.insert(scope.into())
            {
                return Err(error(format!("multiple {} modules for domain {scope}", descriptors.module)));
            }
        }
        Ok(())
    }

    pub(super) fn add_export(
        &mut self,
        entry: &Source<'_>,
        exported: &str,
        value: &str,
        binding: &str,
        source: &mut String,
    ) -> io::Result<()> {
        for descriptors in &mut self.descriptors {
            let kind = descriptors.kind;
            let module = descriptors.module;
            let failure = if exported == "default" {
                format!("{kind} descriptors require named exports: {}", entry.namespace)
            } else if let Some(scope) = descriptor_scope(entry, module) {
                descriptors.metadata.push(format!(
                    "...(is{kind}({value}) ? [[{}, {{...{value}.contract, domain:{}, export:{}}}]] : [])",
                    quote(format!("{}/{exported}", entry.namespace)),
                    quote(scope),
                    quote(binding),
                ));
                continue;
            } else if self.authored {
                // Authored modules may export helpers; the check runs once every binding is known.
                self.unbound.push(format!(
                    "if(is{kind}({value}) && !boundDescriptors.has({value})) throw new Error({});",
                    quote(format!(
                        "{kind} descriptors must be bound in a defineApp or defineScope {module} map: {}/{exported}",
                        entry.namespace
                    ))
                ));
                continue;
            } else {
                format!(
                    "{kind} descriptors must be named exports in server/domains/**/{module}.ts or {module}.mts: {}",
                    entry.namespace
                )
            };
            writeln!(source, "if(is{kind}({value})) throw new Error({});", quote(failure)).map_err(error)?;
        }
        Ok(())
    }

    pub(super) fn bindings(&self, source: &mut String) -> io::Result<()> {
        writeln!(source, "const boundDescriptors = new Set([{}]);", self.bound.join(",")).map_err(error)?;
        for check in &self.unbound {
            writeln!(source, "{check}").map_err(error)?;
        }
        Ok(())
    }

    pub(super) fn add_authored(
        &mut self,
        module: &crate::project::authoring::Module,
        value: &str,
        source: &mut String,
    ) -> io::Result<()> {
        let predicate = if module.app { "isApp" } else { "isScope" };
        writeln!(
            source,
            "if(!{predicate}({value})) throw new Error({});",
            quote(format!("{} requires a {predicate} declaration", module.path.display()))
        )
        .map_err(error)?;
        for descriptors in &mut self.descriptors {
            let names = if descriptors.module == "hooks" { &module.hooks } else { &module.commands };
            for name in names {
                let id = format!("{}/{}/{}", module.namespace, descriptors.module, name);
                let binding = format!(
                    "a{}_{}",
                    if descriptors.module == "hooks" { "h" } else { "c" },
                    descriptors.metadata.len()
                );
                let descriptor = format!("{value}.{}[{}]", descriptors.module, quote(name));
                let kind = descriptors.kind;
                writeln!(
                    source,
                    "if(!is{kind}({descriptor})) throw new Error({});",
                    quote(format!("{id} requires a {kind} descriptor"))
                )
                .map_err(error)?;
                writeln!(source, "export const {binding} = (ctx,args) => invoke{kind}({descriptor},ctx,args);")
                    .map_err(error)?;
                self.bound.push(descriptor.clone());
                descriptors.metadata.push(format!(
                    "[{}, {{...{descriptor}.contract, domain:{}, export:{}}}]",
                    quote(id),
                    quote(&module.scope),
                    quote(binding)
                ));
            }
        }
        Ok(())
    }

    pub(super) fn metadata(&self) -> io::Result<String> {
        let Some(manifest) = &self.manifest else { return Ok(String::new()) };
        let mut source = format!(",domains:{{...{}", serde_json::to_string(manifest).map_err(error)?);
        for descriptors in &self.descriptors {
            write!(source, ",{}:Object.fromEntries([{}])", descriptors.module, descriptors.metadata.join(","))
                .map_err(error)?;
        }
        source.push('}');
        Ok(source)
    }
}

fn descriptor_scope<'a>(source: &'a Source<'_>, module: &str) -> Option<&'a str> {
    let path = source.namespace.strip_prefix("shared/domains/")?;
    if path == module { Some("") } else { path.strip_suffix(module)?.strip_suffix('/') }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod commands_tests;
