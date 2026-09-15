use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{Command, deployment::identifier, visible_commands};

pub const DOMAIN_MANIFEST_VERSION: u32 = 1;

/// Static scopes and named handlers pinned to one backend deployment. The empty
/// path is the root scope; app bindings omitted with the manifest default to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainManifest {
    pub version: u32,
    pub scopes: BTreeMap<String, DomainScope>,
    pub apps: BTreeMap<String, String>,
    pub hooks: BTreeMap<String, Hook>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub commands: BTreeMap<String, Command>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainScope {
    pub parent: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum HookEvent {
    #[serde(rename = "server.ping")]
    ServerPing,
    #[serde(rename = "player.login")]
    PlayerLogin,
    #[serde(rename = "player.route")]
    PlayerRoute,
    #[serde(rename = "player.beforeMove")]
    PlayerBeforeMove,
    #[serde(rename = "player.connect")]
    PlayerConnect,
    #[serde(rename = "player.disconnect")]
    PlayerDisconnect,
    #[serde(rename = "domain.enter")]
    DomainEnter,
    #[serde(rename = "domain.leave")]
    DomainLeave,
}

impl HookEvent {
    #[must_use]
    pub fn admission(self) -> bool {
        matches!(self, Self::PlayerLogin | Self::PlayerBeforeMove)
    }

    #[must_use]
    pub fn single_result(self) -> bool {
        matches!(self, Self::ServerPing | Self::PlayerRoute)
    }

    #[must_use]
    pub fn can_follow_player(self) -> bool {
        matches!(self, Self::PlayerConnect | Self::DomainEnter | Self::DomainLeave)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub domain: String,
    pub event: HookEvent,
    pub export: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub follow_player: bool,
}

impl DomainManifest {
    /// # Errors
    /// Rejects invalid static ancestry, unknown app domains and ambiguous handlers.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.version != DOMAIN_MANIFEST_VERSION {
            return Err("unsupported domain manifest version");
        }
        if self.scopes.len() > 256 || self.apps.len() > 256 || self.hooks.len() > 256 || self.commands.len() > 256 {
            return Err("domain manifest size limit");
        }
        if self.scopes.get("") != Some(&DomainScope { parent: None }) {
            return Err("domain manifest requires a root scope");
        }
        let mut paths = BTreeSet::new();
        for (path, scope) in &self.scopes {
            if !domain_path(path) || !paths.insert(path.to_ascii_lowercase()) {
                return Err("invalid or case-colliding domain path");
            }
            if !path.is_empty() {
                let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
                if scope.parent.as_deref() != Some(parent) || !self.scopes.contains_key(parent) {
                    return Err("invalid domain ancestry");
                }
            }
        }
        let mut apps = BTreeSet::new();
        for (app, domain) in &self.apps {
            if !identifier(app) || !apps.insert(app.to_ascii_lowercase()) || !self.scopes.contains_key(domain) {
                return Err("invalid app domain binding");
            }
        }
        self.validate_handlers()
    }

    fn handler_identity(&self, identity: &str, domain: &str, kind: &str) -> bool {
        let suffix = if domain.is_empty() { format!("{kind}/") } else { format!("{domain}/{kind}/") };
        if ["shared/domains/", "scopes/"]
            .iter()
            .any(|prefix| identity.strip_prefix(&format!("{prefix}{suffix}")).is_some_and(identifier))
        {
            return true;
        }
        let Some((app, path)) = identity.strip_prefix("apps/").and_then(|path| path.split_once('/')) else {
            return false;
        };
        self.apps.get(app).is_some_and(|owner| owner == domain)
            && path.strip_prefix(&format!("app/{kind}/")).is_some_and(identifier)
    }

    fn validate_handlers(&self) -> Result<(), &'static str> {
        let mut identities = BTreeSet::new();
        let mut exports = BTreeSet::new();
        let mut groups: BTreeMap<_, Vec<&Hook>> = BTreeMap::new();
        for (identity, hook) in &self.hooks {
            if !self.handler_identity(identity, &hook.domain, "hooks")
                || !identities.insert(identity.to_ascii_lowercase())
                || !identifier(&hook.export)
                || !exports.insert(&hook.export)
                || !self.scopes.contains_key(&hook.domain)
            {
                return Err("invalid hook identity, export or domain");
            }
            if hook.event.single_result() && !hook.domain.is_empty() {
                return Err("ping and routing responders require the root domain");
            }
            if hook.order.is_some() && !hook.event.admission() {
                return Err("only admission hooks accept ordering");
            }
            if hook.follow_player && !hook.event.can_follow_player() {
                return Err("hook event cannot follow a player");
            }
            groups.entry((&hook.domain, hook.event)).or_default().push(hook);
        }
        for ((_, event), hooks) in groups {
            if event.single_result() && hooks.len() > 1 {
                return Err("ambiguous single-result hook responders");
            }
            if event.admission() && hooks.len() > 1 {
                let orders: BTreeSet<_> = hooks.iter().filter_map(|hook| hook.order).collect();
                if orders.len() != hooks.len() {
                    return Err("same-scope admission hooks require distinct explicit order values");
                }
            }
        }
        for (identity, command) in &self.commands {
            if !self.handler_identity(identity, &command.domain, "commands")
                || !identities.insert(identity.to_ascii_lowercase())
                || !identifier(&command.export)
                || !exports.insert(&command.export)
                || !self.scopes.contains_key(&command.domain)
            {
                return Err("invalid command identity, export or domain");
            }
        }
        for domain in self.scopes.keys() {
            visible_commands(&self.commands, domain, &[])?;
        }
        Ok(())
    }
}

#[must_use]
pub fn domain_path(value: &str) -> bool {
    value.is_empty() || (value.len() <= 256 && value.split('/').count() <= 32 && value.split('/').all(identifier))
}

#[cfg(test)]
mod tests;
