use std::{collections::BTreeMap, io};

use chunk_contract::{SessionMethodDeclaration, SessionMethods};
use serde::Deserialize;

use super::{
    jars::Classpath,
    manifest::{self, read_registration},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    app: String,
    methods: Vec<Method>,
}

impl manifest::Manifest for Manifest {
    type Item = Method;
    const FILE: &'static str = "META-INF/chunk/session-methods.json";
    const LABEL: &'static str = "session method";
    fn parts(self) -> (u32, String, Vec<Method>) {
        (self.version, self.app, self.methods)
    }
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
    classpath: &Classpath,
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
    manifest::validate::<Manifest, _, _>(
        bytes,
        app,
        &expected,
        |_, method| {
            if method.app != app
                || !sessions.contains(&method.session)
                || !chunk_contract::class_name(&method.interface)
                || !chunk_contract::class_name(&method.binary_interface)
                || method.function.is_empty()
                || !classpath.contains(&method.binary_interface)
            {
                return Ok(None);
            }
            let key = (method.session.clone(), method.name.clone());
            let declaration = SessionMethodDeclaration {
                app: method.app,
                session: method.session,
                name: method.name,
                arguments: method.arguments,
                result: method.result,
            };
            Ok(Some((key, declaration)))
        },
        |archive, actual| {
            if actual.is_empty() {
                return Ok(());
            }
            let registration = read_registration(archive, "dev.chunkzero.runtime.SessionMethodProvider")?;
            let providers: Vec<_> = registration.lines().filter(|line| !line.trim().is_empty()).collect();
            if registration.len() > 65_536
                || providers.len() != 1
                || !chunk_contract::class_name(providers[0])
                || !classpath.contains(providers[0])
            {
                return Err(io::Error::other("invalid session method provider registration"));
            }
            Ok(())
        },
    )
}
