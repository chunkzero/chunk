use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionDeclaration {
    pub machine_profile: String,
    pub capacity: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AppArtifact {
    pub id: String,
    pub jar: String,
    pub sha256: String,
    pub java_version: u32,
    pub sessions: BTreeMap<String, SessionDeclaration>,
}

impl AppArtifact {
    /// # Errors
    /// Rejects malformed app identities and session deployment requirements.
    pub fn validate(&self) -> Result<(), String> {
        if !name(&self.id) || self.sessions.is_empty() || self.sessions.len() > 128 {
            return Err("invalid app manifest".into());
        }
        for (id, session) in &self.sessions {
            if !name(id)
                || !(1..=128).contains(&session.capacity)
                || session.machine_profile.is_empty()
                || session.machine_profile.len() > 128
                || !session.machine_profile.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            {
                return Err(format!("invalid session declaration {id}"));
            }
        }
        Ok(())
    }
}

fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[must_use]
pub fn class_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && value.split('.').all(|part| {
            !part.is_empty()
                && part.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$')
                && part.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$')
        })
}
