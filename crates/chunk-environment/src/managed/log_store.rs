//! Replicating core's log to the object storage management grants in each attach.

use chunk_management::v1;
use chunk_store::{Replication, S3Bucket, S3Credentials};
use std::{
    io,
    sync::atomic::{AtomicBool, Ordering},
};

/// Where core's log replicates as management granted it when core started, with the credentials later attaches renew.
pub(crate) struct LogStore {
    opened: Option<(S3Bucket, S3Credentials)>,
    /// Whether management's latest grant names storage other than `opened`.
    moved: AtomicBool,
}

impl LogStore {
    /// Replication to `grant`, unless management granted none.
    /// # Errors
    /// Rejects an invalid grant.
    pub(crate) fn open(grant: Option<&v1::ObjectStore>) -> io::Result<(Self, Option<Replication>)> {
        let Some(grant) = grant else {
            return Ok((Self { opened: None, moved: AtomicBool::new(false) }, None));
        };
        let (bucket, credentials) = (bucket(grant), credentials(grant));
        let replication = Replication::s3(&bucket, credentials.clone()).map_err(io::Error::other)?;
        tracing::info!(
            bucket = bucket.name,
            prefix = bucket.prefix,
            "replicating the log to management's object storage"
        );
        Ok((Self { opened: Some((bucket, credentials)), moved: AtomicBool::new(false) }, Some(replication)))
    }

    /// Signs later requests with `grant`'s credentials when it names the storage core replicates to. Storage granted
    /// anywhere else is used only once core starts again.
    pub(crate) fn renew(&self, grant: Option<&v1::ObjectStore>) {
        let same = match (&self.opened, grant) {
            (Some((opened, current)), Some(grant)) if *opened == bucket(grant) => {
                current.replace(&credentials(grant));
                true
            }
            (opened, grant) => opened.is_none() && grant.is_none(),
        };
        if !self.moved.swap(!same, Ordering::Relaxed) && !same {
            tracing::warn!("management moved the log's object storage; core keeps its own until it starts again");
        }
    }
}

fn bucket(grant: &v1::ObjectStore) -> S3Bucket {
    let set = |value: &str| Some(value.to_owned()).filter(|value| !value.is_empty());
    S3Bucket {
        name: grant.bucket.clone(),
        region: set(&grant.region).unwrap_or_else(|| "us-east-1".into()),
        endpoint: set(&grant.endpoint),
        prefix: set(&grant.prefix),
    }
}

fn credentials(grant: &v1::ObjectStore) -> S3Credentials {
    let session_token = Some(grant.session_token.clone()).filter(|token| !token.is_empty());
    S3Credentials::new(grant.access_key_id.clone(), grant.secret_access_key.clone(), session_token)
}
