use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{AppArtifact, Schema, deployment::identifier, validate_wire_value};

pub const MAX_SESSION_CONFIGURATION_BYTES: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfigurationDeclaration {
    pub app: String,
    pub session: String,
    pub configuration: Schema,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(try_from = "RawSessionConfigurations")]
pub struct SessionConfigurations {
    pub version: u32,
    pub configurations: Vec<SessionConfigurationDeclaration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSessionConfigurations {
    version: u32,
    configurations: Vec<SessionConfigurationDeclaration>,
}

impl TryFrom<RawSessionConfigurations> for SessionConfigurations {
    type Error = &'static str;

    fn try_from(raw: RawSessionConfigurations) -> Result<Self, Self::Error> {
        let configurations = Self { version: raw.version, configurations: raw.configurations };
        configurations.validate()?;
        Ok(configurations)
    }
}

impl SessionConfigurations {
    /// # Errors
    /// Rejects unsupported versions, duplicate implementation identities and invalid schemas.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.version != 1 {
            return Err("unsupported session configuration contract version");
        }
        if self.configurations.len() > 256
            || serde_json::to_vec(self).map_err(|_| "invalid session configuration catalog")?.len() > 2 * 1024 * 1024
        {
            return Err("session configuration catalog size limit");
        }
        let mut identities = BTreeSet::new();
        for declaration in &self.configurations {
            if !identifier(&declaration.app) || !identifier(&declaration.session) {
                return Err("invalid session configuration identity");
            }
            if !identities.insert(format!("{}/{}", declaration.app, declaration.session).to_ascii_lowercase()) {
                return Err("duplicate session configuration identity");
            }
            if !matches!(declaration.configuration, Schema::Object { .. }) {
                return Err("session configuration must be an object");
            }
            declaration.configuration.validate(0)?;
        }
        Ok(())
    }

    /// # Errors
    /// Rejects configuration schemas outside the exact release's implementation catalog.
    pub fn validate_apps(&self, apps: &BTreeMap<String, AppArtifact>) -> Result<(), &'static str> {
        self.validate()?;
        for declaration in &self.configurations {
            if apps.get(&declaration.app).is_none_or(|app| !app.sessions.contains_key(&declaration.session)) {
                return Err("session configuration references unknown app session");
            }
        }
        Ok(())
    }

    /// # Errors
    /// Rejects values that differ from the implementation's immutable creation contract.
    pub fn validate_configuration(&self, session_type: &str, value: &Value) -> Result<(), &'static str> {
        validate_session_configuration(Some(self), session_type, value)
    }
}

/// Validates the wire value and the exact implementation schema. Legacy implementations accept `{}`.
/// # Errors
/// Rejects non-object, oversized, unsafe or undeclared configuration values.
pub fn validate_session_configuration(
    configurations: Option<&SessionConfigurations>,
    session_type: &str,
    value: &Value,
) -> Result<(), &'static str> {
    validate_configuration_value(value)?;
    let schema = configurations.and_then(|catalog| {
        catalog.configurations.iter().find(|declaration| {
            session_type.split_once('/') == Some((declaration.app.as_str(), declaration.session.as_str()))
        })
    });
    if schema.map_or_else(
        || value.as_object().is_some_and(serde_json::Map::is_empty),
        |declaration| declaration.configuration.accepts(value),
    ) {
        Ok(())
    } else {
        Err("session configuration differs from its implementation schema")
    }
}

pub(crate) fn validate_configuration_value(value: &Value) -> Result<(), &'static str> {
    if !value.is_object()
        || serde_json::to_vec(value).map_err(|_| "invalid session configuration")?.len()
            > MAX_SESSION_CONFIGURATION_BYTES
    {
        return Err("session configuration must be an object within 65536 bytes");
    }
    validate_wire_value(value)
}

#[cfg(test)]
mod tests;
