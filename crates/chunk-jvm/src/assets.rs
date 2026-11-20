//! The app's assets: the blobs of the revision core names that the app reads, fetched into the cache's store and
//! materialized as the directory the JVM reads at `CHUNK_ASSETS`.

use crate::{Failure, cache::Cache, fetch::Core};
use chunk_contract::AssetRevision;
use chunk_proto::sync::v1::JvmLaunch;
use futures_util::TryStreamExt as _;
use std::path::PathBuf;

/// How many blobs are downloaded at once.
const CONCURRENT_DOWNLOADS: usize = 4;

/// Fetches each blob of `launch`'s app that the cache lacks, checked against its size and digest, and returns the
/// app's materialized directory. Every other revision and blob leaves the cache, since this host runs only `launch`.
pub(crate) async fn prepare(core: &Core, boot: &str, launch: &JvmLaunch, cache: &Cache) -> Result<PathBuf, Failure> {
    let assets = launch.assets.as_ref().ok_or_else(|| Failure::verify("core named no asset revision"))?;
    let revision = AssetRevision::decode(&assets.manifest).map_err(Failure::verify)?;
    if revision.id() != assets.revision_id {
        return Err(Failure::verify("the asset revision core sent has another ID"));
    }
    let store = cache.assets();
    let unreadable = |error| Failure::io(format!("cannot store the asset blobs: {error}"));
    let mut missing = Vec::new();
    for (sha256, size) in revision.app_blobs(&launch.app) {
        if !store.contains(sha256).map_err(unreadable)? {
            missing.push(Ok((sha256, size)));
        }
    }
    futures_util::stream::iter(missing)
        .try_for_each_concurrent(CONCURRENT_DOWNLOADS, |(sha256, size)| async move {
            let mut staging = cache.blob_staging()?;
            core.download_blob(boot, sha256, size, staging.as_file_mut()).await?;
            let (store, sha256) = (store.clone(), sha256.to_owned());
            tokio::task::spawn_blocking(move || store.insert_file(&sha256, staging.path()))
                .await
                .map_err(Failure::io)?
                .map_err(unreadable)
        })
        .await?;
    let (store, app) = (store.clone(), launch.app.clone());
    tokio::task::spawn_blocking(move || {
        let directory = chunk_build::assets::materialize(&store, &revision, &app)
            .map_err(|error| Failure::io(format!("cannot materialize the app's assets: {error}")))?;
        if let Err(error) = store.write_revision(&revision).and_then(|_| store.retain(&[&revision])) {
            tracing::warn!(%error, "unused asset blobs not removed");
        }
        Ok(directory)
    })
    .await
    .map_err(Failure::io)?
}
