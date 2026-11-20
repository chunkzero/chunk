//! Loading the asset revision a deployment pins into core's asset store before the deployment activates, and removing
//! the revisions no deployment pins any more.

use super::{
    LoadDone,
    release::{self, Staged},
};
use chunk_build::assets::Store;
use chunk_contract::{ASSET_REVISION_VERSION, AssetContract, AssetRevision};
use chunk_management::{Client, v1};
use futures_util::TryStreamExt as _;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

/// How many blobs a load downloads at once.
const CONCURRENT_DOWNLOADS: usize = 4;

/// Checks the revision `artifact` names against the release's declarations in `contract`, then downloads into `store`
/// each blob of it that JVMs read and `store` lacks, verified, and the revision itself. Without an artifact, the
/// deployment pins the empty revision. Returns `None` once `cancel` stops it before its downloads finish. Blocking
/// inserts it started may outlive it, each holding a clone of `done`.
pub(super) async fn load(
    client: &Client,
    store: &Store,
    artifact: Option<&v1::AssetArtifact>,
    contract: &AssetContract,
    done: &Arc<LoadDone>,
    cancel: &CancellationToken,
) -> io::Result<Option<AssetRevision>> {
    let Some(artifact) = artifact else {
        let revision = AssetRevision { version: ASSET_REVISION_VERSION, ..AssetRevision::default() };
        contract.check(&revision).map_err(io::Error::other)?;
        store.write_revision(&revision)?;
        return Ok(Some(revision));
    };
    let revision = decode(artifact)?;
    contract.check(&revision).map_err(io::Error::other)?;
    download(client, store, artifact, revision, done, cancel).await
}

/// Downloads into `store` whatever of the revision `artifact` names it lost since core activated it, such as to a crash
/// before the store's writes were durable. Returns `None` once `cancel` stops it before its downloads finish. Blocking
/// inserts it started may outlive it, each holding a clone of `done`.
pub(super) async fn restore(
    client: &Client,
    store: &Store,
    artifact: Option<&v1::AssetArtifact>,
    done: &Arc<LoadDone>,
    cancel: &CancellationToken,
) -> io::Result<Option<AssetRevision>> {
    let Some(artifact) = artifact else {
        let revision = AssetRevision { version: ASSET_REVISION_VERSION, ..AssetRevision::default() };
        store.write_revision(&revision)?;
        return Ok(Some(revision));
    };
    download(client, store, artifact, decode(artifact)?, done, cancel).await
}

fn decode(artifact: &v1::AssetArtifact) -> io::Result<AssetRevision> {
    let revision = AssetRevision::decode(&artifact.manifest).map_err(io::Error::other)?;
    if revision.id() != artifact.revision_id {
        return Err(io::Error::other("the asset revision's manifest has another ID"));
    }
    Ok(revision)
}

async fn download(
    client: &Client,
    store: &Store,
    artifact: &v1::AssetArtifact,
    revision: AssetRevision,
    done: &Arc<LoadDone>,
    cancel: &CancellationToken,
) -> io::Result<Option<AssetRevision>> {
    fs::create_dir_all(store.root())?;
    let mut missing = Vec::new();
    for (sha256, size) in jvm_blobs(&revision) {
        if !store.contains(sha256)? {
            missing.push(Ok((sha256, size)));
        }
    }
    let fetched = futures_util::stream::iter(missing)
        .try_for_each_concurrent(CONCURRENT_DOWNLOADS, |(sha256, size)| {
            fetch_blob(client, store, &artifact.blob_url_prefix, sha256, size, done)
        });
    tokio::select! {
        fetched = fetched => fetched?,
        () = cancel.cancelled() => return Ok(None),
    }
    store.write_revision(&revision)?;
    Ok(Some(revision))
}

/// Downloads the blob into a hidden file beside the store, then inserts it, which verifies its SHA-256. The insert runs
/// to its end even when this future is dropped, holding `done` until then.
async fn fetch_blob(
    client: &Client,
    store: &Store,
    url_prefix: &str,
    sha256: &str,
    size: u64,
    done: &Arc<LoadDone>,
) -> io::Result<()> {
    let staged = Staged(store.root().join(format!(".{}.blob", uuid::Uuid::new_v4())));
    let url = format!("{url_prefix}{sha256}");
    release::fetch(|| client.download_blob(&url), size, "asset blob", &staged.0).await?;
    let (store, sha256, done) = (store.clone(), sha256.to_owned(), Arc::clone(done));
    tokio::task::spawn_blocking(move || {
        let _done = done;
        store.insert_file(&sha256, &staged.0)
    })
    .await
    .map_err(io::Error::other)?
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

/// Removes from `store` every revision but `kept`, and the blobs none of them holds, returning how many entries it
/// removed. Removes nothing and returns `None` while the store lacks one of `kept`, whose blobs it can't tell.
pub(super) async fn prune(store: &Store, kept: BTreeSet<String>) -> io::Result<Option<usize>> {
    let store = store.clone();
    tokio::task::spawn_blocking(move || {
        let mut revisions = Vec::new();
        for id in &kept {
            match store.read_revision(id) {
                Ok(revision) => revisions.push(revision),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        store.retain(&revisions.iter().collect::<Vec<_>>()).map(Some)
    })
    .await
    .map_err(io::Error::other)?
}
