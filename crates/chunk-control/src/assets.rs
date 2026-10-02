//! The asset revision a deployment pins: what its JVMs read, and the resource packs its players hold.

use chunk_contract::{ASSET_REVISION_VERSION, AssetContract, AssetRevision, ResolvedPack};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// The asset revision a deployment pins, with the worlds and packs its release declares.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentAssets {
    /// The SHA-256 of the revision's canonical JSON.
    pub revision_id: String,
    pub revision: AssetRevision,
    /// What the release declares, which the revision holds.
    pub contract: AssetContract,
    /// Where players' clients download packs: this prefix followed by a pack's SHA-256. Without one, players get no
    /// packs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_url_prefix: Option<String>,
}

impl DeploymentAssets {
    #[must_use]
    pub fn new(revision: AssetRevision, contract: AssetContract, pack_url_prefix: Option<String>) -> Self {
        Self { revision_id: revision.id(), revision, contract, pack_url_prefix }
    }

    /// The packs `app`'s players hold, in the order their clients stack them.
    #[must_use]
    pub fn packs(&self, app: &str) -> Vec<ResolvedPack> {
        let prefix = self.pack_url_prefix.as_deref();
        prefix.map(|prefix| self.contract.packs_for(app, &self.revision, prefix)).unwrap_or_default()
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.revision.validate().is_err() || self.revision.id() != self.revision_id {
            return Err(Error::Invalid("invalid asset revision"));
        }
        if self.contract.validate().is_err() || self.contract.check(&self.revision).is_err() {
            return Err(Error::Invalid("the asset revision lacks what the release declares"));
        }
        if self.pack_url_prefix.as_ref().is_some_and(|prefix| prefix.is_empty() || prefix.len() > 2048) {
            return Err(Error::Invalid("invalid pack URL prefix"));
        }
        Ok(())
    }
}

/// An empty revision, for a release that declares no worlds or packs.
impl Default for DeploymentAssets {
    fn default() -> Self {
        let revision = AssetRevision { version: ASSET_REVISION_VERSION, ..AssetRevision::default() };
        Self::new(revision, AssetContract::default(), None)
    }
}
