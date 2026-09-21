use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{Schema, deployment::identifier};

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMethodDeclaration {
    pub app: String,
    pub session: String,
    pub name: String,
    pub arguments: Schema,
    pub result: Schema,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(try_from = "RawSessionMethods")]
pub struct SessionMethods {
    pub version: u32,
    pub methods: Vec<SessionMethodDeclaration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSessionMethods {
    version: u32,
    methods: Vec<SessionMethodDeclaration>,
}

impl TryFrom<RawSessionMethods> for SessionMethods {
    type Error = &'static str;

    fn try_from(raw: RawSessionMethods) -> Result<Self, Self::Error> {
        let methods = Self { version: raw.version, methods: raw.methods };
        methods.validate()?;
        Ok(methods)
    }
}

impl SessionMethods {
    /// # Errors
    /// Rejects unsupported versions, duplicate identities and values without a wire schema.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.version != 1 {
            return Err("unsupported session method contract version");
        }
        if self.methods.len() > 256 {
            return Err("session method count limit");
        }
        let mut identities = BTreeSet::new();
        for method in &self.methods {
            for part in [&method.app, &method.session, &method.name] {
                if !identifier(part) {
                    return Err("invalid session method identity");
                }
            }
            if !identities.insert(format!("{}/{}/{}", method.app, method.session, method.name).to_ascii_lowercase()) {
                return Err("duplicate session method identity");
            }
            if !matches!(method.arguments, Schema::Object { .. }) {
                return Err("session method arguments must be an object");
            }
            method.arguments.validate(0)?;
            method.result.validate(0)?;
        }
        Ok(())
    }
}
