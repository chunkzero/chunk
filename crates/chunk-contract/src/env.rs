//! A project's plain variables and the secret names it requires, declared in `chunk.toml`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Most variables one section may hold, most distinct variable names all sections may hold, and most secrets one
/// environment may hold.
pub const MAX_ENV_ENTRIES: usize = 256;
/// Most bytes one variable or secret value may hold.
pub const MAX_ENV_VALUE_BYTES: usize = 64 * 1024;
const MAX_ENVIRONMENTS: usize = 64;

/// `[vars]`, `[env.<name>.vars]` and `[secrets] required`. A release carries every section, and core resolves the
/// variables of the environment it serves, so one release can be promoted between environments unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvManifest {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vars: BTreeMap<String, String>,
    /// Per environment name, the variables that override `vars` there.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environments: BTreeMap<String, BTreeMap<String, String>>,
    /// The secrets the project needs; their values are set out of band.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub secrets: BTreeSet<String>,
}

impl EnvManifest {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty() && self.environments.is_empty() && self.secrets.is_empty()
    }

    /// The variables of the environment named `environment`: `vars`, with that environment's overrides applied.
    #[must_use]
    pub fn resolve(&self, environment: Option<&str>) -> BTreeMap<String, String> {
        let mut vars = self.vars.clone();
        if let Some(overrides) = environment.and_then(|name| self.environments.get(name)) {
            vars.extend(overrides.iter().map(|(name, value)| (name.clone(), value.clone())));
        }
        vars
    }

    /// # Errors
    /// Rejects invalid names, empty or oversized values, too many entries, and secrets named like variables.
    pub fn validate(&self) -> Result<(), String> {
        if self.environments.len() > MAX_ENVIRONMENTS {
            return Err(format!("at most {MAX_ENVIRONMENTS} environments may override variables"));
        }
        for (environment, vars) in std::iter::once((None, &self.vars))
            .chain(self.environments.iter().map(|(name, vars)| (Some(name.as_str()), vars)))
        {
            let section = environment.map_or_else(|| "vars".to_owned(), |name| format!("env.{name}.vars"));
            if environment.is_some_and(|name| !environment_name(name)) {
                return Err(format!("{section}: environment names are 1-63 lowercase letters, digits or hyphens"));
            }
            if vars.len() > MAX_ENV_ENTRIES {
                return Err(format!("{section} holds more than {MAX_ENV_ENTRIES} variables"));
            }
            for (name, value) in vars {
                if !valid_env_name(name) {
                    return Err(format!("{section}.{name}: {NAME_RULE}"));
                }
                if !valid_env_value(value) {
                    return Err(format!("{section}.{name}: {VALUE_RULE}"));
                }
                if self.secrets.contains(name) {
                    return Err(format!("{section}.{name} is also a required secret"));
                }
            }
        }
        let names: BTreeSet<_> = self.vars.keys().chain(self.environments.values().flat_map(BTreeMap::keys)).collect();
        if names.len() > MAX_ENV_ENTRIES {
            return Err(format!("vars and env.<name>.vars name more than {MAX_ENV_ENTRIES} variables in all"));
        }
        if self.secrets.len() > MAX_ENV_ENTRIES {
            return Err(format!("secrets.required lists more than {MAX_ENV_ENTRIES} names"));
        }
        if let Some(name) = self.secrets.iter().find(|name| !valid_env_name(name)) {
            return Err(format!("secrets.required: {name:?}: {NAME_RULE}"));
        }
        Ok(())
    }
}

const NAME_RULE: &str = "names are 1-128 letters, digits or underscores, not starting with a digit";
const VALUE_RULE: &str = "values are non-empty and at most 64 KiB";

/// Whether `name` matches `[A-Za-z_][A-Za-z0-9_]{0,127}`, as variable and secret names must.
#[must_use]
pub fn valid_env_name(name: &str) -> bool {
    crate::deployment::ascii_identifier(name, 128)
}

/// Whether `value` is non-empty and at most [`MAX_ENV_VALUE_BYTES`] long, as variable and secret values must be.
#[must_use]
pub fn valid_env_value(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_ENV_VALUE_BYTES
}

/// A management environment name: a DNS label of lowercase letters, digits and hyphens.
fn environment_name(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}
