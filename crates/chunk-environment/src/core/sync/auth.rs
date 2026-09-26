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
        let gateway = self.gateway.as_deref().is_some_and(|gateway| same_secret(credential, gateway));
        let cli = same_secret(credential, &self.cli);
        let class = if gateway {
            Class::Gateway
        } else if cli {
            Class::Cli
        } else if let Some(host) = self.control.authenticate(credential) {
            Class::Jvm { host }
        } else {
            return Err(Status::unauthenticated("unknown credential"));
        };
        Ok(Principal { class, credential: credential.to_owned() })
    }
}
