//! Host-supplied action grants. Values are deliberately neither serializable nor Debug.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use chunk_js::{DeploymentId, HttpMethod};
use reqwest::{Client, Url};

use crate::{Error, Result};

mod http;
pub(crate) use http::ScopedEffects;

pub struct HttpBinding {
    pub(crate) base: Url,
    pub(crate) methods: BTreeSet<HttpMethod>,
    pub(crate) timeout: Duration,
    pub(crate) client: Client,
}

impl HttpBinding {
    /// Grants relative paths below a fixed HTTP(S) base ending in `/`.
    /// # Errors
    /// Rejects userinfo, query/fragment, invalid origins, and empty method grants.
    pub fn new(base: &str, methods: impl IntoIterator<Item = HttpMethod>) -> Result<Self> {
        if base.len() > 2048 {
            return Err(Error::Invalid("HTTP binding origin length"));
        }
        let base = Url::parse(base).map_err(|_| Error::Invalid("HTTP binding origin"))?;
        let methods: BTreeSet<_> = methods.into_iter().collect();
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || !base.path().ends_with('/')
            || methods.is_empty()
        {
            return Err(Error::Invalid("HTTP binding origin or method grant"));
        }
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .referer(false)
            .http1_only()
            .pool_max_idle_per_host(0)
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .build()
            .map_err(|_| Error::Invalid("HTTP client unavailable"))?;
        Ok(Self { base, methods, timeout: Duration::from_secs(10), client })
    }

    /// # Errors
    /// Rejects zero or more than ten seconds. The action deadline also applies.
    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self> {
        if timeout.is_zero() || timeout > Duration::from_secs(10) {
            return Err(Error::Invalid("HTTP timeout"));
        }
        self.timeout = timeout;
        Ok(self)
    }
}

#[derive(Default)]
pub struct ActionGrants {
    pub(crate) http: BTreeMap<String, HttpBinding>,
    secrets: BTreeMap<String, String>,
}

fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}

impl ActionGrants {
    /// # Errors
    /// Rejects duplicate/invalid names and more than 16 bindings.
    pub fn with_http(mut self, name: String, binding: HttpBinding) -> Result<Self> {
        if !self::name(&name) || self.http.len() >= 16 || self.http.contains_key(&name) {
            return Err(Error::Invalid("HTTP binding name or capacity"));
        }
        self.http.insert(name, binding);
        Ok(self)
    }

    /// Install a value supplied by the host, such as an environment variable read
    /// by the embedding application. The backend never serializes these grants.
    /// # Errors
    /// Rejects duplicate/invalid names, empty or >8KiB values, and more than 16 secrets.
    pub fn with_secret(mut self, name: String, value: String) -> Result<Self> {
        if !self::name(&name)
            || self.secrets.len() >= 16
            || self.secrets.contains_key(&name)
            || value.is_empty()
            || value.len() > 8 * 1024
        {
            return Err(Error::Invalid("secret binding name or capacity"));
        }
        self.secrets.insert(name, value);
        Ok(self)
    }

    pub(crate) fn secret(&self, name: &str) -> std::result::Result<String, String> {
        self.secrets.get(name).cloned().ok_or_else(|| "Secret capability denied".into())
    }

    pub(crate) fn redact(&self, text: String) -> String {
        for secret in self.secrets.values() {
            let encoded = serde_json::to_string(secret).expect("serializable secret");
            if text.contains(secret) || text.contains(&encoded[1..encoded.len() - 1]) {
                return "[redacted action diagnostic]".into();
            }
        }
        text
    }
}

pub struct ActionEffects {
    environment: String,
    grants: BTreeMap<DeploymentId, Arc<ActionGrants>>,
}

impl ActionEffects {
    /// Creates deny-by-default grants bound to exactly one environment.
    /// # Errors
    /// Rejects invalid environment identities.
    pub fn new(environment: String) -> Result<Self> {
        if environment.is_empty() || environment.len() > 128 {
            return Err(Error::Invalid("effect environment"));
        }
        Ok(Self { environment, grants: BTreeMap::new() })
    }

    /// # Errors
    /// Rejects duplicate deployment bindings and more than 16 deployments.
    pub fn with_deployment(mut self, id: DeploymentId, grants: ActionGrants) -> Result<Self> {
        if self.grants.len() >= 16 || self.grants.contains_key(&id) {
            return Err(Error::Invalid("effect deployment grant"));
        }
        self.grants.insert(id, Arc::new(grants));
        Ok(self)
    }

    pub(crate) fn validate_environment(&self, environment: &str) -> Result<()> {
        if self.environment != environment {
            return Err(Error::Invalid("effect environment mismatch"));
        }
        Ok(())
    }

    pub(crate) fn grants(&self, id: &DeploymentId) -> Arc<ActionGrants> {
        self.grants.get(id).cloned().unwrap_or_default()
    }
}
