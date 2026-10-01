//! The environment's secrets, as each desired state grants them.

use chunk_management::v1;

/// The name and version of every secret a desired state grants; it changes whenever one is set or deleted.
pub(crate) type Versions = Vec<(String, u64)>;

pub(crate) fn versions(desired: &v1::AttachResponse) -> Versions {
    let mut versions: Versions = desired.secrets.iter().map(|secret| (secret.name.clone(), secret.version)).collect();
    versions.sort_unstable();
    versions
}

/// The secrets `desired` grants, skipping any whose name or value is invalid with a warning that names it.
pub(crate) fn decode(desired: &v1::AttachResponse) -> chunk_backend::Secrets {
    let mut secrets = chunk_backend::Secrets::default();
    for secret in &desired.secrets {
        match std::str::from_utf8(&secret.value) {
            Ok(value) if chunk_contract::valid_env_name(&secret.name) && chunk_contract::valid_env_value(value) => {
                secrets.insert(secret.name.clone(), value.to_owned());
            }
            _ => tracing::warn!(name = %secret.name, "secret skipped: invalid name or value"),
        }
    }
    secrets
}
