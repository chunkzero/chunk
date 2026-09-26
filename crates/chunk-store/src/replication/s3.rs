use std::{io, sync::Arc};

use futures_util::TryStreamExt;
use object_store::{
    ObjectStore, PutMode, PutOptions, PutPayload, aws::AmazonS3Builder, path::Path, prefix::PrefixStore,
};

use super::{Listed, ObjectStorage};
use crate::{Error, Result};

/// S3-compatible storage with its own I/O runtime, callable from any thread.
pub(super) struct S3 {
    store: Arc<dyn ObjectStore>,
    runtime: Option<tokio::runtime::Runtime>,
}

impl S3 {
    /// Uses `prefix`, or `CHUNK_REPLICATION_PREFIX` when it is `None`.
    pub fn from_env(prefix: Option<&str>) -> Result<Option<Self>> {
        let variable = |name| std::env::var(name).ok().filter(|value: &String| !value.is_empty());
        let Some(bucket) = variable("CHUNK_REPLICATION_BUCKET") else {
            return Ok(None);
        };
        let (Some(key), Some(secret)) =
            (variable("CHUNK_REPLICATION_ACCESS_KEY_ID"), variable("CHUNK_REPLICATION_SECRET_ACCESS_KEY"))
        else {
            return Err(Error::Invalid("replication requires an access key ID and secret access key"));
        };
        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(bucket)
            .with_region(variable("CHUNK_REPLICATION_REGION").unwrap_or_else(|| "us-east-1".into()))
            .with_access_key_id(key)
            .with_secret_access_key(secret);
        if let Some(endpoint) = variable("CHUNK_REPLICATION_ENDPOINT") {
            builder = builder.with_allow_http(endpoint.starts_with("http://")).with_endpoint(endpoint);
        }
        let bucket = builder.build().map_err(io::Error::other)?;
        let store: Arc<dyn ObjectStore> =
            match prefix.map(str::to_owned).or_else(|| variable("CHUNK_REPLICATION_PREFIX")) {
                Some(prefix) => Arc::new(PrefixStore::new(bucket, prefix.as_str())),
                None => Arc::new(bucket),
            };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("chunk-replication-io")
            .enable_all()
            .build()?;
        Ok(Some(Self { store, runtime: Some(runtime) }))
    }

    /// Waits on a channel instead of `block_on`, so callers may be inside another runtime.
    fn run<T: Send + 'static>(
        &self,
        operation: impl Future<Output = object_store::Result<T>> + Send + 'static,
    ) -> io::Result<T> {
        let runtime = self.runtime.as_ref().ok_or_else(|| io::Error::other("replication runtime stopped"))?;
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        runtime.spawn(async move {
            let _ = sender.send(operation.await);
        });
        receiver.recv().map_err(io::Error::other)?.map_err(io::Error::other)
    }
}

impl ObjectStorage for S3 {
    fn put(&self, key: &str, bytes: Vec<u8>) -> io::Result<()> {
        let (store, key) = (self.store.clone(), Path::from(key));
        self.run(async move { store.put(&key, PutPayload::from(bytes)).await.map(|_| ()) })
    }

    fn create(&self, key: &str, bytes: Vec<u8>) -> io::Result<bool> {
        let (store, key) = (self.store.clone(), Path::from(key));
        self.run(async move {
            let options = PutOptions { mode: PutMode::Create, ..PutOptions::default() };
            match store.put_opts(&key, PutPayload::from(bytes), options).await {
                Ok(_) => Ok(true),
                Err(object_store::Error::AlreadyExists { .. } | object_store::Error::Precondition { .. }) => Ok(false),
                Err(error) => Err(error),
            }
        })
    }

    fn get(&self, key: &str) -> io::Result<Vec<u8>> {
        let (store, key) = (self.store.clone(), Path::from(key));
        self.run(async move { store.get(&key).await?.bytes().await }).map(Vec::from)
    }

    fn list(&self, prefix: &str) -> io::Result<Vec<Listed>> {
        let (store, prefix) = (self.store.clone(), Path::from(prefix));
        self.run(async move {
            store
                .list(Some(&prefix))
                .map_ok(|object| Listed {
                    key: object.location.to_string(),
                    size: object.size,
                    modified: object.last_modified.into(),
                })
                .try_collect()
                .await
        })
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        let (store, key) = (self.store.clone(), Path::from(key));
        self.run(async move {
            match store.delete(&key).await {
                Err(object_store::Error::NotFound { .. }) => Ok(()),
                result => result,
            }
        })
    }
}

impl Drop for S3 {
    fn drop(&mut self) {
        // Dropping a runtime blocks, which panics inside async contexts.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}
