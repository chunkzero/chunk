//! Loading the asset revision a deployment pins before it activates.

use super::*;
use crate::managed::assets;
use chunk_build::assets::Store;
use chunk_contract::{AssetBlob, AssetContract, AssetRevision};
use chunk_management::v1::AssetArtifact;

#[tokio::test]
async fn a_revisions_missing_blobs_download_unless_it_lacks_a_declared_world() {
    let harness = Harness::new().await;
    let blob = |bytes: &[u8]| AssetBlob { sha256: format!("{:x}", Sha256::digest(bytes)), size: bytes.len() as u64 };
    let (held, missing): (&[u8], &[u8]) = (b"held", b"Polr koth");
    let mut revision = AssetRevision { version: chunk_contract::ASSET_REVISION_VERSION, ..AssetRevision::default() };
    revision.shared.insert("held.txt".into(), blob(held));
    revision.apps.entry("lobby".into()).or_default().worlds.insert("koth".into(), blob(missing));
    // Management serves only the missing blob, so fetching the held one would fail.
    let path = format!("/blobs/{}", blob(missing).sha256);
    harness.management.archives.lock().unwrap().insert(path, missing.to_vec());
    let store = Store::new(harness.state().join("assets"));
    store.insert(&blob(held).sha256, held).unwrap();
    let blob_url_prefix = format!("{}/blobs/", harness.url);
    let artifact = AssetArtifact { revision_id: revision.id(), manifest: revision.encode(), blob_url_prefix };
    let client = harness.management_config().client();
    let declared = |world: &str| AssetContract {
        worlds: BTreeMap::from([("lobby".into(), BTreeSet::from([world.into()]))]),
        ..AssetContract::default()
    };
    let cancel = CancellationToken::new();

    let loaded = assets::load(&client, &store, Some(&artifact), &declared("koth"), &cancel).await.unwrap();
    assert_eq!(loaded, Some(revision));
    assert!(store.contains(&blob(missing).sha256).unwrap());
    let error = assets::load(&client, &store, Some(&artifact), &declared("arena"), &cancel).await.unwrap_err();
    assert!(error.to_string().contains("no world arena for app lobby"), "{error}");
    let renamed = AssetArtifact { revision_id: "0".repeat(64), ..artifact };
    assert!(assets::load(&client, &store, Some(&renamed), &declared("koth"), &cancel).await.is_err());
}
