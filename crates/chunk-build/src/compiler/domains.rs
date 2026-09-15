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
}

impl DomainEntries {
    pub(super) fn new(root: &Path) -> io::Result<Self> {
        let scopes = crate::project::domains::discover(root)?;
        let apps = crate::project::discover_apps(root)?.into_iter().map(|app| (app.id, app.domain)).collect();
        let authored = !crate::project::authoring::discover(root)?.modules.is_empty();
        let manifest = (authored || root.join("server/domains").exists()).then_some(DomainManifest {
            version: DOMAIN_MANIFEST_VERSION,
            scopes,
            apps,
            hooks: BTreeMap::new(),
            commands: BTreeMap::new(),
        });
        Ok(Self {
            manifest,
            authored,
            descriptors: [("Hook", "hooks"), ("Command", "commands")].map(|(kind, module)| Descriptors {
                kind,
                module,
                metadata: Vec::new(),
                scopes: BTreeSet::new(),
            }),
        })
    }

    pub(super) fn add_module(&mut self, entry: &Source) -> io::Result<()> {
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
        entry: &Source,
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
            } else {
                format!(
                    "{kind} descriptors must be named exports in server/domains/**/{module}.ts or {module}.mts: {}",
                    entry.namespace
                )
            };
            if !self.authored {
                writeln!(source, "if(is{kind}({value})) throw new Error({});", quote(failure)).map_err(error)?;
            }
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

fn descriptor_scope<'a>(source: &'a Source, module: &str) -> Option<&'a str> {
    let path = source.namespace.strip_prefix("shared/domains/")?;
    if path == module { Some("") } else { path.strip_suffix(module)?.strip_suffix('/') }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod commands_tests;
