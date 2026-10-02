//! The asset revision a deployment pins: what its JVMs read, and the resource packs its players hold.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

use chunk_build::assets::Store;
use chunk_contract::{ASSET_REVISION_VERSION, AssetContract, AssetRevision, PackBlob, ResolvedPack};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// The asset revision a deployment pins, with the worlds and packs its release declares. The revision itself stays in
/// the asset store; only its packs are kept here, which players are sent.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentAssets {
    /// The SHA-256 of the revision's canonical JSON.
    pub revision_id: String,
    /// The revision's packs.
    pub packs: BTreeMap<String, PackBlob>,
    /// What the release declares, which the revision holds.
    pub contract: AssetContract,
    /// Where players' clients download packs: this prefix followed by a pack's SHA-256. Without one, players get no
    /// packs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_url_prefix: Option<String>,
}

impl DeploymentAssets {
    /// The assets of a deployment that pins `revision`. The caller has checked `revision` against `contract`.
    #[must_use]
    pub fn new(revision: &AssetRevision, contract: AssetContract, pack_url_prefix: Option<String>) -> Self {
        Self { revision_id: revision.id(), packs: revision.packs.clone(), contract, pack_url_prefix }
    }

    /// The pinned revision, read from `store`, which holds it unless it is empty.
    ///
    /// # Errors
    /// When `store` holds no valid revision with this ID.
    pub fn revision(&self, store: &Store) -> io::Result<AssetRevision> {
        let empty = empty();
        if self.revision_id == empty.id() {
            return Ok(empty);
        }
        store.read_revision(&self.revision_id)
    }

    /// The directory of `app`'s assets in the store at `root`, which its JVMs read.
    pub(crate) fn materialize(&self, root: &Path, app: &str) -> io::Result<PathBuf> {
        let store = Store::new(root);
        chunk_build::assets::materialize(&store, &self.revision(&store)?, app)
    }

    /// The packs `app`'s players hold, in the order their clients stack them.
    #[must_use]
    pub fn packs(&self, app: &str) -> Vec<ResolvedPack> {
        let Some(prefix) = self.pack_url_prefix.as_deref() else { return Vec::new() };
        let revision = AssetRevision { packs: self.packs.clone(), ..AssetRevision::default() };
        self.contract.packs_for(app, &revision, prefix)
    }

    /// Checks what is kept here: the revision's worlds are checked against the contract where the revision is loaded.
    pub(crate) fn validate(&self) -> Result<()> {
        let digest = |value: &str| value.len() == 64 && value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        let packs = AssetRevision { version: ASSET_REVISION_VERSION, packs: self.packs.clone(), ..Default::default() };
        if !digest(&self.revision_id) || packs.validate().is_err() {
            return Err(Error::Invalid("invalid asset revision"));
        }
        if self.contract.validate().is_err() || self.contract.packs.keys().any(|pack| !self.packs.contains_key(pack)) {
            return Err(Error::Invalid("the asset revision lacks what the release declares"));
        }
        if self.pack_url_prefix.as_ref().is_some_and(|prefix| prefix.is_empty() || prefix.len() > 2048) {
            return Err(Error::Invalid("invalid pack URL prefix"));
        }
        Ok(())
    }
}

/// The empty revision, for a release that declares no worlds or packs.
impl Default for DeploymentAssets {
    fn default() -> Self {
        Self::new(&empty(), AssetContract::default(), None)
    }
}

fn empty() -> AssetRevision {
    AssetRevision { version: ASSET_REVISION_VERSION, ..AssetRevision::default() }
}
