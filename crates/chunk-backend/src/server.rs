//! Embeddable backend transport and storage lifecycle.
use crate::{Backend, Service};
use chunk_contract::{BackendConnection, Deployment};
use std::{io, net::SocketAddr, path::PathBuf, time::Duration};
use tokio::{net::TcpListener, sync::oneshot};
use tokio_stream::{StreamExt, wrappers::TcpListenerStream};
use tokio_util::sync::CancellationToken;

pub struct Config {
    pub bundle: PathBuf,
    pub environment: String,
    pub state: PathBuf,
    pub connection: PathBuf,
    pub bind: SocketAddr,
}

/// Starts the backend, publishes readiness and drains its workers on shutdown.
/// # Errors
/// Reports configuration, storage, bind and server errors.
pub async fn run(config: Config, ready: oneshot::Sender<BackendConnection>, stop: CancellationToken) -> io::Result<()> {
    if !config.bind.ip().is_loopback() {
        return Err(io::Error::other("backend must bind loopback"));
    }
    let listener = TcpListener::bind(config.bind).await?;
    let address = listener.local_addr()?;
    let connection_path = config.connection.clone();
    let (backend, bundle, token) = tokio::task::spawn_blocking(move || -> io::Result<_> {
        std::fs::create_dir_all(&config.state)?;
        let bundle: Deployment = chunk_service::read(&config.bundle)?;
        let database = config.state.join("environment.sqlite");
        if !database.exists() {
            chunk_service::private_file(&database)?;
        }
        let token = chunk_service::secret(&config.state.join("token"))?;
        let store = chunk_store::SqliteStore::open(database, &config.environment).map_err(io::Error::other)?;
        let backend = Backend::new(config.environment, Box::new(store)).map_err(io::Error::other)?;
        Ok((backend, bundle, token))
    })
    .await
    .map_err(io::Error::other)??;
    let result = async {
        let deployment = bundle.id.clone();
        backend.deploy(bundle).await.map_err(io::Error::other)?;
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
        let incoming = TcpListenerStream::new(listener).map(|stream| {
            let stream = stream?;
            stream.set_nodelay(true)?;
            Ok::<_, io::Error>(stream)
        });
        let server = tonic::transport::Server::builder()
            .add_service(service.into_server())
            .serve_with_incoming_shutdown(incoming, stop.clone().cancelled_owned());
        let _ = ready.send(connection);
        tracing::info!(%address, "backend ready");
        tokio::pin!(server);
        let result = tokio::select! {
            result = &mut server => result.map_err(io::Error::other),
            () = stop.cancelled() => {
                shutdown.cancel();
                match tokio::time::timeout(Duration::from_secs(5), &mut server).await {
                    Ok(result) => result.map_err(io::Error::other),
                    Err(_) => Err(io::Error::other("backend transport shutdown timed out")),
                }
            }
        };
        shutdown.cancel();
        workers.close();
        workers.wait().await;
        drop(record);
        result
    }
    .await;
    tokio::task::spawn_blocking(move || drop(backend)).await.map_err(io::Error::other)?;
    result
}
