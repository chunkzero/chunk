//! Which class of client a request's credential belongs to.

use chunk_service::same_secret;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt::Write,
    sync::{Arc, PoisonError, RwLock},
};
use tonic::{Request, Status};

/// `Unadopted` is a JVM launched before core restarted, which may only call `chunk:register` until it re-attaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Class {
    Gateway { id: String },
    Jvm { host: String },
    Unadopted { host: String },
    Cli,
}

/// A request's credential and the class it grants.
pub(super) struct Principal {
    pub class: Class,
    pub credential: String,
}

pub(super) struct Credentials {
    pub gateways: Arc<Gateways>,
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
        let gateway = self.gateways.gateway(credential);
        let cli = same_secret(credential, &self.cli);
        if let Some(id) = gateway {
            Some(Class::Gateway { id })
        } else if cli {
            Some(Class::Cli)
        } else if let Some(host) = self.control.authenticate(credential) {
            Some(Class::Jvm { host })
        } else {
            self.control.unadopted(credential).map(|host| Class::Unadopted { host })
        }
    }
}

/// The credential core minted for each gateway, by gateway ID. Only digests are kept.
#[derive(Default)]
pub(crate) struct Gateways(RwLock<BTreeMap<String, String>>);

impl Gateways {
    /// Mints `id`'s credential, which replaces any earlier one and is the gateway's authority for its topic and
    /// callers.
    pub fn mint(&self, id: &str) -> String {
        let credential = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let mut gateways = self.0.write().unwrap_or_else(PoisonError::into_inner);
        gateways.insert(id.to_owned(), digest(&credential));
        credential
    }

    /// The gateway `credential` was minted for, comparing it against every gateway's in constant time.
    fn gateway(&self, credential: &str) -> Option<String> {
        let presented = digest(credential);
        let gateways = self.0.read().unwrap_or_else(PoisonError::into_inner);
        let matches = gateways.iter().filter(|(_, expected)| same_secret(&presented, expected));
        matches.fold(None, |_, (id, _)| Some(id.clone()))
    }
}

fn digest(credential: &str) -> String {
    Sha256::digest(credential).iter().fold(String::with_capacity(64), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}
