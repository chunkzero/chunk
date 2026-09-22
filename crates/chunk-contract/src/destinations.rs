use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{AppArtifact, deployment::identifier};

const DESTINATION_MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Destination {
    pub key: String,
    pub session_type: String,
    pub machine_profile: String,
}

impl Destination {
    /// # Errors
    /// Rejects malformed keys and session/profile identities.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.key.is_empty()
            || self.key.len() > 128
            || self.key.chars().any(char::is_control)
            || !self.session_type.split_once('/').is_some_and(|(app, session)| identifier(app) && identifier(session))
            || self.machine_profile.is_empty()
            || self.machine_profile.len() > 128
            || !self.machine_profile.bytes().all(|ch| ch.is_ascii_alphanumeric() || b"_-".contains(&ch))
        {
            return Err("invalid destination key, session type or profile");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestinationOverflow {
    Replicate,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCreation {
    pub capacity: u32,
    pub configuration: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationPolicy {
    pub destination: Destination,
    pub overflow: DestinationOverflow,
    pub empty_timeout_seconds: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creation: Option<SessionCreation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationManifest {
    pub version: u32,
    pub entries: BTreeMap<String, DestinationPolicy>,
}

impl DestinationManifest {
    /// # Errors
    /// Rejects duplicate scoped keys, unsupported policy versions and unbounded idle timeouts.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.version != DESTINATION_MANIFEST_VERSION || self.entries.is_empty() || self.entries.len() > 256 {
            return Err("invalid destination manifest version or size");
        }
        let mut names = BTreeSet::new();
        let mut keys = BTreeSet::new();
        for (id, policy) in &self.entries {
            let legacy = id.strip_prefix("shared/destinations/").is_some_and(identifier);
            let local = id.strip_prefix("apps/").and_then(|path| path.split_once("/destinations/")).is_some_and(
                |(app, key)| {
                    identifier(app)
                        && identifier(key)
                        && policy.destination.session_type.split_once('/').is_some_and(|(owner, _)| owner == app)
                },
            );
            if !(legacy || local) || !names.insert(id.to_ascii_lowercase()) {
                return Err("invalid destination declaration identity");
            }
            policy.destination.validate()?;
            if let Some(creation) = &policy.creation {
                if !(1..=128).contains(&creation.capacity) {
                    return Err("destination creation capacity must be between 1 and 128");
                }
                crate::session_configurations::validate_configuration_value(&creation.configuration)?;
            }
            if !keys.insert((&policy.destination.session_type, &policy.destination.key)) {
                return Err("destination key already declared for this session type");
            }
            if !(1..=86_400).contains(&policy.empty_timeout_seconds) {
                return Err("empty destination timeout must be between 1 and 86400 seconds");
            }
        }
        Ok(())
    }

    /// # Errors
    /// Rejects unknown implementations and implicit changes to their default hosting profile.
    pub fn validate_apps(&self, apps: &BTreeMap<String, AppArtifact>) -> Result<(), &'static str> {
        self.validate()?;
        for policy in self.entries.values() {
            let (app, session) = policy.destination.session_type.split_once('/').ok_or("invalid session type")?;
            let declaration = apps
                .get(app)
                .and_then(|app| app.sessions.get(session))
                .ok_or("destination references unknown app session")?;
            if policy.creation.is_none() && declaration.machine_profile != policy.destination.machine_profile {
                return Err("destination profile differs from its immutable app session");
            }
        }
        Ok(())
    }

    /// # Errors
    /// Rejects creation values that differ from the exact implementation schema.
    pub fn validate_configurations(
        &self,
        configurations: Option<&crate::SessionConfigurations>,
    ) -> Result<(), &'static str> {
        for policy in self.entries.values() {
            let empty = serde_json::json!({});
            let value = policy.creation.as_ref().map_or(&empty, |creation| &creation.configuration);
            crate::validate_session_configuration(configurations, &policy.destination.session_type, value)?;
        }
        Ok(())
    }

    #[must_use]
    pub fn policy(&self, session_type: &str, key: &str) -> Option<&DestinationPolicy> {
        self.entries
            .values()
            .find(|policy| policy.destination.session_type == session_type && policy.destination.key == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identities_and_catalog_bindings_are_immutable_and_versioned() {
        let manifest: DestinationManifest=serde_json::from_value(json!({"version":1,"entries":{
            "shared/destinations/main":{"destination":{"key":"main","session_type":"lobby/default","machine_profile":"local"},"overflow":"reject","empty_timeout_seconds":60}
        }})).unwrap();
        manifest.validate().unwrap();
        let mut duplicate = manifest.clone();
        duplicate.entries.insert("shared/destinations/other".into(), manifest.entries.values().next().unwrap().clone());
        assert!(duplicate.validate().is_err());
        let mut changed = manifest.clone();
        changed.version = 2;
        assert!(changed.validate().is_err());
        let app:AppArtifact=serde_json::from_value(json!({"id":"lobby","jar":"lobby.jar","sha256":"artifact","java_version":25,"sessions":{"default":{"machine_profile":"local","capacity":16}}})).unwrap();
        let apps = [("lobby".into(), app)].into();
        manifest.validate_apps(&apps).unwrap();
        let mut changed = manifest.clone();
        changed.entries.values_mut().next().unwrap().destination.machine_profile = "other".into();
        assert!(changed.validate_apps(&apps).is_err());
        let mut unsupported = serde_json::to_value(manifest).unwrap();
        unsupported["entries"]["shared/destinations/main"]["parameters"] = json!({"map":"other"});
        assert!(serde_json::from_value::<DestinationManifest>(unsupported).is_err());
    }
}
