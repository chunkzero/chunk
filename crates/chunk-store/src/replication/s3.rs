use std::{
    fmt, io,
    sync::{Arc, PoisonError, RwLock},
};

use futures_util::TryStreamExt;
use object_store::{
    CredentialProvider, ObjectStore, PutMode, PutOptions, PutPayload,
    aws::{AmazonS3Builder, AwsCredential},
    path::Path,
    prefix::PrefixStore,
};

use super::{Listed, ObjectStorage};
use crate::{Error, Result};

/// Where an S3-compatible bucket keeps the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S3Bucket {
    pub name: String,
    pub region: String,
    /// The endpoint URL, for stores other than AWS; an `http://` URL allows plain HTTP.
    pub endpoint: Option<String>,
    /// A key prefix within the bucket.
    pub prefix: Option<String>,
}

impl S3Bucket {
    /// Reads `CHUNK_REPLICATION_BUCKET`, or `None` when it is unset, with the optional `CHUNK_REPLICATION_ENDPOINT`,
    /// `CHUNK_REPLICATION_REGION` (default `us-east-1`) and `CHUNK_REPLICATION_PREFIX`.
    pub(super) fn from_env() -> Option<Self> {
        Some(Self {
            name: variable("CHUNK_REPLICATION_BUCKET")?,
            region: variable("CHUNK_REPLICATION_REGION").unwrap_or_else(|| "us-east-1".into()),
            endpoint: variable("CHUNK_REPLICATION_ENDPOINT"),
            prefix: variable("CHUNK_REPLICATION_PREFIX"),
        })
    }
}

/// Keys that sign a bucket's requests. Clones share them, so [`Self::replace`] reaches every store that signs with
/// them from its next request on.
#[derive(Clone)]
pub struct S3Credentials(Arc<RwLock<Arc<AwsCredential>>>);

impl S3Credentials {
    /// `session_token` is set for temporary credentials.
    #[must_use]
    pub fn new(access_key_id: String, secret_access_key: String, session_token: Option<String>) -> Self {
        let keys = AwsCredential { key_id: access_key_id, secret_key: secret_access_key, token: session_token };
        Self(Arc::new(RwLock::new(Arc::new(keys))))
    }

    /// Signs later requests with `with`'s keys.
    pub fn replace(&self, with: &Self) {
        let keys = with.keys();
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = keys;
    }

    fn keys(&self) -> Arc<AwsCredential> {
        self.0.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub(super) fn from_env() -> Result<Self> {
        let (Some(key), Some(secret)) =
            (variable("CHUNK_REPLICATION_ACCESS_KEY_ID"), variable("CHUNK_REPLICATION_SECRET_ACCESS_KEY"))
        else {
            return Err(Error::Invalid("replication requires an access key ID and secret access key"));
        };
        Ok(Self::new(key, secret, None))
    }
}

impl fmt::Debug for S3Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("S3Credentials(..)")
    }
}

#[async_trait::async_trait]
impl CredentialProvider for S3Credentials {
    type Credential = AwsCredential;

    async fn get_credential(&self) -> object_store::Result<Arc<AwsCredential>> {
        Ok(self.keys())
    }
}

fn variable(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// S3-compatible storage with its own I/O runtime, callable from any thread.
pub(super) struct S3 {
    store: Arc<dyn ObjectStore>,
    runtime: Option<tokio::runtime::Runtime>,
}

impl S3 {
    /// Uses `prefix`, or `CHUNK_REPLICATION_PREFIX` when it is `None`.
    pub fn from_env(prefix: Option<&str>) -> Result<Option<Self>> {
        let Some(mut bucket) = S3Bucket::from_env() else {
            return Ok(None);
        };
        if let Some(prefix) = prefix {
            bucket.prefix = Some(prefix.into());
        }
        Self::new(&bucket, S3Credentials::from_env()?).map(Some)
    }

    pub fn new(bucket: &S3Bucket, credentials: S3Credentials) -> Result<Self> {
        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(&bucket.name)
            .with_region(&bucket.region)
            .with_credentials(Arc::new(credentials));
        if let Some(endpoint) = &bucket.endpoint {
            builder = builder.with_allow_http(endpoint.starts_with("http://")).with_endpoint(endpoint);
        }
        let store = builder.build().map_err(io::Error::other)?;
        let store: Arc<dyn ObjectStore> = match &bucket.prefix {
            Some(prefix) => Arc::new(PrefixStore::new(store, prefix.as_str())),
            None => Arc::new(store),
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("chunk-replication-io")
            .enable_all()
            .build()?;
        Ok(Self { store, runtime: Some(runtime) })
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
