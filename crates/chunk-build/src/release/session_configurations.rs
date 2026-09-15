use std::{
    collections::BTreeMap,
    io::{self, Cursor, Read},
};

use chunk_contract::{SessionConfigurationDeclaration, SessionConfigurations};
use serde::Deserialize;
use zip::ZipArchive;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    app: String,
    configurations: Vec<PackagedConfiguration>,
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
    let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(io::Error::other)?;
    let manifest = match archive.by_name("META-INF/chunk/session-configurations.json") {
        Ok(mut entry) => {
            let mut bytes = Vec::new();
            (&mut entry).take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > 2 * 1024 * 1024 {
                return Err(io::Error::other("session configuration manifest size limit"));
            }
            Some(serde_json::from_slice::<Manifest>(&bytes).map_err(io::Error::other)?)
        }
        Err(zip::result::ZipError::FileNotFound) if expected.is_empty() => None,
        Err(_) => return Err(io::Error::other("missing session configuration manifest")),
    };
    let Some(manifest) = manifest else {
        return Ok(());
    };
    if manifest.version != 1 || manifest.app != app {
        return Err(io::Error::other("session configuration manifest identity mismatch"));
    }
    let mut actual = BTreeMap::new();
    for configuration in manifest.configurations {
        if configuration.app != app
            || !sessions.contains(&configuration.session)
            || !chunk_contract::class_name(&configuration.interface)
            || !chunk_contract::class_name(&configuration.binary_interface)
            || !chunk_contract::class_name(&configuration.provider)
            || archive.by_name(&format!("{}.class", configuration.binary_interface.replace('.', "/"))).is_err()
            || archive.by_name(&format!("{}.class", configuration.provider.replace('.', "/"))).is_err()
        {
            return Err(io::Error::other("invalid packaged session configuration"));
        }
        let mut registration = String::new();
        archive
            .by_name("META-INF/services/dev.chunkzero.runtime.SessionProvider")
            .map_err(io::Error::other)?
            .take(65_537)
            .read_to_string(&mut registration)?;
        if registration.len() > 65_536 || !registration.lines().any(|line| line.trim() == configuration.provider) {
            return Err(io::Error::other("unregistered configured session provider"));
        }
        let declaration = SessionConfigurationDeclaration {
            app: configuration.app,
            session: configuration.session.clone(),
            configuration: configuration.configuration,
        };
        if actual.insert(configuration.session, declaration).is_some() {
            return Err(io::Error::other("duplicate packaged session configuration"));
        }
    }
    if actual != expected {
        return Err(io::Error::other("packaged session configurations differ from backend contract"));
    }
    Ok(())
}
