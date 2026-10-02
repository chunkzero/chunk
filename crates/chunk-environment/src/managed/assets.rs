//! Loading the asset revision a deployment pins into core's asset store, blob by blob, before the deployment activates.

use super::release::{self, Staged};
use chunk_build::assets::Store;
use chunk_contract::{ASSET_REVISION_VERSION, AssetContract, AssetRevision};
use chunk_management::{Client, v1};
use std::{collections::BTreeMap, fs, io};
use tokio_util::sync::CancellationToken;

/// Checks the revision `artifact` names against the release's declarations in `contract`, then downloads into `store`
/// each blob of it that JVMs read and `store` lacks, verified. Without an artifact, the deployment pins the empty
/// revision. Returns `None` once `cancel` stops it before its downloads finish.
pub(super) async fn load(
    client: &Client,
    store: &Store,
    artifact: Option<&v1::AssetArtifact>,
    contract: &AssetContract,
    cancel: &CancellationToken,
) -> io::Result<Option<AssetRevision>> {
    let Some(artifact) = artifact else {
        let revision = AssetRevision { version: ASSET_REVISION_VERSION, ..AssetRevision::default() };
        contract.check(&revision).map_err(io::Error::other)?;
        return Ok(Some(revision));
    };
    let revision = AssetRevision::decode(&artifact.manifest).map_err(io::Error::other)?;
    if revision.id() != artifact.revision_id {
        return Err(io::Error::other("the asset revision's manifest has another ID"));
    }
    contract.check(&revision).map_err(io::Error::other)?;
    fs::create_dir_all(store.root())?;
    for (sha256, size) in jvm_blobs(&revision) {
        if store.contains(sha256)? {
            continue;
        }
        let staged = Staged(store.root().join(format!(".{}.blob", uuid::Uuid::new_v4())));
        let url = format!("{}{sha256}", artifact.blob_url_prefix);
        tokio::select! {
            fetched = release::fetch(|| client.download_blob(&url), size, "asset blob", &staged.0) => fetched?,
            () = cancel.cancelled() => return Ok(None),
        }
        let (store, sha256) = (store.clone(), sha256.to_owned());
        tokio::task::spawn_blocking(move || store.insert_file(&sha256, &staged.0))
            .await
            .map_err(io::Error::other)??;
    }
    Ok(Some(revision))
}

/// Every blob some app's JVMs read: each app's worlds and files, and the shared files. Players' clients download packs
/// from management, never through core.
fn jvm_blobs(revision: &AssetRevision) -> BTreeMap<&str, u64> {
    let apps = revision.apps.values().flat_map(|assets| assets.worlds.values().chain(assets.files.values()));
    revision.shared.values().chain(apps).map(|blob| (blob.sha256.as_str(), blob.size)).collect()
}

/// Removes the blob downloads a previous run left unfinished.
pub(super) async fn sweep(store: &Store) -> io::Result<()> {
    let unfinished = match fs::read_dir(store.root()) {
        Ok(entries) => entries.map(|entry| entry.map(|entry| entry.path())).collect::<io::Result<Vec<_>>>()?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let hidden =
        |path: &std::path::PathBuf| path.file_name().is_some_and(|name| name.as_encoded_bytes().starts_with(b"."));
    release::remove(unfinished.into_iter().filter(hidden).collect()).await
}
