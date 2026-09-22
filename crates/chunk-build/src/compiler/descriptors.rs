use std::{fmt::Write, io, path::Path};

use super::{error, sources::Source};
use crate::project::AppMetadata;
use crate::quote;

struct Descriptor {
    kind: &'static str,
    sdk: Option<&'static str>,
    module: Option<&'static str>,
    export: Option<fn(name: &str, value: &str, binding: &str) -> String>,
    app: Option<fn(value: &str, app: &AppMetadata) -> String>,
    metadata: Vec<String>,
    seen: bool,
}

impl Descriptor {
    fn owns(&self, entry: &Source<'_>) -> bool {
        self.module.is_some_and(|module| entry.namespace == format!("shared/{module}"))
    }
}

/// Function, session method, destination and app configuration descriptors collected from entry exports.
pub(super) struct Descriptors([Descriptor; 4]);

impl Descriptors {
    pub(super) fn new(sdk: &Path, source: &mut String) -> io::Result<Self> {
        let descriptor = |kind, sdk, module, export, app| Descriptor {
            kind,
            sdk,
            module,
            export,
            app,
            metadata: Vec::new(),
            seen: false,
        };
        let descriptors = Self([
            descriptor(
                "Function",
                Some("functions.ts"),
                None,
                Some(|name, value, binding| {
                    format!("...(isFunction({value}) ? [[{name}, {{...{value}.contract, export:{binding}}}]] : [])")
                }),
                None,
            ),
            descriptor(
                "SessionMethod",
                Some("sessions.ts"),
                None,
                Some(|_, value, _| format!("...(isSessionMethod({value}) ? [{value}.contract] : [])")),
                None,
            ),
            descriptor(
                "Destination",
                Some("destinations.ts"),
                Some("destinations"),
                Some(|name, value, _| format!("...(isDestination({value}) ? [[{name}, {value}.contract]] : [])")),
                Some(|value, app| {
                    let defaults = serde_json::json!({
                        "machineProfile": app.runtime.machine_profile,
                        "maxPlayers": app.runtime.capacity,
                    });
                    format!("...appDestinations({value},{defaults})")
                }),
            ),
            descriptor("Configuration", None, None, None, Some(|value, _| format!("...appConfigurations({value})"))),
        ]);
        for descriptor in &descriptors.0 {
            if let Some(module) = descriptor.sdk {
                writeln!(
                    source,
                    "import {{ is{} }} from {};",
                    descriptor.kind,
                    quote(sdk.join(module).to_string_lossy())
                )
                .map_err(error)?;
            }
        }
        Ok(descriptors)
    }

    pub(super) fn add_module(&mut self, entry: &Source<'_>) -> io::Result<()> {
        for descriptor in &mut self.0 {
            if descriptor.owns(entry)
                && std::mem::replace(&mut descriptor.seen, true)
                && let Some(module) = descriptor.module
            {
                return Err(error(format!("multiple server/{module}.ts or {module}.mts modules")));
            }
        }
        Ok(())
    }

    pub(super) fn add_app(&mut self, value: &str, app: &AppMetadata) {
        for descriptor in &mut self.0 {
            if let Some(entry) = descriptor.app {
                descriptor.metadata.push(entry(value, app));
            }
        }
    }

    pub(super) fn add_default(&self, entry: &Source<'_>, value: &str, source: &mut String) -> io::Result<()> {
        for descriptor in self.0.iter().filter(|descriptor| descriptor.module.is_some()) {
            let kind = descriptor.kind;
            writeln!(
                source,
                "if(is{kind}({value})) throw new Error({});",
                quote(format!("{kind} descriptors require named exports: {}", entry.namespace))
            )
            .map_err(error)?;
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
        let name = quote(format!("{}/{exported}", entry.namespace));
        for descriptor in &mut self.0 {
            let Some(export) = descriptor.export else { continue };
            match descriptor.module {
                Some(module) if !descriptor.owns(entry) => {
                    let kind = descriptor.kind;
                    writeln!(
                        source,
                        "if(is{kind}({value})) throw new Error({});",
                        quote(format!(
                            "{kind} descriptors require named exports in server/{module}.ts or {module}.mts: {}",
                            entry.namespace
                        ))
                    )
                    .map_err(error)?;
                }
                _ => descriptor.metadata.push(export(&name, value, &quote(binding))),
            }
        }
        Ok(())
    }

    /// Joined metadata entries in table order: functions, session methods, destinations, configurations.
    pub(super) fn metadata(self) -> [String; 4] {
        self.0.map(|descriptor| descriptor.metadata.join(","))
    }
}
