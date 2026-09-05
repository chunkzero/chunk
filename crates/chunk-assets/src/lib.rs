//! Content-addressed deployment objects. Authorization and manifests belong to the backend.
//!
//! This foundation accepts buffered objects. Streaming publication, directory manifests,
//! transfer endpoints, and retention are not implemented yet.

use std::sync::Arc;

use bytes::Bytes;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, path::Path};
use sha2::{Digest, Sha256};

#[cfg(feature = "s3")]
pub use object_store::aws::AmazonS3Builder;
pub use object_store::local::LocalFileSystem;

/// An object identifier derived from its bytes, not a user-supplied filesystem path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectId(String);

impl ObjectId {
    #[must_use]
    pub fn digest(bytes: &[u8]) -> Self {
        Self(format!("{:x}", Sha256::digest(bytes)))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn path(&self) -> Path {
        Path::from(format!("sha256/{}", self.0))
    }
}

/// An object store scoped by the caller to one application's storage namespace.
pub struct Assets {
    store: Arc<dyn ObjectStore>,
}

impl Assets {
    #[must_use]
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self { store }
    }

    /// Publish an immutable object, reusing an existing identical revision.
    ///
    /// # Errors
    /// Returns storage errors other than an already published content ID.
    pub async fn publish(&self, bytes: Bytes) -> object_store::Result<ObjectId> {
        let id = ObjectId::digest(&bytes);
        let options = PutOptions {
            mode: PutMode::Create,
            ..Default::default()
        };
        match self.store.put_opts(&id.path(), bytes.into(), options).await {
            Ok(_) | Err(object_store::Error::AlreadyExists { .. }) => Ok(id),
            Err(error) => Err(error),
        }
    }

    /// Read an object and verify that its content matches the requested ID.
    ///
    /// # Errors
    /// Returns storage errors or an integrity error for changed object contents.
    pub async fn read(&self, id: &ObjectId) -> object_store::Result<Bytes> {
        let bytes = self.store.get(&id.path()).await?.bytes().await?;
        if ObjectId::digest(&bytes) != *id {
            return Err(object_store::Error::Generic {
                store: "chunk-assets",
                source: std::io::Error::new(std::io::ErrorKind::InvalidData, "asset digest mismatch").into(),
            });
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_objects_survive_reopening_and_duplicate_publication() {
        let dir = tempfile::tempdir().unwrap();
        let assets = Assets::new(Arc::new(LocalFileSystem::new_with_prefix(dir.path()).unwrap()));
        let data = Bytes::from_static(b"map revision");
        let first = assets.publish(data.clone()).await.unwrap();
        assert_eq!(first, assets.publish(data.clone()).await.unwrap());
        drop(assets);
        let reopened = Assets::new(Arc::new(LocalFileSystem::new_with_prefix(dir.path()).unwrap()));
        assert_eq!(reopened.read(&first).await.unwrap(), data);
    }

    #[tokio::test]
    async fn corrupted_objects_are_rejected() {
        let store = Arc::new(object_store::memory::InMemory::new());
        let assets = Assets::new(store.clone());
        let id = assets.publish(Bytes::from_static(b"original")).await.unwrap();
        store
            .put(&id.path(), Bytes::from_static(b"changed").into())
            .await
            .unwrap();
        assert!(matches!(
            assets.read(&id).await,
            Err(object_store::Error::Generic { .. })
        ));
    }
}
