//! What backend code reaches beyond the store: its environment's variables and secrets through `ctx.env`, and public
//! HTTP endpoints through `ctx.fetch`.
use std::{
    collections::BTreeMap,
    sync::{Arc, PoisonError, RwLock, Weak},
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

    /// Hides `text` if it holds any secret value raw, or escaped once or twice as `JSON.stringify` escapes strings,
    /// as a value logged inside serialized JSON is.
    pub(crate) fn redact(&self, text: String) -> String {
        let escaped = |value: &str| {
            let encoded = serde_json::to_string(value).expect("serializable secret");
            encoded[1..encoded.len() - 1].to_owned()
        };
        for secret in self.0.values() {
            let once = escaped(secret);
            if text.contains(secret.as_str()) || text.contains(&once) || text.contains(&escaped(&once)) {
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

/// The environment's current secrets, which [`crate::Backend::set_secrets`] replaces, and the earlier sets invocations
/// still hold.
#[derive(Clone, Default)]
pub(crate) struct SecretSlot(Arc<RwLock<Held>>);

#[derive(Default)]
struct Held {
    current: Arc<Secrets>,
    earlier: Vec<Weak<Secrets>>,
}

impl SecretSlot {
    /// The secrets an invocation starting now keeps until it ends.
    pub fn snapshot(&self) -> Arc<Secrets> {
        self.0.read().unwrap_or_else(PoisonError::into_inner).current.clone()
    }

    pub fn set(&self, secrets: Secrets) {
        let mut held = self.0.write().unwrap_or_else(PoisonError::into_inner);
        let earlier = std::mem::replace(&mut held.current, Arc::new(secrets));
        held.earlier.retain(|secrets| secrets.strong_count() > 0);
        held.earlier.push(Arc::downgrade(&earlier));
    }

    /// Hides `text` if it holds a value of any secret an invocation may still read, so the queries and mutations an
    /// action runs can't log what it passes them.
    pub fn redact(&self, text: String) -> String {
        let held = self.0.read().unwrap_or_else(PoisonError::into_inner);
        let earlier = held.earlier.iter().filter_map(Weak::upgrade);
        std::iter::once(held.current.clone()).chain(earlier).fold(text, |text, secrets| secrets.redact(text))
    }
}

pub struct ActionEffects {
    environment: String,
    /// Selects `[env.<name>.vars]`; unset, deployments read their top-level variables only.
    vars: Option<String>,
    pub(crate) secrets: SecretSlot,
    pub(crate) fetcher: Arc<Fetcher>,
    pub(crate) moves: crate::moves::Slot,
}

impl ActionEffects {
    /// Effects bound to exactly one environment, with no secrets until [`Self::with_secrets`] or
    /// [`crate::Backend::set_secrets`].
    /// # Errors
    /// Rejects invalid environment identities, and reports an HTTP client that can't be built.
    pub fn new(environment: String) -> Result<Self> {
        if environment.is_empty() || environment.len() > 128 {
            return Err(Error::Invalid("effect environment"));
        }
        Ok(Self {
            environment,
            vars: None,
            secrets: SecretSlot::default(),
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

    /// Actions read `secrets` from the start, so none, a restored job included, runs before they are installed.
    #[must_use]
    pub fn with_secrets(self, secrets: Secrets) -> Self {
        self.secrets.set(secrets);
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
        self.secrets.snapshot()
    }

    /// `deployment`'s variables in this environment, as the JSON object `ctx.env` reads. Actions read secrets over
    /// them on demand.
    pub(crate) fn env(&self, deployment: &Deployment) -> Json {
        serde_json::to_value(deployment.contracts.env.resolve(self.vars.as_deref())).expect("string map").into()
    }
}
