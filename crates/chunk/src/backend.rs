use std::{
    io::{self, Write},
    net::SocketAddr,
    path::PathBuf,
    time::Duration,
};

use chunk_contract::{BackendConnection, Deployment};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;

#[derive(clap::Args)]
pub(crate) struct Options {
    #[arg(long)]
    bundle: PathBuf,
    #[arg(long, default_value = "local")]
    environment: String,
    #[arg(long, default_value = ".chunk/backend")]
    state: PathBuf,
    #[arg(long, default_value = ".chunk/backend.json")]
    connection: PathBuf,
    #[arg(long, default_value = "127.0.0.1:25568")]
    bind: SocketAddr,
}

pub(crate) async fn run(options: Options) -> io::Result<()> {
    if !options.bind.ip().is_loopback() {
        return Err(io::Error::other("backend must bind loopback"));
    }
    std::fs::create_dir_all(&options.state)?;
    let bundle: Deployment = serde_json::from_slice(&std::fs::read(options.bundle)?).map_err(io::Error::other)?;
    let database = options.state.join("environment.sqlite");
    if !database.exists() {
        super::control::private_file(&database)?;
    }
    let store = chunk_store::SqliteStore::open(database, &options.environment).map_err(io::Error::other)?;
    let backend =
        chunk_backend::Backend::new(options.environment.clone(), Box::new(store)).map_err(io::Error::other)?;
    let deployment = bundle.id.clone();
    backend.register(bundle).map_err(io::Error::other)?;
    let token = super::control::secret(&options.state.join("token"))?;
    let listener = TcpListener::bind(options.bind).await?;
    let connection = BackendConnection {
        endpoint: format!("http://{}", listener.local_addr()?),
        token: token.clone(),
        environment: options.environment,
        deployment,
    };
    if let Ok(bytes) = std::fs::read(&options.connection) {
        let old: BackendConnection = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if old.token != token {
            return Err(io::Error::other("connection file belongs to another backend"));
        }
    }
    let shutdown = CancellationToken::new();
    let stop_server = shutdown.clone();
    let service = chunk_backend::Service::new(backend, &token).map_err(io::Error::other)?;
    let mut server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service.into_server())
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), stop_server.cancelled_owned())
            .await
    });
    let bytes = serde_json::to_vec(&connection).map_err(io::Error::other)?;
    let result = async {
        if let Some(parent) = options.connection.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = options
            .connection
            .with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        super::control::private_file(&temporary)?.write_all(&bytes)?;
        std::fs::rename(temporary, &options.connection)?;
        tracing::info!(endpoint = %connection.endpoint, "environment backend ready");
        tokio::select! {
            result = crate::shutdown_signal()? => result,
            _ = &mut server => Err(io::Error::other("backend server stopped")),
        }
    }
    .await;
    shutdown.cancel();
    if !server.is_finished() && tokio::time::timeout(Duration::from_secs(3), &mut server).await.is_err() {
        server.abort();
        let _ = server.await;
    }
    if std::fs::read(&options.connection).ok().as_deref() == Some(&bytes) {
        std::fs::remove_file(options.connection)?;
    }
    result
}
