//! Which class of client a request's credential belongs to.

use chunk_service::same_secret;
use std::sync::Arc;
use tonic::{Request, Status};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Class {
    Gateway,
    Jvm { host: String },
    Cli,
}

/// A request's credential and the class it grants.
pub(super) struct Principal {
    pub class: Class,
    pub credential: String,
}

pub(super) struct Credentials {
    /// The backend's platform credential, which gateways present.
    pub gateway: Option<String>,
    /// Control's credential, which the CLI presents.
    pub cli: String,
    pub control: Arc<chunk_control::Control>,
}

impl Credentials {
    /// # Errors
    /// Reports a missing or unknown credential as `UNAUTHENTICATED`.
    pub fn authenticate<T>(&self, request: &Request<T>) -> Result<Principal, Status> {
        let credential = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .filter(|credential| !credential.is_empty())
            .ok_or_else(|| Status::unauthenticated("missing credential"))?;
        let class = self.class(credential).ok_or_else(|| Status::unauthenticated("unknown credential"))?;
        Ok(Principal { class, credential: credential.to_owned() })
    }

    /// The class `credential` currently grants; a JVM's lapses once its process stops.
    pub fn class(&self, credential: &str) -> Option<Class> {
        let gateway = self.gateway.as_deref().is_some_and(|gateway| same_secret(credential, gateway));
        let cli = same_secret(credential, &self.cli);
        if gateway {
            Some(Class::Gateway)
        } else if cli {
            Some(Class::Cli)
        } else {
            self.control.authenticate(credential).map(|host| Class::Jvm { host })
        }
    }
}
