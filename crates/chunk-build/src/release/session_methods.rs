use std::{
    collections::BTreeMap,
    io::{self, Cursor, Read},
};

use chunk_contract::{SessionMethodDeclaration, SessionMethods};
use serde::Deserialize;
use zip::ZipArchive;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    app: String,
    methods: Vec<Method>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Method {
    app: String,
    session: String,
    name: String,
    arguments: chunk_contract::Schema,
    result: chunk_contract::Schema,
    interface: String,
    binary_interface: String,
    function: String,
}

pub(super) fn validate(
    bytes: &[u8],
    app: &str,
    sessions: &[String],
    contract: Option<&SessionMethods>,
) -> io::Result<()> {
    let expected: BTreeMap<_, _> = contract
        .into_iter()
        .flat_map(|methods| &methods.methods)
        .filter(|method| method.app == app)
        .map(|method| ((method.session.clone(), method.name.clone()), method.clone()))
        .collect();
    let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(io::Error::other)?;
    let manifest = match archive.by_name("META-INF/chunk/session-methods.json") {
        Ok(mut entry) => {
            if entry.size() > 2 * 1024 * 1024 {
                return Err(io::Error::other("session method manifest size limit"));
            }
            let mut bytes = Vec::new();
            (&mut entry).take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > 2 * 1024 * 1024 {
                return Err(io::Error::other("session method manifest size limit"));
            }
            Some(serde_json::from_slice::<Manifest>(&bytes).map_err(io::Error::other)?)
        }
        Err(zip::result::ZipError::FileNotFound) if expected.is_empty() => None,
        Err(_) => return Err(io::Error::other("missing session method manifest")),
    };
    let Some(manifest) = manifest else {
        return Ok(());
    };
    if manifest.version != 1 || manifest.app != app {
        return Err(io::Error::other("session method manifest identity mismatch"));
    }
    let mut actual = BTreeMap::new();
    for method in manifest.methods {
        if method.app != app
            || !sessions.contains(&method.session)
            || !chunk_contract::class_name(&method.interface)
            || !chunk_contract::class_name(&method.binary_interface)
            || method.function.is_empty()
            || archive.by_name(&format!("{}.class", method.binary_interface.replace('.', "/"))).is_err()
        {
            return Err(io::Error::other("invalid packaged session method"));
        }
        let key = (method.session.clone(), method.name.clone());
        let declaration = SessionMethodDeclaration {
            app: method.app,
            session: method.session,
            name: method.name,
            arguments: method.arguments,
            result: method.result,
        };
        if actual.insert(key, declaration).is_some() {
            return Err(io::Error::other("duplicate packaged session method"));
        }
    }
    if actual != expected {
        return Err(io::Error::other("packaged session methods differ from backend contract"));
    }
    if !actual.is_empty() {
        let mut entry = archive
            .by_name("META-INF/services/dev.chunkzero.runtime.SessionMethodProvider")
            .map_err(io::Error::other)?;
        let mut registration = String::new();
        (&mut entry).take(65_537).read_to_string(&mut registration)?;
        drop(entry);
        let providers: Vec<_> = registration.lines().filter(|line| !line.trim().is_empty()).collect();
        if registration.len() > 65_536
            || providers.len() != 1
            || !chunk_contract::class_name(providers[0])
            || archive.by_name(&format!("{}.class", providers[0].replace('.', "/"))).is_err()
        {
            return Err(io::Error::other("invalid session method provider registration"));
        }
    }
    Ok(())
}
