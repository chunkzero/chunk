//! Which class of client a request's credential belongs to.

use chunk_control::MachineKind;
use chunk_service::same_secret;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt::Write,
    net::SocketAddr,
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

/// A request's credential, the peer it came from, and the class they grant.
pub(super) struct Principal {
    pub class: Class,
    pub credential: String,
    /// Unknown only when the transport doesn't report it.
    pub peer: Option<SocketAddr>,
}

pub(super) struct Credentials {
    /// The in-process gateway's credential.
    pub gateways: Arc<Gateways>,
    /// Control's credential, which the CLI presents from a loopback peer.
    pub cli: String,
    pub issuer: Issuer,
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
        let peer = request.remote_addr();
        let class = self.class(credential, peer).ok_or_else(|| Status::unauthenticated("unknown credential"))?;
        Ok(Principal { class, credential: credential.to_owned(), peer })
    }

    /// Whether `principal`'s credential still grants its class.
    pub fn holds(&self, principal: &Principal) -> bool {
        self.class(&principal.credential, principal.peer).as_ref() == Some(&principal.class)
    }

    /// The class `credential` currently grants from `peer`. Control's credential holds only from a loopback peer, a
    /// gateway machine's until it's revoked, and a JVM's until its process stops.
    fn class(&self, credential: &str, peer: Option<SocketAddr>) -> Option<Class> {
        let gateway = self.gateways.gateway(credential);
        let loopback = peer.is_some_and(|peer| peer.ip().to_canonical().is_loopback());
        let cli = (same_secret(credential, &self.cli) && loopback) | same_secret(credential, self.issuer.operator());
        if let Some(id) = gateway {
            Some(Class::Gateway { id })
        } else if cli {
            Some(Class::Cli)
        } else if let Some((MachineKind::Gateway, id)) = self.issuer.verify(credential) {
            self.control.machine(id, MachineKind::Gateway).then(|| Class::Gateway { id: id.to_owned() })
        } else if let Some(host) = self.control.authenticate(credential) {
            Some(Class::Jvm { host })
        } else {
            self.control.unadopted(credential).map(|host| Class::Unadopted { host })
        }
    }
}

/// The credential core minted for each gateway serving in its process, by gateway ID. Only digests are kept.
#[derive(Default)]
pub(crate) struct Gateways(RwLock<BTreeMap<String, String>>);

impl Gateways {
    /// Mints `id`'s credential, which replaces any earlier one and is the gateway's authority for its topic and
    /// callers.
    pub fn mint(&self, id: &str) -> String {
        let credential = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let mut gateways = self.0.write().unwrap_or_else(PoisonError::into_inner);
        gateways.insert(id.to_owned(), hex(&Sha256::digest(&credential)));
        credential
    }

    /// The gateway `credential` was minted for, comparing it against every gateway's in constant time.
    fn gateway(&self, credential: &str) -> Option<String> {
        let presented = hex(&Sha256::digest(credential));
        let gateways = self.0.read().unwrap_or_else(PoisonError::into_inner);
        let matches = gateways.iter().filter(|(_, expected)| same_secret(&presented, expected));
        matches.fold(None, |_, (id, _)| Some(id.clone()))
    }
}

/// Derives an environment's machine and operator credentials. Each is `<scope>/<MAC>`: its scope,
/// `machine/v1/<environment>/<kind>/<id>` or `operator/v1/<environment>`, then the lowercase hex HMAC-SHA256 of the
/// scope. So a retry mints the same credential, and whether a machine's holds is decided by control's rows.
#[derive(Clone)]
pub(crate) struct Issuer {
    environment: String,
    key: Vec<u8>,
    operator: String,
}

impl Issuer {
    /// Keys credentials with SHA-256 of the environment's token when management issued one, and otherwise with
    /// control's credential.
    pub fn new(environment: &str, environment_token: Option<&str>, control: &str) -> Self {
        let key = environment_token.map_or_else(|| control.as_bytes().to_vec(), |token| Sha256::digest(token).to_vec());
        let operator = sign(&key, format!("operator/v1/{environment}"));
        Self { environment: environment.to_owned(), key, operator }
    }

    /// Machine `id`'s credential as a machine of `kind`.
    pub fn machine(&self, kind: MachineKind, id: &str) -> String {
        sign(&self.key, format!("machine/v1/{}/{}/{id}", self.environment, kind.name()))
    }

    /// The operator credential management presents when it relays operator calls.
    pub fn operator(&self) -> &str {
        &self.operator
    }

    /// The machine `credential` names, if this issuer minted it.
    fn verify<'a>(&self, credential: &'a str) -> Option<(MachineKind, &'a str)> {
        let (scope, _) = credential.rsplit_once('/')?;
        let mut parts = scope.strip_prefix("machine/v1/")?.rsplitn(3, '/');
        let (id, kind, environment) = (parts.next()?, parts.next()?, parts.next()?);
        let kind = [MachineKind::Gateway, MachineKind::Jvm].into_iter().find(|known| known.name() == kind)?;
        let minted = environment == self.environment && same_secret(credential, &self.machine(kind, id));
        minted.then_some((kind, id))
    }
}

/// `scope` followed by its MAC under `key`.
fn sign(key: &[u8], scope: String) -> String {
    let mac = hex(&hmac(key, scope.as_bytes()));
    scope + "/" + &mac
}

/// HMAC-SHA256 of `message` under `key`.
pub(super) fn hmac(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0; 64];
    if key.len() > block.len() {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let inner = Sha256::new().chain_update(block.map(|byte| byte ^ 0x36)).chain_update(message).finalize();
    Sha256::new().chain_update(block.map(|byte| byte ^ 0x5c)).chain_update(inner).finalize().into()
}

pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc_4231() {
        let short = hmac(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(hex(&short), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        let long = hmac(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First");
        assert_eq!(hex(&long), "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54");
    }

    #[test]
    fn machine_credentials_name_their_machine_and_verify_only_under_their_key() {
        let issuer = Issuer::new("env/one", Some("environment-token"), "control");
        let credential = issuer.machine(MachineKind::Gateway, "gateway-1");
        assert!(credential.starts_with("machine/v1/env/one/gateway/gateway-1/"));
        assert_eq!(issuer.machine(MachineKind::Gateway, "gateway-1"), credential);
        assert_eq!(issuer.verify(&credential), Some((MachineKind::Gateway, "gateway-1")));

        let tampered = credential.replace("/gateway-1/", "/gateway-2/");
        assert_eq!(issuer.verify(&tampered), None);
        assert_eq!(Issuer::new("env/one", None, "control").verify(&credential), None);
        assert_eq!(Issuer::new("env/two", Some("environment-token"), "control").verify(&credential), None);
        assert_eq!(issuer.verify(issuer.operator()), None);
        // Management keys by the token's digest, so it derives credentials without holding the token.
        assert_eq!(
            issuer.operator(),
            format!("operator/v1/env/one/{}", hex(&hmac(&Sha256::digest("environment-token"), b"operator/v1/env/one")))
        );
    }
}
