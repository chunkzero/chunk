//! Loading the asset revision a deployment pins before it activates.

use super::*;
use crate::managed::{LoadDone, assets};
use chunk_build::assets::Store;
use chunk_contract::{AssetBlob, AssetContract, AssetRevision};
use chunk_management::v1::AssetArtifact;

#[tokio::test]
async fn a_revision_and_its_missing_blobs_load_unless_it_lacks_a_declared_world() {
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
    let done = Arc::<LoadDone>::default();

    let loaded = assets::load(&client, &store, Some(&artifact), &declared("koth"), &done, &cancel).await.unwrap();
    assert!(store.contains(&blob(missing).sha256).unwrap());
    assert_eq!(store.read_revision(&revision.id()).unwrap(), revision);
    assert_eq!(loaded, Some(revision));
    let error = assets::load(&client, &store, Some(&artifact), &declared("arena"), &done, &cancel).await.unwrap_err();
    assert!(error.to_string().contains("no world arena for app lobby"), "{error}");
    let renamed = AssetArtifact { revision_id: "0".repeat(64), ..artifact };
    assert!(assets::load(&client, &store, Some(&renamed), &declared("koth"), &done, &cancel).await.is_err());
}

#[tokio::test]
async fn pruning_keeps_everything_until_the_store_holds_every_kept_revision() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(directory.path());
    let blob = |bytes: &[u8]| AssetBlob { sha256: format!("{:x}", Sha256::digest(bytes)), size: bytes.len() as u64 };
    let mut old = AssetRevision { version: chunk_contract::ASSET_REVISION_VERSION, ..AssetRevision::default() };
    old.shared.insert("old.txt".into(), blob(b"old"));
    store.insert(&blob(b"old").sha256, b"old").unwrap();
    store.write_revision(&old).unwrap();
    let loading = AssetRevision { version: chunk_contract::ASSET_REVISION_VERSION, ..AssetRevision::default() };

    assert_eq!(assets::prune(&store, BTreeSet::from([loading.id()])).await.unwrap(), None);
    assert!(store.contains(&blob(b"old").sha256).unwrap());
    store.write_revision(&loading).unwrap();
    assert_eq!(assets::prune(&store, BTreeSet::from([loading.id()])).await.unwrap(), Some(2));
    assert!(!store.contains(&blob(b"old").sha256).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tick_after_a_failed_load_prunes_the_blobs_it_left_behind() {
    let harness = Harness::new().await;
    let core = crate::Core::start(harness.core(), || {}).await.unwrap();
    let (gateway, lease) = (std::sync::OnceLock::new(), watch::Sender::new(crate::managed::Lease::Waiting));
    let managed = crate::managed::Managed::new(
        &harness.management_config(),
        lease,
        crate::managed::Registration::local("env_test"),
        &harness.state(),
        &core,
        &gateway,
        None,
    );
    let store = core.asset_store();
    let blob = |bytes: &[u8]| format!("{:x}", Sha256::digest(bytes));

    store.insert(&blob(b"first"), b"first").unwrap();
    managed.remove_unused_assets().await;
    assert!(!store.contains(&blob(b"first")).unwrap());

    // A load that fails after downloading leaves its blob behind, and the kept set stays the same.
    store.insert(&blob(b"second"), b"second").unwrap();
    drop(managed.begin_asset_load().await);
    managed.remove_unused_assets().await;
    assert!(!store.contains(&blob(b"second")).unwrap());
}
