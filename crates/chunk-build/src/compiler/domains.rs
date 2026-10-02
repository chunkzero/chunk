use std::{collections::BTreeMap, fmt::Write, io};

use chunk_contract::{DOMAIN_MANIFEST_VERSION, DomainManifest};

use super::{error, sources::Source};
use crate::project::{Inventory, authoring::Module};
use crate::quote;

struct Descriptors {
    kind: &'static str,
    module: &'static str,
    binding_prefix: &'static str,
    names: fn(&Module) -> &[String],
    metadata: Vec<String>,
}

pub(super) struct DomainEntries {
    manifest: Option<DomainManifest>,
    descriptors: [Descriptors; 2],
    bound: Vec<String>,
    unbound: Vec<String>,
}

impl DomainEntries {
    pub(super) fn new(inventory: &Inventory) -> Self {
        let manifest = (!inventory.modules.is_empty()).then(|| DomainManifest {
            version: DOMAIN_MANIFEST_VERSION,
            scopes: inventory.scopes.clone(),
            apps: inventory.apps.iter().map(|app| (app.id.clone(), app.domain.clone())).collect(),
            hooks: BTreeMap::new(),
            commands: BTreeMap::new(),
        });
        Self {
            manifest,
            descriptors: [
                Descriptors {
                    kind: "Hook",
                    module: "hooks",
                    binding_prefix: "h",
                    names: |module| &module.hooks,
                    metadata: Vec::new(),
                },
                Descriptors {
                    kind: "Command",
                    module: "commands",
                    binding_prefix: "c",
                    names: |module| &module.commands,
                    metadata: Vec::new(),
                },
            ],
            bound: Vec::new(),
            unbound: Vec::new(),
        }
    }

    pub(super) fn add_export(
        &mut self,
        entry: &Source<'_>,
        exported: &str,
        value: &str,
        source: &mut String,
    ) -> io::Result<()> {
        for descriptors in &self.descriptors {
            let kind = descriptors.kind;
            if exported == "default" {
                writeln!(
                    source,
                    "if(is{kind}({value})) throw new Error({});",
                    quote(format!("{kind} descriptors require named exports: {}", entry.namespace))
                )
                .map_err(error)?;
            } else {
                // Modules may export helpers; the check runs once every binding is known.
                self.unbound.push(format!(
                    "if(is{kind}({value}) && !boundDescriptors.has({value})) throw new Error({});",
                    quote(format!(
                        "{kind} descriptors must be bound in a defineApp or defineScope {} map: {}/{exported}",
                        descriptors.module, entry.namespace
                    ))
                ));
            }
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

    pub(super) fn add_authored(&mut self, module: &Module, value: &str, source: &mut String) -> io::Result<()> {
        let predicate = if module.app { "isApp" } else { "isScope" };
        writeln!(
            source,
            "if(!{predicate}({value})) throw new Error({});",
            quote(format!("{} requires a {predicate} declaration", module.path.display()))
        )
        .map_err(error)?;
        for descriptors in &mut self.descriptors {
            for name in (descriptors.names)(module) {
                let id = format!("{}/{}/{}", module.namespace, descriptors.module, name);
                let binding = format!("a{}_{}", descriptors.binding_prefix, descriptors.metadata.len());
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

#[cfg(test)]
mod tests;
