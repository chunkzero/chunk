//! Replicating core's log to the object storage management grants in each attach.

use super::{ATTACH_IDLE, Interrupted, REATTACH, REQUEST_TIMEOUT, check, deadline};
use chunk_management::{Client, Stream, v1};
use chunk_store::{ForkSource, Replication, S3Bucket, S3Credentials};
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

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

    #[cfg(test)]
    pub(crate) fn credentials(&self) -> Option<&S3Credentials> {
        self.opened.as_ref().map(|(_, credentials)| credentials)
    }
}

/// What renews a log store's credentials while no core attach does: before core opens its log and attaches as core,
/// and from when that attach ends until the final flush. These attaches take no lease.
#[derive(Clone)]
pub(crate) struct Renewer {
    pub(super) client: Client,
    pub(super) instance_id: String,
    pub(super) environment: String,
    pub(super) log_store: Arc<LogStore>,
}

impl Renewer {
    /// Renews from `attached`, an attach that takes no lease, then from new ones, until the renewal stops. `None` when
    /// management granted no log store.
    pub(crate) fn start(self, attached: Option<Stream<v1::AttachResponse>>) -> Option<Renewal> {
        self.log_store.opened.as_ref()?;
        let stop = CancellationToken::new();
        let task = tokio::spawn(self.keep_renewing(attached, stop.clone()));
        Some(Renewal { stop, task })
    }

    async fn keep_renewing(self, mut attached: Option<Stream<v1::AttachResponse>>, stop: CancellationToken) {
        loop {
            let error = tokio::select! {
                () = stop.cancelled() => return,
                renewed = self.renew_from(attached.take()) => match renewed {
                    Ok(()) => io::Error::other("management ended the attach"),
                    Err(Interrupted::Fatal(error)) => {
                        tracing::error!(%error, "log store credentials no longer renew");
                        return;
                    }
                    Err(Interrupted::Retry(error) | Interrupted::Fenced(error)) => error,
                },
            };
            tracing::warn!(%error, "management attach interrupted; log store credentials renew once it answers again");
            tokio::select! {
                () = stop.cancelled() => return,
                () = tokio::time::sleep(REATTACH) => {}
            }
        }
    }

    /// Renews from each desired state of `attached`, or else of a new attach, until the stream ends.
    async fn renew_from(&self, attached: Option<Stream<v1::AttachResponse>>) -> Result<(), Interrupted> {
        let mut stream = match attached {
            Some(stream) => stream,
            None => deadline(REQUEST_TIMEOUT, self.client.attach(&request(&self.instance_id))).await?,
        };
        while let Some(desired) = deadline(ATTACH_IDLE, stream.message()).await? {
            check(&desired, &self.environment)?;
            self.log_store.renew(desired.log_store.as_ref());
        }
        Ok(())
    }
}

/// An attach that takes no lease, for this run of core.
pub(super) fn request(instance_id: &str) -> v1::AttachRequest {
    v1::AttachRequest {
        instance_id: instance_id.into(),
        version: env!("CARGO_PKG_VERSION").into(),
        core: false,
        epoch: 0,
    }
}

/// Runs `work` while `renewer`, if any, renews, and returns once that renewal stopped.
pub(crate) async fn renewing<T>(renewer: Option<Renewer>, work: impl Future<Output = T>) -> T {
    let renewal = renewer.and_then(|renewer| renewer.start(None));
    let output = work.await;
    if let Some(renewal) = renewal {
        renewal.stop().await;
    }
    output
}

/// A running [`Renewer`]. Dropping it stops renewal soon; [`Self::stop`] waits until it has.
pub(crate) struct Renewal {
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl Renewal {
    /// Returns once no more renewals come from here.
    pub(crate) async fn stop(mut self) {
        self.stop.cancel();
        _ = (&mut self.task).await;
    }
}

impl Drop for Renewal {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// The log a fork's core starts from, as management's `restore` grants it to read.
/// # Errors
/// Rejects a restore without a source, an invalid grant or snapshot ID.
pub(crate) fn fork_source(restore: &v1::Restore) -> io::Result<ForkSource> {
    let grant = restore.source.as_ref().ok_or_else(|| io::Error::other("the restore names no source log"))?;
    let replication = Replication::s3(&bucket(grant), credentials(grant)).map_err(io::Error::other)?;
    let snapshot = Some(restore.snapshot_id.as_str()).filter(|id| !id.is_empty()).map(str::parse).transpose();
    Ok(ForkSource {
        replication,
        environment: restore.source_environment_id.clone(),
        snapshot: snapshot.map_err(io::Error::other)?,
    })
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
