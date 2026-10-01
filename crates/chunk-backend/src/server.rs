//! Embeddable backend storage lifecycle.
use crate::Backend;
use chunk_contract::Deployment;
use std::{io, path::PathBuf, time::Duration};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// Published once the backend serves requests; `backend` deploys and releases further versions.
pub struct Ready {
    pub backend: Backend,
    /// The deployment served first, or empty without one.
    pub deployment: String,
    /// How far the log's replication got; unset when replication is off.
    pub replication: Option<chunk_store::ReplicationProgress>,
}

pub struct Config {
    /// The deployment served first. Without one, the backend serves only the deployments it retained.
    pub bundle: Option<PathBuf>,
    pub environment: String,
    /// Selects `[env.<name>.vars]`; unset, deployments read their top-level variables only.
    pub vars: Option<String>,
    /// The secrets actions read until [`Backend::set_secrets`] replaces them.
    pub secrets: crate::Secrets,
    pub state: PathBuf,
    /// Where the log replicates; unset keeps it local only.
    pub replication: Option<chunk_store::Replication>,
}

/// Starts the backend, publishes readiness, and runs until `stop` or until another store fences this one, then stops
/// the backend and flushes replication.
/// # Errors
/// Reports storage and replication errors, including a final flush that did not put the whole log in object storage.
pub async fn run(config: Config, ready: oneshot::Sender<Ready>, stop: CancellationToken) -> io::Result<()> {
    // Cancelled on a fence as well as by the caller, without cancelling the caller's token.
    let stop = stop.child_token();
    let (backend, bundle, replicator) = tokio::task::spawn_blocking(move || -> io::Result<_> {
        std::fs::create_dir_all(&config.state)?;
        let bundle: Option<Deployment> = config.bundle.as_deref().map(chunk_service::read).transpose()?;
        let database = config.state.join("environment.sqlite");
        if !database.exists() {
            chunk_service::private_file(&database)?;
        }
        let (store, replicator) = match config.replication {
            Some(replication) => {
                let (store, replicator) =
                    chunk_store::SqliteStore::open_replicated(database, &config.environment, replication)
                        .map_err(io::Error::other)?;
                (store, Some(replicator))
            }
            None => (chunk_store::SqliteStore::open(database, &config.environment).map_err(io::Error::other)?, None),
        };
        let effects = crate::ActionEffects::new(config.environment.clone()).map_err(io::Error::other)?;
        let backend = Backend::with_action_effects(config.environment, Box::new(store), effects.with_vars(config.vars))
            .map_err(io::Error::other)?;
        backend.set_secrets(config.secrets);
        Ok((backend, bundle, replicator))
    })
    .await
    .map_err(io::Error::other)??;
    let result = async {
        let deployment = bundle.as_ref().map(|bundle| bundle.id.clone()).unwrap_or_default();
        if let Some(bundle) = bundle {
            backend.deploy(bundle).await.map_err(io::Error::other)?;
        }
        let replication = replicator.as_ref().map(chunk_store::Replicator::progress);
        let _ = ready.send(Ready { backend: backend.clone(), deployment, replication });
        tracing::info!("backend ready");
        stopped(&stop, replicator.as_ref()).await;
        Ok(())
    }
    .await;
    let flushed = tokio::task::spawn_blocking(move || {
        // Embedders may still hold handles from `Ready`; nothing commits after this.
        backend.stop();
        replicator.as_ref().map_or(Ok(()), chunk_store::Replicator::flush)
    })
    .await
    .map_err(io::Error::other)?;
    if let Err(error) = &flushed {
        tracing::error!(%error, "final log flush failed; object storage misses the latest commits");
    }
    result.and(flushed.map_err(io::Error::other))
}

/// Resolves once `stop` is cancelled, cancelling it when another store claims a
/// newer epoch of this environment.
async fn stopped(stop: &CancellationToken, replicator: Option<&chunk_store::Replicator>) {
    let fenced = async {
        let Some(replicator) = replicator else {
            return std::future::pending().await;
        };
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        while !replicator.fenced() {
            interval.tick().await;
        }
        tracing::error!("another store took over this environment; stopping");
    };
    tokio::select! {
        () = stop.cancelled() => {}
        () = fenced => stop.cancel(),
    }
}
