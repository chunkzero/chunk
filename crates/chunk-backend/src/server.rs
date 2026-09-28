//! Embeddable backend transport and storage lifecycle.
use crate::{Backend, Service};
use chunk_contract::{BackendConnection, Deployment};
use std::{io, net::SocketAddr, path::PathBuf, time::Duration};
use tokio::{net::TcpListener, sync::oneshot};
use tokio_stream::{StreamExt, wrappers::TcpListenerStream};
use tokio_util::sync::CancellationToken;

/// Published once the backend serves requests; `backend` deploys and releases further versions.
pub struct Ready {
    pub connection: BackendConnection,
    pub backend: Backend,
}

pub struct Config {
    /// The deployment served first. Without one, the backend serves only the deployments it retained.
    pub bundle: Option<PathBuf>,
    pub environment: String,
    pub state: PathBuf,
    pub connection: PathBuf,
    pub bind: SocketAddr,
}

/// Starts the backend, publishes readiness and drains its workers on shutdown.
/// # Errors
/// Reports configuration, storage, bind and server errors.
pub async fn run(config: Config, ready: oneshot::Sender<Ready>, stop: CancellationToken) -> io::Result<()> {
    if !config.bind.ip().is_loopback() {
        return Err(io::Error::other("backend must bind loopback"));
    }
    // Cancelled on a fence as well as by the caller, without cancelling the caller's token.
    let stop = stop.child_token();
    let listener = TcpListener::bind(config.bind).await?;
    let address = listener.local_addr()?;
    let connection_path = config.connection.clone();
    let (backend, bundle, token, replicator) = tokio::task::spawn_blocking(move || -> io::Result<_> {
        std::fs::create_dir_all(&config.state)?;
        let bundle: Option<Deployment> = config.bundle.as_deref().map(chunk_service::read).transpose()?;
        let database = config.state.join("environment.sqlite");
        if !database.exists() {
            chunk_service::private_file(&database)?;
        }
        let token = chunk_service::secret(&config.state.join("token"))?;
        let (store, replicator) = match chunk_store::Replication::from_env().map_err(io::Error::other)? {
            Some(replication) => {
                let (store, replicator) =
                    chunk_store::SqliteStore::open_replicated(database, &config.environment, replication)
                        .map_err(io::Error::other)?;
                (store, Some(replicator))
            }
            None => (chunk_store::SqliteStore::open(database, &config.environment).map_err(io::Error::other)?, None),
        };
        let backend = Backend::new(config.environment, Box::new(store)).map_err(io::Error::other)?;
        Ok((backend, bundle, token, replicator))
    })
    .await
    .map_err(io::Error::other)??;
    let result = async {
        let deployment = bundle.as_ref().map(|bundle| bundle.id.clone()).unwrap_or_default();
        if let Some(bundle) = bundle {
            backend.deploy(bundle).await.map_err(io::Error::other)?;
        }
        let connection = BackendConnection {
            endpoint: format!("http://{address}"),
            token: token.clone(),
            environment: backend.environment().into(),
            deployment,
        };
        if connection_path.exists() {
            let old: BackendConnection = chunk_service::read(&connection_path)?;
            if old.token != token {
                return Err(io::Error::other("connection file belongs to another backend"));
            }
        }
        let service = Service::new(backend.clone(), &token).map_err(io::Error::other)?;
        let workers = service.workers();
        let shutdown = service.shutdown();
        let record = chunk_service::Record::publish(&connection_path, &connection)?;
        let connections = chunk_service::Connections::default();
        let incoming = TcpListenerStream::new(listener).map(|stream| {
            let stream = stream?;
            stream.set_nodelay(true)?;
            Ok::<_, io::Error>(connections.track(stream))
        });
        let server = tonic::transport::Server::builder()
            .add_service(service.into_server())
            .serve_with_incoming_shutdown(incoming, stop.clone().cancelled_owned());
        let _ = ready.send(Ready { connection, backend: backend.clone() });
        tracing::info!(%address, "backend ready");
        tokio::pin!(server);
        let result = tokio::select! {
            result = &mut server => result.map_err(io::Error::other),
            () = stopped(&stop, replicator.as_ref()) => {
                shutdown.cancel();
                connections.drain("backend", &mut server).await.map_err(io::Error::other)
            }
        };
        shutdown.cancel();
        workers.close();
        workers.wait().await;
        drop(record);
        result
    }
    .await;
    let flushed = tokio::task::spawn_blocking(move || {
        // Embedders may still hold handles from `Ready`; nothing commits after this.
        backend.stop();
        replicator.as_ref().map_or(Ok(()), chunk_store::Replicator::flush)
    })
    .await
    .map_err(io::Error::other)?;
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
