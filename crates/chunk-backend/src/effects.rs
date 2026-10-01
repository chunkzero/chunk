//! What backend code reaches beyond the store: its environment's variables and secrets through `ctx.env`, and public
//! HTTP endpoints through `ctx.fetch`.
use std::{
    collections::BTreeMap,
    sync::{Arc, PoisonError, RwLock},
};

use chunk_contract::Deployment;
use chunk_js::Json;

use crate::{Error, Result};

mod http;
pub(crate) use http::{Fetcher, ScopedEffects};

/// An environment's secret values by name. Neither `Debug` nor serializable, so values stay out of logs.
#[derive(Clone, Default)]
pub struct Secrets(BTreeMap<String, String>);

impl Secrets {
    pub fn insert(&mut self, name: String, value: String) {
        self.0.insert(name, value);
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// Hides `text` if it holds any secret value, raw or JSON-escaped.
    pub(crate) fn redact(&self, text: String) -> String {
        for secret in self.0.values() {
            let encoded = serde_json::to_string(secret).expect("serializable secret");
            if text.contains(secret.as_str()) || text.contains(&encoded[1..encoded.len() - 1]) {
                return "[redacted action diagnostic]".into();
            }
        }
        text
    }
}

impl FromIterator<(String, String)> for Secrets {
    fn from_iter<T: IntoIterator<Item = (String, String)>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// The environment's current secrets, which [`crate::Backend::set_secrets`] replaces.
pub(crate) type SecretSlot = Arc<RwLock<Arc<Secrets>>>;

pub struct ActionEffects {
    environment: String,
    /// Selects `[env.<name>.vars]`; unset, deployments read their top-level variables only.
    vars: Option<String>,
    pub(crate) secrets: SecretSlot,
    pub(crate) fetcher: Arc<Fetcher>,
    pub(crate) moves: crate::moves::Slot,
}

impl ActionEffects {
    /// Effects bound to exactly one environment, with no secrets until [`crate::Backend::set_secrets`].
    /// # Errors
    /// Rejects invalid environment identities, and reports an HTTP client that can't be built.
    pub fn new(environment: String) -> Result<Self> {
        if environment.is_empty() || environment.len() > 128 {
            return Err(Error::Invalid("effect environment"));
        }
        Ok(Self {
            environment,
            vars: None,
            secrets: Arc::default(),
            fetcher: Arc::new(Fetcher::new(chunk_service::net::public)?),
            moves: Arc::default(),
        })
    }

    /// Deployments read the variables `[env.<name>.vars]` overrides for the environment named `name`.
    #[must_use]
    pub fn with_vars(mut self, name: Option<String>) -> Self {
        self.vars = name;
        self
    }

    /// Fetches only reach addresses `policy` admits, such as a test's loopback server.
    #[cfg(test)]
    pub(crate) fn with_policy(mut self, policy: fn(std::net::IpAddr) -> bool) -> Self {
        self.fetcher = Arc::new(Fetcher::new(policy).expect("HTTP client"));
        self
    }

    pub(crate) fn validate_environment(&self, environment: &str) -> Result<()> {
        if self.environment != environment {
            return Err(Error::Invalid("effect environment mismatch"));
        }
        Ok(())
    }

    /// The secrets an invocation starting now keeps until it ends.
    pub(crate) fn secrets(&self) -> Arc<Secrets> {
        self.secrets.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// `deployment`'s variables in this environment, as the JSON object `ctx.env` reads. Actions read secrets over
    /// them on demand.
    pub(crate) fn env(&self, deployment: &Deployment) -> Json {
        serde_json::to_value(deployment.contracts.env.resolve(self.vars.as_deref())).expect("string map").into()
    }
}
