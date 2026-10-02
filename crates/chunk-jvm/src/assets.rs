//! The app's assets: the blobs of the revision core names that the app reads, fetched into the cache's store and
//! materialized as the directory the JVM reads at `CHUNK_ASSETS`.

use crate::{Failure, cache::Cache, fetch::Core};
use chunk_contract::AssetRevision;
use chunk_proto::sync::v1::JvmLaunch;
use std::path::PathBuf;

/// Fetches each blob of `launch`'s app that the cache lacks, checked against its size and digest, and returns the
/// app's materialized directory.
pub(crate) async fn prepare(core: &Core, boot: &str, launch: &JvmLaunch, cache: &Cache) -> Result<PathBuf, Failure> {
    let assets = launch.assets.as_ref().ok_or_else(|| Failure::verify("core named no asset revision"))?;
    let revision = AssetRevision::decode(&assets.manifest).map_err(Failure::verify)?;
    if revision.id() != assets.revision_id {
        return Err(Failure::verify("the asset revision core sent has another ID"));
    }
    let store = cache.assets();
    let unreadable = |error| Failure::io(format!("cannot store the asset blobs: {error}"));
    for (sha256, size) in revision.app_blobs(&launch.app) {
        if store.contains(sha256).map_err(unreadable)? {
            continue;
        }
        let mut staging = cache.blob_staging()?;
        core.download_blob(boot, sha256, size, staging.as_file_mut()).await?;
        store.insert_file(sha256, staging.path()).map_err(unreadable)?;
    }
    let (store, app) = (store.clone(), launch.app.clone());
    tokio::task::spawn_blocking(move || chunk_build::assets::materialize(&store, &revision, &app))
        .await
        .map_err(Failure::io)?
        .map_err(|error| Failure::io(format!("cannot materialize the app's assets: {error}")))
}
