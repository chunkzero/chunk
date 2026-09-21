use std::{collections::BTreeMap, io};

use chunk_contract::{SessionConfigurationDeclaration, SessionConfigurations};
use serde::Deserialize;

use super::manifest::{self, class_exists, read_registration};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    app: String,
    configurations: Vec<PackagedConfiguration>,
}

impl manifest::Manifest for Manifest {
    type Item = PackagedConfiguration;
    const FILE: &'static str = "META-INF/chunk/session-configurations.json";
    const LABEL: &'static str = "session configuration";
    fn parts(self) -> (u32, String, Vec<PackagedConfiguration>) {
        (self.version, self.app, self.configurations)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PackagedConfiguration {
    app: String,
    session: String,
    configuration: chunk_contract::Schema,
    interface: String,
    binary_interface: String,
    provider: String,
}

pub(super) fn validate(
    bytes: &[u8],
    app: &str,
    sessions: &[String],
    contract: Option<&SessionConfigurations>,
) -> io::Result<()> {
    let expected: BTreeMap<_, _> = contract
        .into_iter()
        .flat_map(|catalog| &catalog.configurations)
        .filter(|configuration| configuration.app == app)
        .map(|configuration| (configuration.session.clone(), configuration.clone()))
        .collect();
    manifest::validate::<Manifest, _, _>(
        bytes,
        app,
        &expected,
        |archive, configuration| {
            if configuration.app != app
                || !sessions.contains(&configuration.session)
                || !chunk_contract::class_name(&configuration.interface)
                || !chunk_contract::class_name(&configuration.binary_interface)
                || !chunk_contract::class_name(&configuration.provider)
                || !class_exists(archive, &configuration.binary_interface)
                || !class_exists(archive, &configuration.provider)
            {
                return Ok(None);
            }
            let registration = read_registration(archive, "dev.chunkzero.runtime.SessionProvider")?;
            if registration.len() > 65_536 || !registration.lines().any(|line| line.trim() == configuration.provider) {
                return Err(io::Error::other("unregistered configured session provider"));
            }
            let declaration = SessionConfigurationDeclaration {
                app: configuration.app,
                session: configuration.session.clone(),
                configuration: configuration.configuration,
            };
            Ok(Some((configuration.session, declaration)))
        },
        |_, _| Ok(()),
    )
}
